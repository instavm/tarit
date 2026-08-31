use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::io::{BufReader, Read};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use tarit_store::{ImageRecord, Store, StoreError};
use tarit_types::{CreateVmRequest, OrchError, VmStatus};

use crate::config::{expand_path, load_warm_pool_config, Config, WarmPoolConfig};

pub const DEFAULT_IMAGE_TAG: &str = "latest";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageRef {
    pub name: String,
    pub tag: String,
}

impl ImageRef {
    pub fn label(&self) -> String {
        format!("{}:{}", self.name, self.tag)
    }
}

pub fn parse_image_ref(raw: &str) -> Result<ImageRef> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("image reference must not be empty");
    }
    let mut parts = trimmed.split(':');
    let name = parts.next().unwrap_or_default();
    let tag = parts.next().unwrap_or(DEFAULT_IMAGE_TAG);
    if parts.next().is_some() {
        bail!("image reference must be name[:tag]");
    }
    validate_image_name(name)?;
    validate_image_tag(tag)?;
    Ok(ImageRef {
        name: name.to_string(),
        tag: tag.to_string(),
    })
}

pub struct LocalImageConfig {
    pub vmm_bin: PathBuf,
    pub vmm_agent: PathBuf,
    pub db_path: PathBuf,
    pub images_dir: PathBuf,
    pub admission_policy: ImageAdmissionPolicy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageAdmissionPolicy {
    pub require_signature: bool,
    pub cosign_key: Option<PathBuf>,
}

impl ImageAdmissionPolicy {
    pub fn from_env() -> Result<Self> {
        let require_signature = match env::var("TARIT_IMAGE_REQUIRE_SIGNATURE") {
            Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                _ => bail!("TARIT_IMAGE_REQUIRE_SIGNATURE must be a boolean"),
            },
            Err(_) => false,
        };
        let cosign_key = env::var("TARIT_IMAGE_COSIGN_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| expand_path(&value));
        if require_signature && cosign_key.is_none() {
            bail!("TARIT_IMAGE_REQUIRE_SIGNATURE requires TARIT_IMAGE_COSIGN_KEY");
        }
        Ok(Self {
            require_signature,
            cosign_key,
        })
    }

    pub(crate) fn trusted_key_digest(&self) -> Result<Option<String>> {
        self.cosign_key
            .as_deref()
            .map(sha256_regular_file)
            .transpose()
    }
}

/// Find the guest agent next to either a source build or an installed Tarit
/// prefix. Packaged installations place it in `libexec/tarit`, because it runs
/// in guests and is not a host command.
fn default_vmm_agent(vmm_bin: &Path) -> PathBuf {
    let source_build = vmm_bin
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|root| root.join("guest/agent/vmm-agent"));
    if let Some(path) = source_build.as_ref().filter(|path| path.is_file()) {
        return path.clone();
    }

    let installed = vmm_bin
        .parent()
        .and_then(|bin| bin.parent())
        .map(|prefix| prefix.join("libexec/tarit/vmm-agent"));
    if let Some(path) = installed.as_ref().filter(|path| path.is_file()) {
        return path.clone();
    }

    [
        PathBuf::from("/usr/local/libexec/tarit/vmm-agent"),
        PathBuf::from("/usr/libexec/tarit/vmm-agent"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .or(installed)
    .or(source_build)
    .unwrap_or_else(|| PathBuf::from("guest/agent/vmm-agent"))
}

impl LocalImageConfig {
    pub fn from_env() -> Result<Self> {
        let vmm_bin = expand_path(&env::var("TARIT_VMM_BIN").unwrap_or_else(|_| "vmm".into()));
        let vmm_agent = env::var("TARIT_VMM_AGENT")
            .map(|s| expand_path(&s))
            .unwrap_or_else(|_| default_vmm_agent(&vmm_bin));
        Ok(Self {
            vmm_bin,
            vmm_agent,
            db_path: expand_path(
                &env::var("TARIT_DB").unwrap_or_else(|_| "~/.taritd/fleet.db".into()),
            ),
            images_dir: expand_path(
                &env::var("TARIT_IMAGES_DIR").unwrap_or_else(|_| "~/.taritd/images".into()),
            ),
            admission_policy: ImageAdmissionPolicy::from_env()?,
        })
    }
}

pub struct BuildImageOptions {
    pub oci_ref: String,
    pub size_mib: u64,
    pub image_ref: ImageRef,
    pub vmm_bin: PathBuf,
    pub vmm_agent: PathBuf,
    pub db_path: PathBuf,
    pub images_dir: PathBuf,
    pub admission_policy: ImageAdmissionPolicy,
}

#[derive(Debug, serde::Deserialize)]
struct SkopeoInspect {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Digest")]
    digest: String,
}

pub fn build_image(opts: BuildImageOptions) -> Result<ImageRecord> {
    std::fs::create_dir_all(&opts.images_dir)
        .with_context(|| format!("create images dir {}", opts.images_dir.display()))?;
    let store = Store::open(&opts.db_path)
        .with_context(|| format!("open store {}", opts.db_path.display()))?;
    if store
        .get_image(&opts.image_ref.name, &opts.image_ref.tag)
        .is_ok()
    {
        bail!(
            "image {} already exists; remove it or use a new tag",
            opts.image_ref.label()
        );
    }

    let final_path = opts.images_dir.join(image_filename(&opts.image_ref));
    if final_path.exists() {
        bail!(
            "image output already exists at {}; remove it or use a new tag",
            final_path.display()
        );
    }

    let temp_path = opts.images_dir.join(format!(
        ".build-{}-{}-{}.ext4",
        sanitize_component(&opts.image_ref.name),
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let result = build_image_inner(&opts, &store, &temp_path, &final_path);
    if result.is_err() {
        remove_file_if_exists(&temp_path);
        remove_file_if_exists(&final_path);
    }
    result
}

fn build_image_inner(
    opts: &BuildImageOptions,
    store: &Store,
    temp_path: &Path,
    final_path: &Path,
) -> Result<ImageRecord> {
    // `vmm pull --output <ext4> --agent <agent-binary> <docker://ref>`.
    // The OCI ref needs a transport scheme (default docker://) and the agent
    // binary is injected as init so app images (e.g. node:20) boot to the exec
    // agent.
    let oci_ref = if opts.oci_ref.contains("://") {
        opts.oci_ref.clone()
    } else {
        format!("docker://{}", opts.oci_ref)
    };
    let resolved = resolve_oci_reference(&oci_ref)?;
    let provenance_key_digest = verify_oci_provenance(&resolved, &opts.admission_policy)?;
    let agent_digest = sha256_regular_file(&opts.vmm_agent)
        .with_context(|| format!("digest guest agent {}", opts.vmm_agent.display()))?;
    let status = Command::new(&opts.vmm_bin)
        .arg("pull")
        .arg("--output")
        .arg(temp_path)
        .arg("--agent")
        .arg(&opts.vmm_agent)
        .arg("--size")
        .arg(opts.size_mib.to_string())
        .arg(&resolved.pinned_ref)
        .status()
        .with_context(|| format!("run {}", opts.vmm_bin.display()))?;
    if !status.success() {
        bail!("vmm pull failed with status {status}");
    }

    let status = Command::new("e2fsck")
        .arg("-fy")
        .arg(temp_path)
        .status()
        .context("run e2fsck -fy")?;
    match status.code() {
        Some(0 | 1) => {}
        _ => bail!("e2fsck -fy failed with status {status}"),
    }

    let rootfs_digest = sha256_regular_file(temp_path)
        .with_context(|| format!("digest admitted rootfs {}", temp_path.display()))?;

    std::fs::rename(temp_path, final_path).with_context(|| {
        format!(
            "move image {} to {}",
            temp_path.display(),
            final_path.display()
        )
    })?;
    #[cfg(unix)]
    std::fs::set_permissions(final_path, std::fs::Permissions::from_mode(0o444))?;
    let size_bytes = std::fs::metadata(final_path)
        .with_context(|| format!("stat {}", final_path.display()))?
        .len();
    let image = ImageRecord {
        name: opts.image_ref.name.clone(),
        tag: opts.image_ref.tag.clone(),
        rootfs_path: final_path.display().to_string(),
        created_at: Utc::now(),
        size_bytes,
        source_ref: resolved.pinned_ref,
        source_digest: Some(resolved.digest),
        rootfs_digest: Some(rootfs_digest),
        agent_digest: Some(agent_digest),
        provenance_key_digest,
        provenance_verified_at: opts
            .admission_policy
            .cosign_key
            .as_ref()
            .map(|_| Utc::now()),
        golden_snapshot_path: None,
    };
    store.upsert_image(&image).context("register image")?;
    Ok(image)
}

pub fn resolve_warm_pool_images(config: &mut Config, store: &Store) -> Result<()> {
    let policy = config.image_admission_policy.clone();
    resolve_warm_pool_image_refs_with_policy(&mut config.warm_pool, store, Some(&policy))
}

pub fn resolve_warm_pool_image_refs(warm_pool: &mut WarmPoolConfig, store: &Store) -> Result<()> {
    resolve_warm_pool_image_refs_with_policy(warm_pool, store, None)
}

fn resolve_warm_pool_image_refs_with_policy(
    warm_pool: &mut WarmPoolConfig,
    store: &Store,
    policy: Option<&ImageAdmissionPolicy>,
) -> Result<()> {
    for class in &mut warm_pool.classes {
        let Some(raw) = class.image.as_deref() else {
            continue;
        };
        let image_ref = parse_image_ref(raw)?;
        let image = store
            .get_image(&image_ref.name, &image_ref.tag)
            .with_context(|| format!("resolve warm-pool image {}", image_ref.label()))?;
        if let Some(policy) = policy {
            verify_admitted_image(&image, policy)
                .with_context(|| format!("admit warm-pool image {}", image_ref.label()))?;
        }
        class.rootfs = Some(PathBuf::from(image.rootfs_path));
    }
    Ok(())
}

pub fn resolve_request_image(
    store: &Store,
    req: &CreateVmRequest,
    policy: &ImageAdmissionPolicy,
) -> Result<CreateVmRequest, OrchError> {
    if req.image.is_some() && req.rootfs_path.is_some() {
        return Err(OrchError::BadRequest(
            "set either image or rootfs_path, not both".into(),
        ));
    }
    let Some(raw) = req.image.as_deref() else {
        return Ok(req.clone());
    };
    let image_ref =
        parse_image_ref(raw).map_err(|e| OrchError::BadRequest(format!("invalid image: {e}")))?;
    let image = match store.get_image(&image_ref.name, &image_ref.tag) {
        Ok(image) => image,
        Err(StoreError::NotFound) => {
            return Err(OrchError::NotFound(format!(
                "image {} not found",
                image_ref.label()
            )))
        }
        Err(e) => return Err(OrchError::Internal(format!("image lookup: {e}"))),
    };
    verify_admitted_image(&image, policy).map_err(|error| {
        OrchError::Unprocessable(format!(
            "image {} failed immutable admission: {error}",
            image_ref.label()
        ))
    })?;
    let mut resolved = req.clone();
    resolved.rootfs_path = Some(image.rootfs_path);
    resolved.image = None;
    Ok(resolved)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedOciReference {
    pinned_ref: String,
    cosign_ref: String,
    digest: String,
}

fn resolve_oci_reference(reference: &str) -> Result<ResolvedOciReference> {
    let output = Command::new("skopeo")
        .arg("inspect")
        .arg(reference)
        .output()
        .context("run skopeo inspect to resolve immutable OCI digest")?;
    if !output.status.success() {
        bail!(
            "skopeo inspect failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    resolved_reference_from_inspect(&output.stdout)
}

fn resolved_reference_from_inspect(bytes: &[u8]) -> Result<ResolvedOciReference> {
    let inspect: SkopeoInspect =
        serde_json::from_slice(bytes).context("parse skopeo inspect output")?;
    validate_registry_name(&inspect.name)?;
    validate_sha256_digest(&inspect.digest)?;
    let cosign_ref = format!("{}@{}", inspect.name, inspect.digest);
    Ok(ResolvedOciReference {
        pinned_ref: format!("docker://{cosign_ref}"),
        cosign_ref,
        digest: inspect.digest,
    })
}

fn verify_oci_provenance(
    resolved: &ResolvedOciReference,
    policy: &ImageAdmissionPolicy,
) -> Result<Option<String>> {
    let Some(key) = policy.cosign_key.as_deref() else {
        if policy.require_signature {
            bail!("image signature is required but no trusted cosign key is configured");
        }
        return Ok(None);
    };
    let key_digest = sha256_regular_file(key)
        .with_context(|| format!("read trusted cosign key {}", key.display()))?;
    let output = Command::new("cosign")
        .arg("verify")
        .arg("--key")
        .arg(key)
        .arg(&resolved.cosign_ref)
        .output()
        .context("run cosign verify")?;
    if !output.status.success() {
        bail!(
            "cosign verification failed for {}: {}",
            resolved.cosign_ref,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(Some(key_digest))
}

pub(crate) fn verify_admitted_image(
    image: &ImageRecord,
    policy: &ImageAdmissionPolicy,
) -> Result<()> {
    let source_digest = image
        .source_digest
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("legacy record has no resolved source digest"))?;
    validate_sha256_digest(source_digest)?;
    if !image.source_ref.ends_with(source_digest) || !image.source_ref.contains('@') {
        bail!("stored source reference is not pinned to its admitted digest");
    }
    let expected_rootfs = image
        .rootfs_digest
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("legacy record has no rootfs digest"))?;
    validate_sha256_digest(expected_rootfs)?;
    let actual_rootfs = sha256_regular_file(Path::new(&image.rootfs_path))?;
    if actual_rootfs != expected_rootfs {
        bail!("rootfs content digest mismatch");
    }
    let size = std::fs::metadata(&image.rootfs_path)?.len();
    if size != image.size_bytes {
        bail!("rootfs size mismatch");
    }
    let agent_digest = image
        .agent_digest
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("legacy record has no injected-agent digest"))?;
    validate_sha256_digest(agent_digest)?;

    if policy.require_signature {
        let expected_key = policy
            .trusted_key_digest()?
            .ok_or_else(|| anyhow::anyhow!("no trusted provenance key configured"))?;
        if image.provenance_key_digest.as_deref() != Some(expected_key.as_str())
            || image.provenance_verified_at.is_none()
        {
            bail!("image was not verified by the currently trusted provenance key");
        }
    }
    Ok(())
}

fn validate_registry_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('/')
        || name.contains("..")
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':')
        })
    {
        bail!("skopeo returned an unsafe registry image name");
    }
    Ok(())
}

fn validate_sha256_digest(digest: &str) -> Result<()> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        bail!("digest must use sha256");
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid sha256 digest");
    }
    Ok(())
}

pub(crate) fn sha256_regular_file(path: &Path) -> Result<String> {
    let metadata =
        std::fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path)?;
    if !file.metadata()?.file_type().is_file() {
        bail!("{} changed type while opening", path.display());
    }
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

pub fn remove_image(config: &LocalImageConfig, image_ref: &ImageRef) -> Result<ImageRecord> {
    let store = Store::open(&config.db_path)
        .with_context(|| format!("open store {}", config.db_path.display()))?;
    let image = store
        .get_image(&image_ref.name, &image_ref.tag)
        .with_context(|| format!("lookup image {}", image_ref.label()))?;
    let protected = protected_paths(&store)?;
    if protected.contains(&image.rootfs_path) {
        bail!(
            "image {} is referenced by a warm-pool class or active VM",
            image_ref.label()
        );
    }
    let deleted = store
        .delete_image(&image_ref.name, &image_ref.tag)
        .with_context(|| format!("delete image {}", image_ref.label()))?;
    remove_registered_file(&config.images_dir, &deleted.rootfs_path);
    Ok(deleted)
}

pub fn list_images(config: &LocalImageConfig) -> Result<Vec<ImageRecord>> {
    let store = Store::open(&config.db_path)
        .with_context(|| format!("open store {}", config.db_path.display()))?;
    store.list_images().context("list images")
}

pub fn verify_registered_image(
    config: &LocalImageConfig,
    image_ref: &ImageRef,
) -> Result<ImageRecord> {
    let store = Store::open(&config.db_path)
        .with_context(|| format!("open store {}", config.db_path.display()))?;
    let image = store
        .get_image(&image_ref.name, &image_ref.tag)
        .with_context(|| format!("lookup image {}", image_ref.label()))?;
    verify_admitted_image(&image, &config.admission_policy)
        .with_context(|| format!("verify image {}", image_ref.label()))?;
    Ok(image)
}

pub struct GcPlan {
    pub candidates: Vec<ImageRecord>,
}

pub fn gc_images(
    config: &LocalImageConfig,
    older_than_days: u64,
    pattern: Option<&str>,
    dry_run: bool,
) -> Result<GcPlan> {
    let store = Store::open(&config.db_path)
        .with_context(|| format!("open store {}", config.db_path.display()))?;
    let images = store.list_images().context("list images")?;
    let protected = protected_paths(&store)?;
    let candidates = select_gc_candidates(
        &images,
        &protected,
        Utc::now(),
        Duration::days(older_than_days as i64),
        pattern,
    );
    if !dry_run {
        for image in &candidates {
            let _ = store.delete_image(&image.name, &image.tag)?;
            remove_registered_file(&config.images_dir, &image.rootfs_path);
        }
    }
    Ok(GcPlan { candidates })
}

pub fn select_gc_candidates(
    images: &[ImageRecord],
    protected_paths: &HashSet<String>,
    now: DateTime<Utc>,
    min_age: Duration,
    pattern: Option<&str>,
) -> Vec<ImageRecord> {
    images
        .iter()
        .filter(|image| !protected_paths.contains(&image.rootfs_path))
        .filter(|image| now.signed_duration_since(image.created_at) >= min_age)
        .filter(|image| {
            pattern
                .map(|p| wildcard_match(p, &format!("{}:{}", image.name, image.tag)))
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

fn protected_paths(store: &Store) -> Result<HashSet<String>> {
    let mut protected = HashSet::new();
    for vm in store.list_vms().context("list VMs for image GC")? {
        if matches!(
            vm.status,
            VmStatus::Creating | VmStatus::Running | VmStatus::Paused | VmStatus::Suspended
        ) {
            if let Some(rootfs) = vm.rootfs_path {
                protected.insert(rootfs);
            }
        }
    }

    let mut warm_pool = load_warm_pool_config()?;
    for class in &warm_pool.classes {
        if let Some(rootfs) = &class.rootfs {
            protected.insert(rootfs.display().to_string());
        }
    }
    resolve_warm_pool_image_refs(&mut warm_pool, store)?;
    for class in &warm_pool.classes {
        if let Some(rootfs) = &class.rootfs {
            protected.insert(rootfs.display().to_string());
        }
    }
    Ok(protected)
}

fn image_filename(image_ref: &ImageRef) -> String {
    format!(
        "{}__{}.ext4",
        sanitize_component(&image_ref.name),
        sanitize_component(&image_ref.tag)
    )
}

fn validate_image_name(name: &str) -> Result<()> {
    validate_component(name, "image name", true)
}

fn validate_image_tag(tag: &str) -> Result<()> {
    validate_component(tag, "image tag", false)
}

fn validate_component(raw: &str, label: &str, allow_slash: bool) -> Result<()> {
    if raw.is_empty() {
        bail!("{label} must not be empty");
    }
    if raw == "." || raw == ".." || raw.contains("..") {
        bail!("{label} must not contain '..'");
    }
    let ok = raw.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') || (allow_slash && b == b'/')
    });
    if !ok {
        bail!(
            "{label} may only contain ASCII letters, digits, '.', '_', '-'{}",
            if allow_slash { ", or '/'" } else { "" }
        );
    }
    Ok(())
}

fn sanitize_component(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == text;
    }
    let starts_with_star = pattern.starts_with('*');
    let ends_with_star = pattern.ends_with('*');
    let parts = pattern
        .split('*')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return true;
    }

    let mut rest = text;
    for (idx, part) in parts.iter().enumerate() {
        let Some(pos) = rest.find(part) else {
            return false;
        };
        if idx == 0 && !starts_with_star && pos != 0 {
            return false;
        }
        rest = &rest[pos + part.len()..];
    }
    if !ends_with_star {
        if let Some(last) = parts.last() {
            return text.ends_with(last);
        }
    }
    true
}

fn remove_file_if_exists(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), "failed to remove image file: {e}"),
    }
}

fn remove_registered_file(images_dir: &Path, rootfs_path: &str) {
    let path = Path::new(rootfs_path);
    if path.starts_with(images_dir) {
        remove_file_if_exists(path);
    } else {
        tracing::warn!(
            path = %path.display(),
            images_dir = %images_dir.display(),
            "registered image file is outside images dir; leaving file in place"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_agent_is_derived_from_vmm_prefix() {
        assert_eq!(
            default_vmm_agent(Path::new("/opt/tarit/bin/vmm")),
            PathBuf::from("/opt/tarit/libexec/tarit/vmm-agent")
        );
    }

    #[test]
    fn source_build_agent_wins_when_present() {
        let root = std::env::temp_dir().join(format!(
            "tarit-image-agent-path-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let vmm = root.join("vmm/target/release/vmm");
        let agent = root.join("vmm/guest/agent/vmm-agent");
        std::fs::create_dir_all(vmm.parent().unwrap()).unwrap();
        std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
        std::fs::write(&vmm, []).unwrap();
        std::fs::write(&agent, []).unwrap();

        assert_eq!(default_vmm_agent(&vmm), agent);

        std::fs::remove_dir_all(root).unwrap();
    }

    fn image(name: &str, tag: &str, path: &str, created_at: DateTime<Utc>) -> ImageRecord {
        ImageRecord {
            name: name.into(),
            tag: tag.into(),
            rootfs_path: path.into(),
            created_at,
            size_bytes: 1,
            source_ref: format!("{name}:{tag}"),
            source_digest: None,
            rootfs_digest: None,
            agent_digest: None,
            provenance_key_digest: None,
            provenance_verified_at: None,
            golden_snapshot_path: None,
        }
    }

    #[test]
    fn parses_name_tag_with_latest_default() {
        assert_eq!(
            parse_image_ref("node20").unwrap(),
            ImageRef {
                name: "node20".into(),
                tag: "latest".into()
            }
        );
        assert_eq!(
            parse_image_ref("node:20").unwrap(),
            ImageRef {
                name: "node".into(),
                tag: "20".into()
            }
        );
        assert!(parse_image_ref("node:").is_err());
        assert!(parse_image_ref("node:20:extra").is_err());
    }

    #[test]
    fn skopeo_result_is_pinned_to_one_valid_manifest_digest() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let json = serde_json::json!({
            "Name": "docker.io/library/ubuntu",
            "Digest": digest,
        });
        let resolved = resolved_reference_from_inspect(json.to_string().as_bytes()).unwrap();
        assert_eq!(
            resolved.pinned_ref,
            format!("docker://docker.io/library/ubuntu@{digest}")
        );
        assert_eq!(
            resolved.cosign_ref,
            format!("docker.io/library/ubuntu@{digest}")
        );

        for bad in [
            serde_json::json!({"Name":"../../etc/passwd","Digest":digest}),
            serde_json::json!({"Name":"docker.io/library/ubuntu","Digest":"sha256:abcd"}),
            serde_json::json!({"Name":"docker.io/library/ubuntu","Digest":format!("sha512:{}", "a".repeat(128))}),
        ] {
            assert!(resolved_reference_from_inspect(bad.to_string().as_bytes()).is_err());
        }
    }

    #[test]
    fn immutable_admission_rejects_tamper_legacy_and_policy_rotation() {
        let root = std::env::temp_dir().join(format!(
            "tarit-image-admission-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let rootfs = root.join("ubuntu.ext4");
        let trusted_key = root.join("trusted.pub");
        let rotated_key = root.join("rotated.pub");
        std::fs::write(&rootfs, b"immutable ubuntu rootfs").unwrap();
        std::fs::write(&trusted_key, b"trusted key v1").unwrap();
        std::fs::write(&rotated_key, b"trusted key v2").unwrap();
        let source_digest = format!("sha256:{}", "b".repeat(64));
        let mut record = ImageRecord {
            name: "ubuntu".into(),
            tag: "24.04".into(),
            rootfs_path: rootfs.display().to_string(),
            created_at: Utc::now(),
            size_bytes: std::fs::metadata(&rootfs).unwrap().len(),
            source_ref: format!("docker://docker.io/library/ubuntu@{source_digest}"),
            source_digest: Some(source_digest),
            rootfs_digest: Some(sha256_regular_file(&rootfs).unwrap()),
            agent_digest: Some(format!("sha256:{}", "c".repeat(64))),
            provenance_key_digest: Some(sha256_regular_file(&trusted_key).unwrap()),
            provenance_verified_at: Some(Utc::now()),
            golden_snapshot_path: None,
        };
        let signed_policy = ImageAdmissionPolicy {
            require_signature: true,
            cosign_key: Some(trusted_key),
        };
        verify_admitted_image(&record, &signed_policy).unwrap();

        let rotated_policy = ImageAdmissionPolicy {
            require_signature: true,
            cosign_key: Some(rotated_key),
        };
        assert!(verify_admitted_image(&record, &rotated_policy)
            .unwrap_err()
            .to_string()
            .contains("currently trusted"));

        std::fs::write(&rootfs, b"mutated rootfs").unwrap();
        assert!(verify_admitted_image(&record, &signed_policy)
            .unwrap_err()
            .to_string()
            .contains("digest mismatch"));
        std::fs::write(&rootfs, b"immutable ubuntu rootfs").unwrap();

        record.source_digest = None;
        assert!(
            verify_admitted_image(&record, &ImageAdmissionPolicy::default())
                .unwrap_err()
                .to_string()
                .contains("legacy record")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gc_candidates_skip_protected_and_apply_age_and_pattern() {
        let now = Utc::now();
        let images = vec![
            image(
                "node",
                "18",
                "/images/node18.ext4",
                now - Duration::days(10),
            ),
            image(
                "node",
                "20",
                "/images/node20.ext4",
                now - Duration::days(10),
            ),
            image(
                "busybox",
                "latest",
                "/images/busybox.ext4",
                now - Duration::days(1),
            ),
        ];
        let protected = HashSet::from(["/images/node20.ext4".to_string()]);

        let selected =
            select_gc_candidates(&images, &protected, now, Duration::days(7), Some("node:*"));

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].label(), "node:18");
    }

    trait ImageRecordExt {
        fn label(&self) -> String;
    }

    impl ImageRecordExt for ImageRecord {
        fn label(&self) -> String {
            format!("{}:{}", self.name, self.tag)
        }
    }
}
