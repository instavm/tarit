//! OCI image fetching — pull from registries (Docker Hub, GHCR) and
//! convert to a bootable ext4 disk image.
//!
//! Pipeline:
//! 1. Pull the OCI image using `skopeo` (handles auth, manifest, layers)
//! 2. Unpack the layers using `umoci` (flattens overlay layers to a dir)
//! 3. Create an ext4 disk image from the directory using `mke2fs -d`
//! 4. Boot the VM with the ext4 image as a virtio-blk volume + root=/dev/vda
//!
//! This runs as an external pipeline (skopeo + umoci + mke2fs) rather than
//! a pure-Rust implementation because:
//! - skopeo handles registry auth, manifest parsing, layer pulling, caching
//! - umoci handles layer unpacking (tar.gz + overlay whiteout)
//! - mke2fs -d creates a proper ext4 filesystem from a directory
//!
//! The VMM binary itself stays lean — the OCI pipeline is an orchestrator
//! concern. The VMM just boots the resulting ext4 image.

use flate2::read::MultiGzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OciError {
    #[error("skopeo not found — install with: apt install skopeo")]
    SkopeoNotFound,
    #[error("umoci not found — install with: apt install umoci")]
    UmociNotFound,
    #[error("mke2fs not found — install with: apt install e2fsprogs")]
    Mke2fsNotFound,
    #[error("command failed: {cmd}: {stderr}")]
    CommandFailed { cmd: String, stderr: String },
    #[error("invalid output path: {0}")]
    InvalidOutput(String),
    #[error("unsafe OCI rootfs path: {0}")]
    UnsafePath(String),
    #[error("invalid OCI layout: {0}")]
    InvalidLayout(String),
    #[error("OCI resource limit exceeded: {0}")]
    ResourceLimit(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// An OCI image reference (e.g., "docker://ubuntu:22.04", "ghcr.io/owner/repo:tag").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciImageRef {
    /// Full image reference (e.g., "docker://ubuntu:22.04").
    pub reference: String,
    /// Optional auth file path (for private registries).
    pub auth_file: Option<String>,
}

/// Result of pulling + converting an OCI image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciPullResult {
    /// Path to the resulting ext4 disk image.
    pub disk_image_path: String,
    /// Size of the disk image in bytes.
    pub size_bytes: u64,
    /// Time taken in milliseconds.
    pub elapsed_ms: u64,
    /// Whether the guest exec agent was injected as the image's init. When
    /// true the image boots straight to the agent (no init system required),
    /// which is what makes an app image like `node:20` usable as a microVM.
    pub agent_init: bool,
}

const MAX_OCI_LAYERS: usize = 128;
const MAX_OCI_JSON_BYTES: u64 = 8 * 1024 * 1024;
const MAX_OCI_PATH_BYTES: usize = 4096;
const OCI_EXPANSION_MULTIPLIER: u64 = 2;
const OCI_ENTRIES_PER_DISK_MIB: u64 = 256;
const MIN_OCI_ENTRIES: u64 = 16_384;

#[derive(Debug, Clone, Copy)]
struct OciResourceLimits {
    max_layers: usize,
    max_compressed_bytes: u64,
    max_expanded_bytes: u64,
    max_entries: u64,
    max_single_file_bytes: u64,
    max_path_bytes: usize,
}

impl OciResourceLimits {
    fn for_disk_size(size_mb: u64) -> Result<Self, OciError> {
        let disk_bytes = size_mb
            .checked_mul(1024 * 1024)
            .ok_or_else(|| OciError::ResourceLimit("disk size overflows bytes".into()))?;
        if disk_bytes == 0 {
            return Err(OciError::ResourceLimit(
                "disk size must be greater than zero".into(),
            ));
        }
        let stream_budget = disk_bytes
            .checked_mul(OCI_EXPANSION_MULTIPLIER)
            .ok_or_else(|| OciError::ResourceLimit("OCI byte budget overflows".into()))?;
        Ok(Self {
            max_layers: MAX_OCI_LAYERS,
            max_compressed_bytes: stream_budget,
            max_expanded_bytes: stream_budget,
            max_entries: size_mb
                .saturating_mul(OCI_ENTRIES_PER_DISK_MIB)
                .max(MIN_OCI_ENTRIES),
            max_single_file_bytes: disk_bytes,
            max_path_bytes: MAX_OCI_PATH_BYTES,
        })
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct OciPreflightStats {
    layers: usize,
    compressed_bytes: u64,
    expanded_bytes: u64,
    entries: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OciDescriptor {
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    annotations: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OciIndex {
    manifests: Vec<OciDescriptor>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OciManifest {
    config: OciDescriptor,
    layers: Vec<OciDescriptor>,
}

/// Pull an OCI image and convert it to a bootable ext4 disk image.
///
/// Steps:
/// 1. `skopeo copy docker://image:tag oci:image-oci:default`
/// 2. `umoci unpack --image image-oci:default rootfs-dir`
/// 3. `mke2fs -t ext4 -d rootfs-dir -L rootfs disk.ext4 1G`
pub fn pull_and_convert(
    image: &OciImageRef,
    output_path: &Path,
    size_mb: u64,
) -> Result<OciPullResult, OciError> {
    pull_and_convert_with_agent(image, output_path, size_mb, None)
}

/// Like [`pull_and_convert`], but also injects the guest exec agent as the
/// image's init when `agent_path` is given. OCI/Docker images ship only a root
/// filesystem — no kernel, and their configured entrypoint is normally started
/// by a container runtime. To boot one as a microVM we supply the kernel, install
/// the agent at `/usr/sbin/vmm-agent`, and atomically point `/sbin/init` at it.
/// This deliberately replaces an image-provided init: otherwise images such as
/// Alpine try to start an incomplete OpenRC userspace and never expose the exec
/// channel. Booted with the agent as PID 1, app images become directly runnable
/// sandboxes.
pub fn pull_and_convert_with_agent(
    image: &OciImageRef,
    output_path: &Path,
    size_mb: u64,
    agent_path: Option<&Path>,
) -> Result<OciPullResult, OciError> {
    let start = std::time::Instant::now();

    // Reject invalid or overflowing budgets before invoking network tools or
    // creating a build workspace.
    let limits = OciResourceLimits::for_disk_size(size_mb)?;

    // A local container engine can export any image it can run, flattened
    // and with its own registry credentials. Use it when asked to, or when a
    // registry reference cannot be pulled because skopeo/umoci are missing.
    if let Some((engine, name)) = engine_image_source(&image.reference) {
        return convert_engine_image(
            engine,
            name,
            output_path,
            size_mb,
            agent_path,
            limits,
            start,
        );
    }
    if let Some(name) = image.reference.strip_prefix("docker://") {
        if (which("skopeo").is_none() || which("umoci").is_none()) && image.auth_file.is_none() {
            if let Some(engine) = ENGINES.iter().find(|engine| which(engine).is_some()) {
                log::info!("oci: skopeo/umoci unavailable; exporting {name} through {engine}");
                return convert_engine_image(
                    engine,
                    name,
                    output_path,
                    size_mb,
                    agent_path,
                    limits,
                    start,
                );
            }
        }
    }

    // Check for required tools.
    check_tool("skopeo")?;
    check_tool("umoci")?;
    check_tool("mke2fs")?;

    let final_output = resolved_output_path(output_path)?;
    let output_dir = final_output
        .parent()
        .expect("resolved output always has a parent");
    // Every build receives an atomic, mode-0700 workspace. Fixed sibling
    // directories allowed concurrent builds to delete or corrupt each other.
    let workspace = create_oci_workspace(output_dir)?;
    let oci_dir = workspace.path().join("oci-image");
    let rootfs_dir = workspace.path().join("bundle");
    let staged_output = workspace.path().join("disk.ext4");

    // Step 1: Pull the image via skopeo.
    log::info!("oci: pulling {} via skopeo", image.reference);
    let mut skopeo_cmd = std::process::Command::new("skopeo");
    skopeo_cmd
        .arg("copy")
        .arg(&image.reference)
        .arg(format!("oci:{}:default", oci_dir.display()));
    if let Some(ref auth) = image.auth_file {
        skopeo_cmd.arg("--authfile").arg(auth);
    }
    run_command(skopeo_cmd, "skopeo copy")?;

    // Descriptor sizes only bound the compressed blobs. Stream every layer
    // before extraction so a high-ratio archive or inode flood cannot consume
    // the worker's filesystem while the unpacker is running.
    let preflight = preflight_oci_layout(&oci_dir, "default", limits)?;
    log::info!(
        "oci: admitted {} layers, {} compressed bytes, {} expanded bytes, {} entries",
        preflight.layers,
        preflight.compressed_bytes,
        preflight.expanded_bytes,
        preflight.entries
    );

    // Step 2: Unpack layers via umoci.
    log::info!("oci: unpacking layers via umoci");
    let mut umoci = Command::new("umoci");
    umoci
        .arg("unpack")
        .arg("--image")
        .arg(format!("{}:default", oci_dir.display()))
        .arg(&rootfs_dir);
    run_command(umoci, "umoci unpack")?;

    // Step 2b: Inject the exec agent as init so the (initless) image can boot.
    let agent_init = match agent_path {
        Some(agent) => {
            inject_agent_init(&rootfs_dir.join("rootfs"), agent)?;
            true
        }
        None => false,
    };

    // Step 3: Create ext4 disk image via mke2fs. umoci unpacks the root fs into
    // a `rootfs` subdirectory of the bundle.
    let src_dir = rootfs_dir.join("rootfs");
    log::info!("oci: creating ext4 disk image ({}MB)", size_mb);
    let mut mke2fs = Command::new("mke2fs");
    mke2fs
        .arg("-t")
        .arg("ext4")
        .arg("-d")
        .arg(&src_dir)
        .arg("-L")
        .arg("rootfs")
        .arg("-F")
        .arg(&staged_output)
        .arg(format!("{}M", size_mb));
    run_command(mke2fs, "mke2fs")?;
    publish_disk_image(&staged_output, &final_output)?;

    let size_bytes = std::fs::metadata(&final_output)?.len();
    let elapsed_ms = start.elapsed().as_millis() as u64;

    log::info!(
        "oci: conversion complete — {} bytes in {}ms (agent_init={})",
        size_bytes,
        elapsed_ms,
        agent_init
    );

    Ok(OciPullResult {
        disk_image_path: output_path.to_string_lossy().to_string(),
        size_bytes,
        elapsed_ms,
        agent_init,
    })
}

/// Container engines whose `export` yields an image's flattened rootfs.
const ENGINES: [&str; 2] = ["docker", "podman"];

/// `docker-daemon:IMAGE` and `podman:IMAGE` select a local engine.
fn engine_image_source(reference: &str) -> Option<(&'static str, &str)> {
    if let Some(name) = reference.strip_prefix("docker-daemon:") {
        return Some(("docker", name));
    }
    reference
        .strip_prefix("podman:")
        .map(|name| ("podman", name))
}

// Darwin's mode_t is u16, while Linux uses u32; exported ext4 modes are u32.
#[allow(clippy::useless_conversion)]
fn image_mode(mode: libc::mode_t) -> u32 {
    u32::from(mode)
}

/// Owner, mode and type of one exported entry, applied to the ext4 image when
/// the build runs without root and could not preserve them while unpacking.
#[derive(Clone)]
struct EntryMeta {
    uid: u64,
    gid: u64,
    /// Full `st_mode`, including the file type bits.
    mode: u32,
    /// Device numbers for character and block devices.
    device: Option<(u32, u32)>,
}

type InodeIdentity = (u64, u64);

#[derive(Default)]
struct ExportMetadata {
    inodes: HashMap<InodeIdentity, EntryMeta>,
    paths: HashMap<PathBuf, InodeIdentity>,
    special: HashMap<PathBuf, EntryMeta>,
}

fn inode_identity(metadata: &std::fs::Metadata) -> InodeIdentity {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

/// Match tar::Entry::unpack_in's path normalization, including leading './'.
fn export_relative_path(path: &Path) -> Result<PathBuf, OciError> {
    use std::path::Component;
    let mut relative = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(name) => relative.push(name),
            Component::CurDir | Component::RootDir => {}
            _ => return Err(OciError::UnsafePath(path.display().to_string())),
        }
    }
    Ok(relative)
}

/// libss quotes tokens by doubling `"`; backslashes are ordinary bytes.
/// Keep paths as bytes: neither Unix nor ext4 requires UTF-8 filenames.
/// See e2fsprogs lib/ss/parse.c. Requests containing CR/LF must use `-R`,
/// because debugfs's command-file reader truncates at either byte.
fn debugfs_path(path: &Path) -> Result<Vec<u8>, OciError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(OciError::UnsafePath("filename contains NUL".into()));
    }
    let mut quoted = Vec::with_capacity(bytes.len() + 3);
    quoted.extend_from_slice(b"\"/");
    for &byte in bytes {
        quoted.push(byte);
        if byte == b'"' {
            quoted.push(byte);
        }
    }
    quoted.push(b'"');
    Ok(quoted)
}

fn debugfs_request(operation: &str, path: &[u8], arguments: &str) -> Vec<u8> {
    let mut request = operation.as_bytes().to_vec();
    request.push(b' ');
    request.extend_from_slice(path);
    request.extend_from_slice(arguments.as_bytes());
    request
}

/// Build an ext4 image from an image a local container engine can export.
fn convert_engine_image(
    engine: &str,
    name: &str,
    output_path: &Path,
    size_mb: u64,
    agent_path: Option<&Path>,
    limits: OciResourceLimits,
    start: std::time::Instant,
) -> Result<OciPullResult, OciError> {
    if name.is_empty() || name.starts_with('-') {
        return Err(OciError::InvalidLayout(format!(
            "invalid image name {name:?}"
        )));
    }
    check_tool(engine)?;
    check_tool("mke2fs")?;
    let rootless = !can_set_any_owner();
    if rootless {
        check_tool("debugfs")?;
    }

    let final_output = resolved_output_path(output_path)?;
    let output_dir = final_output
        .parent()
        .expect("resolved output always has a parent");
    let workspace = create_oci_workspace(output_dir)?;
    let rootfs = workspace.path().join("rootfs");
    std::fs::create_dir(&rootfs)?;
    let staged_output = workspace.path().join("disk.ext4");

    // `create` pulls the image when missing. The entrypoint is never run; it
    // only lets images without a default command (distroless) be created.
    log::info!("oci: exporting {name} through {engine}");
    let mut create = Command::new(engine);
    create.args(["create", "--entrypoint", "/tarit-export-only", name]);
    let created = create.output().map_err(|e| OciError::CommandFailed {
        cmd: format!("{engine} create"),
        stderr: e.to_string(),
    })?;
    if !created.status.success() {
        return Err(OciError::CommandFailed {
            cmd: format!("{engine} create {name}"),
            stderr: String::from_utf8_lossy(&created.stderr).trim().to_string(),
        });
    }
    let container = String::from_utf8_lossy(&created.stdout).trim().to_string();
    if container.is_empty() || !container.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(OciError::CommandFailed {
            cmd: format!("{engine} create {name}"),
            stderr: format!("unexpected container id {container:?}"),
        });
    }
    struct RemoveContainer<'a>(&'a str, &'a str);
    impl Drop for RemoveContainer<'_> {
        fn drop(&mut self) {
            let _ = Command::new(self.0).args(["rm", "-f", self.1]).output();
        }
    }
    let _remove = RemoveContainer(engine, &container);

    let mut export = Command::new(engine)
        .args(["export", &container])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| OciError::CommandFailed {
            cmd: format!("{engine} export"),
            stderr: e.to_string(),
        })?;
    let stdout = export.stdout.take().expect("piped stdout");
    let unpacked = unpack_export(stdout, &rootfs, limits, rootless);
    let exported = export.wait_with_output()?;
    let metadata = unpacked?;
    if !exported.status.success() {
        return Err(OciError::CommandFailed {
            cmd: format!("{engine} export"),
            stderr: String::from_utf8_lossy(&exported.stderr).trim().to_string(),
        });
    }

    let agent_init = match agent_path {
        Some(agent) => {
            inject_agent_init(&rootfs, agent)?;
            true
        }
        None => false,
    };

    log::info!("oci: creating ext4 disk image ({}MB)", size_mb);
    let mut mke2fs = Command::new("mke2fs");
    mke2fs
        .args(["-q", "-t", "ext4", "-b", "1024", "-d"])
        .arg(&rootfs)
        .args(["-L", "rootfs", "-F"])
        .arg(&staged_output)
        .arg(format!("{size_mb}M"));
    run_command(mke2fs, "mke2fs")?;
    if rootless {
        restore_image_ownership(&rootfs, &metadata, &staged_output, workspace.path())?;
    }
    publish_disk_image(&staged_output, &final_output)?;

    let size_bytes = std::fs::metadata(&final_output)?.len();
    let elapsed_ms = start.elapsed().as_millis() as u64;
    log::info!(
        "oci: {name} converted through {engine} — {size_bytes} bytes in {elapsed_ms}ms \
         (agent_init={agent_init}, rootless={rootless})"
    );
    Ok(OciPullResult {
        disk_image_path: output_path.to_string_lossy().to_string(),
        size_bytes,
        elapsed_ms,
        agent_init,
    })
}

/// Unpack an exported rootfs tar into `rootfs`, enforcing the same budgets as
/// registry pulls. As root, ownership, modes and device nodes are kept. Without
/// root they cannot be, so each entry's real owner, mode and device numbers are
/// recorded for `restore_image_ownership`, and the staged copy is made
/// readable and traversable by the build user.
fn unpack_export<R: Read>(
    reader: R,
    rootfs: &Path,
    limits: OciResourceLimits,
    rootless: bool,
) -> Result<ExportMetadata, OciError> {
    use std::os::unix::fs::PermissionsExt;

    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(!rootless);
    archive.set_preserve_ownerships(!rootless);
    archive.set_unpack_xattrs(!rootless);
    archive.set_overwrite(true);
    let mut metadata = ExportMetadata::default();
    let mut entries = 0u64;
    let mut expanded = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        entries += 1;
        if entries > limits.max_entries {
            return Err(OciError::ResourceLimit(format!(
                "image has more than {} entries",
                limits.max_entries
            )));
        }
        let size = entry.header().size()?;
        if size > limits.max_single_file_bytes {
            return Err(OciError::ResourceLimit(format!("file of {size} bytes")));
        }
        expanded = expanded.saturating_add(size);
        if expanded > limits.max_expanded_bytes {
            return Err(OciError::ResourceLimit(format!(
                "image expands past {} bytes",
                limits.max_expanded_bytes
            )));
        }
        let relative = entry.path()?.into_owned();
        if relative.as_os_str().len() > limits.max_path_bytes {
            return Err(OciError::ResourceLimit("path too long".into()));
        }
        let relative = export_relative_path(&relative)?;
        let kind = entry.header().entry_type();
        if relative.as_os_str().is_empty() && kind != tar::EntryType::Directory {
            return Err(OciError::UnsafePath("non-directory archive root".into()));
        }
        let mode = entry.header().mode()? & 0o7777;
        let type_bits = match kind {
            tar::EntryType::Directory => libc::S_IFDIR,
            tar::EntryType::Symlink => libc::S_IFLNK,
            tar::EntryType::Char => libc::S_IFCHR,
            tar::EntryType::Block => libc::S_IFBLK,
            tar::EntryType::Fifo => libc::S_IFIFO,
            tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::Link => {
                libc::S_IFREG
            }
            _ => continue,
        };
        let device = match kind {
            tar::EntryType::Char | tar::EntryType::Block => Some((
                entry.header().device_major()?.unwrap_or(0),
                entry.header().device_minor()?.unwrap_or(0),
            )),
            _ => None,
        };
        let meta = EntryMeta {
            uid: entry.header().uid()?,
            gid: entry.header().gid()?,
            mode: image_mode(type_bits) | mode,
            device,
        };
        let special = matches!(
            kind,
            tar::EntryType::Char | tar::EntryType::Block | tar::EntryType::Fifo
        );
        if rootless && special {
            // Device nodes and FIFOs are created in the image by debugfs.
            metadata.paths.remove(&relative);
            metadata.special.insert(relative, meta);
            continue;
        }
        if !entry.unpack_in(rootfs)? {
            return Err(OciError::UnsafePath(relative.display().to_string()));
        }
        if rootless && kind != tar::EntryType::Symlink && kind != tar::EntryType::Link {
            let staged = rootfs.join(&relative);
            let usable = if kind == tar::EntryType::Directory {
                mode | 0o700
            } else {
                (mode | 0o600) & 0o777
            };
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(usable))?;
        }
        let identity = inode_identity(&std::fs::symlink_metadata(rootfs.join(&relative))?);
        if kind == tar::EntryType::Link {
            // The inode's original entry, not the hardlink header, owns its
            // metadata. Keep it even if its original pathname is replaced.
            if !metadata.inodes.contains_key(&identity) {
                return Err(OciError::UnsafePath(format!(
                    "hardlink target has no exported inode metadata: {}",
                    relative.display()
                )));
            }
        } else {
            metadata.inodes.insert(identity, meta);
        }
        metadata.special.remove(&relative);
        metadata.paths.insert(relative, identity);
    }
    Ok(metadata)
}

const MAX_OCI_UNBATCHED_SPECIAL_LINKS: usize = 64;
const DEBUGFS_DIRECTORY_END: &[u8] = b"# tarit-directory-end\n";

struct OwnershipEntry {
    path: PathBuf,
    identity: InodeIdentity,
    staged_mode: u32,
    meta: Option<EntryMeta>,
}

struct SpecialOwnership {
    path: PathBuf,
    temporary: PathBuf,
    meta: EntryMeta,
}

struct OwnershipPlan {
    entries: Vec<OwnershipEntry>,
    special: Vec<SpecialOwnership>,
}

/// Discover image inodes in one read-only session, then apply metadata with
/// numeric targets. Arbitrary filename bytes never enter metadata commands.
fn restore_image_ownership(
    rootfs: &Path,
    metadata: &ExportMetadata,
    image: &Path,
    workspace: &Path,
) -> Result<(), OciError> {
    let processes = apply_image_ownership(rootfs, metadata, image, workspace)?;
    log::debug!("oci: restored image metadata with {processes} debugfs processes");
    Ok(())
}

fn apply_image_ownership(
    rootfs: &Path,
    metadata: &ExportMetadata,
    image: &Path,
    workspace: &Path,
) -> Result<usize, OciError> {
    // Validate collisions and the exceptional-link budget before starting
    // debugfs or changing any image bytes.
    let plan = image_ownership_plan(rootfs, metadata)?;
    let image_inodes = map_image_inodes(&plan, image, workspace)?;
    let mut processes = 1;
    let mut requests = Vec::new();
    let mut seen = HashSet::new();
    for entry in &plan.entries {
        let inode = image_inodes[&entry.path];
        if seen.insert(inode) {
            append_inode_ownership(&mut requests, format!("<{inode}>").as_bytes(), entry);
        }
    }

    if !plan.special.is_empty() {
        let (special_inodes, creation_processes) =
            create_special_inodes(&plan.special, image, workspace)?;
        processes += creation_processes;
        let mut links = Vec::new();
        let mut exceptional_links = Vec::new();
        // debugfs ln does not expand destination directories. Each new block
        // supplies at least 1000 usable bytes, and every dirent is <=264 bytes.
        // Before first-fit insertion could fail, each added block would have
        // used >736 bytes (1000 - 264). Reserving ceil(total/736) blocks makes
        // that impossible, independent of link ordering or existing slack.
        let mut directory_bytes = HashMap::<u32, usize>::new();
        for special in &plan.special {
            let parent = special.path.parent().unwrap_or_else(|| Path::new(""));
            let bytes =
                (8 + special.path.file_name().expect("special basename").len()).next_multiple_of(4);
            *directory_bytes.entry(image_inodes[parent]).or_default() += bytes;
        }
        for (directory, bytes) in directory_bytes {
            for _ in 0..bytes.div_ceil(736) {
                links.push(format!("expand_dir <{directory}>").into_bytes());
            }
        }
        for (special, inode) in plan.special.iter().zip(special_inodes) {
            if !seen.insert(inode) {
                return Err(OciError::InvalidLayout(
                    "duplicate special inode identity".into(),
                ));
            }
            let target = format!("<{inode}>");
            let link = special_link_request(inode, &special.path)?;
            if debugfs_batchable(&link) {
                links.push(link);
            } else {
                exceptional_links.push(link);
            }
            append_meta_commands(&mut requests, target.as_bytes(), &special.meta);
        }
        // Links are independent. Group ordinary links first so each unusual
        // link adds just one process, not additional flush/cleanup processes.
        links.extend(exceptional_links);
        processes += run_debugfs_requests(image, workspace, links)?;
    }
    processes += run_debugfs_requests(image, workspace, requests)?;
    Ok(processes)
}

fn debugfs_batchable(request: &[u8]) -> bool {
    // fgets(BUFSIZ) has only 1024 bytes on Darwin, including newline and NUL.
    request.len() <= 1022 && !request.contains(&b'\n') && !request.contains(&b'\r')
}

fn run_debugfs_requests(
    image: &Path,
    workspace: &Path,
    requests: Vec<Vec<u8>>,
) -> Result<usize, OciError> {
    let script_path = workspace.join("ownership.debugfs");
    let mut processes = 0;
    let mut batch = Vec::new();
    for request in requests {
        if debugfs_batchable(&request) {
            if batch.len() + request.len() + 1 > 1024 * 1024 {
                processes += run_debugfs_batch(image, &script_path, &mut batch)?;
            }
            batch.extend_from_slice(&request);
            batch.push(b'\n');
        } else {
            processes += run_debugfs_batch(image, &script_path, &mut batch)?;
            let mut command = Command::new("debugfs");
            command
                .args(["-w", "-R"])
                .arg(std::ffi::OsStr::from_bytes(&request))
                .arg(image);
            run_debugfs(command)?;
            processes += 1;
        }
    }
    processes += run_debugfs_batch(image, &script_path, &mut batch)?;
    Ok(processes)
}

fn run_debugfs_batch(
    image: &Path,
    script_path: &Path,
    batch: &mut Vec<u8>,
) -> Result<usize, OciError> {
    if batch.is_empty() {
        return Ok(0);
    }
    std::fs::write(script_path, &*batch)?;
    let mut command = Command::new("debugfs");
    command.args(["-w", "-f"]).arg(script_path).arg(image);
    run_debugfs(command)?;
    batch.clear();
    Ok(1)
}

fn run_debugfs(mut command: Command) -> Result<(), OciError> {
    let output = command.output().map_err(|e| OciError::CommandFailed {
        cmd: "debugfs".into(),
        stderr: e.to_string(),
    })?;
    check_debugfs_output(output.status, &output.stderr)
}

fn check_debugfs_output(status: std::process::ExitStatus, stderr: &[u8]) -> Result<(), OciError> {
    // Per-command failures can leave exit status zero. Ignore only the first
    // version banner, not subsequent diagnostics containing filename bytes.
    let stderr = String::from_utf8_lossy(stderr);
    let failures: Vec<&str> = stderr
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            (!(line.trim().is_empty() || (index == 0 && line.starts_with("debugfs "))))
                .then_some(line)
        })
        .collect();
    if !status.success() || !failures.is_empty() {
        return Err(OciError::CommandFailed {
            cmd: "debugfs ownership fix-up".into(),
            stderr: failures
                .iter()
                .take(5)
                .copied()
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    Ok(())
}

fn image_ownership_plan(
    rootfs: &Path,
    metadata: &ExportMetadata,
) -> Result<OwnershipPlan, OciError> {
    use std::os::unix::fs::MetadataExt;
    // Retain metadata by live host inode, including hardlinks whose original
    // tar pathname was replaced. Build-injected replacement inodes stay root.
    let live: HashSet<_> = metadata
        .paths
        .iter()
        .filter_map(|(path, expected)| {
            std::fs::symlink_metadata(rootfs.join(path))
                .ok()
                .filter(|stat| inode_identity(stat) == *expected)
                .map(|_| *expected)
        })
        .collect();
    let mut entries = Vec::new();
    let mut staged_paths = HashSet::new();
    let mut staged_directories = HashSet::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(path) = stack.pop() {
        let stat = std::fs::symlink_metadata(rootfs.join(&path))?;
        staged_paths.insert(path.clone());
        if stat.is_dir() {
            staged_directories.insert(path.clone());
            let mut children = std::fs::read_dir(rootfs.join(&path))?
                .map(|child| child.map(|child| path.join(child.file_name())))
                .collect::<Result<Vec<_>, _>>()?;
            children.sort();
            stack.extend(children.into_iter().rev());
        }
        let identity = inode_identity(&stat);
        let meta = live
            .contains(&identity)
            .then(|| metadata.inodes.get(&identity))
            .flatten()
            .cloned();
        entries.push(OwnershipEntry {
            path,
            identity,
            staged_mode: stat.mode(),
            meta,
        });
    }
    let mut exceptional = 0;
    for path in metadata.special.keys() {
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        // ln treats existing directories as link destinations. Reject them,
        // mke2fs-created lost+found, and parents reached only through symlinks.
        // Comparing walk results also supports raw-byte leaves on APFS.
        if path == Path::new("lost+found")
            || staged_paths.contains(path)
            || !staged_directories.contains(parent)
        {
            return Err(OciError::UnsafePath(format!(
                "special inode destination must be absent with a real staged parent, and cannot replace lost+found: {path:?}"
            )));
        }
        if path.file_name().is_none_or(|name| name.len() > 255) {
            return Err(OciError::InvalidLayout(
                "special basename exceeds ext4's 255-byte limit".into(),
            ));
        }
        // Use the longest possible numeric target when checking the budget.
        if !debugfs_batchable(&special_link_request(u32::MAX, path)?) {
            exceptional += 1;
        }
    }
    if exceptional > MAX_OCI_UNBATCHED_SPECIAL_LINKS {
        return Err(OciError::ResourceLimit(format!(
            "image requires {exceptional} unbatchable special-node links; maximum is {MAX_OCI_UNBATCHED_SPECIAL_LINKS}"
        )));
    }
    let mut special = Vec::new();
    let mut sorted: Vec<_> = metadata.special.iter().collect();
    sorted.sort_by_key(|(path, _)| *path);
    let mut index = 0;
    let temporary = loop {
        let candidate = PathBuf::from(format!(".tarit-oci-node-{index}"));
        index += 1;
        if !staged_paths.contains(&candidate) && !metadata.special.contains_key(&candidate) {
            break candidate;
        }
    };
    for (path, meta) in sorted {
        special_kind(meta)?;
        special.push(SpecialOwnership {
            path: path.clone(),
            temporary: temporary.clone(),
            meta: meta.clone(),
        });
    }
    Ok(OwnershipPlan { entries, special })
}

/// mknod reports the allocated inode numerically. Unlink only the temporary
/// directory entry, keeping the inode allocated with link count one; a later
/// ln <inode> establishes its final name without changing that link count.
fn create_special_inodes(
    special: &[SpecialOwnership],
    image: &Path,
    workspace: &Path,
) -> Result<(Vec<u32>, usize), OciError> {
    let mut inodes = Vec::new();
    let mut processes = 0;
    for chunk in special.chunks(4096) {
        let first = inodes.is_empty();
        let mut script = String::new();
        if first {
            script.push_str("expand_dir <2>\n");
        }
        let mut commands = Vec::new();
        for entry in chunk {
            let create = format!(
                "mknod {} {}",
                entry.temporary.display(),
                special_kind(&entry.meta)?
            );
            let unlink = format!("unlink /{}", entry.temporary.display());
            script.push_str(&create);
            script.push('\n');
            script.push_str(&unlink);
            script.push('\n');
            commands.push((create, unlink));
        }
        let script_path = workspace.join("ownership.debugfs");
        std::fs::write(&script_path, script)?;
        let output = Command::new("debugfs")
            .args(["-w", "-f"])
            .arg(&script_path)
            .arg(image)
            .output()?;
        check_debugfs_output(output.status, &output.stderr)?;
        processes += 1;
        // No filename bytes occur in this output: only our generated ASCII
        // commands and mknod's numeric allocation response. Exact framing
        // rejects unexpected diagnostics before linking or publishing.
        let mut lines = output.stdout.split(|byte| *byte == b'\n');
        let malformed =
            || OciError::InvalidLayout("unexpected debugfs inode allocation response".into());
        if first && lines.next() != Some(b"debugfs: expand_dir <2>".as_slice()) {
            return Err(malformed());
        }
        for (create, unlink) in commands {
            if lines.next() != Some(format!("debugfs: {create}").as_bytes()) {
                return Err(malformed());
            }
            let inode = lines
                .next()
                .and_then(|line| line.strip_prefix(b"Allocated inode: "))
                .and_then(|number| std::str::from_utf8(number).ok())
                .and_then(|number| number.parse::<u32>().ok())
                .filter(|inode| *inode != 0)
                .ok_or_else(malformed)?;
            if lines.next() != Some(format!("debugfs: {unlink}").as_bytes()) {
                return Err(malformed());
            }
            inodes.push(inode);
        }
        if lines.next() != Some(b"".as_slice()) || lines.next().is_some() {
            return Err(malformed());
        }
    }
    Ok((inodes, processes))
}

fn special_kind(meta: &EntryMeta) -> Result<String, OciError> {
    match (meta.mode & image_mode(libc::S_IFMT), meta.device) {
        (kind, Some((major, minor))) if kind == image_mode(libc::S_IFCHR) => {
            Ok(format!("c {major} {minor}"))
        }
        (kind, Some((major, minor))) if kind == image_mode(libc::S_IFBLK) => {
            Ok(format!("b {major} {minor}"))
        }
        (kind, _) if kind == image_mode(libc::S_IFIFO) => Ok("p".into()),
        _ => Err(OciError::InvalidLayout("unsupported special inode".into())),
    }
}

fn special_link_request(inode: u32, path: &Path) -> Result<Vec<u8>, OciError> {
    let mut link = format!("ln <{inode}> ").into_bytes();
    link.extend_from_slice(&debugfs_path(path)?);
    Ok(link)
}

fn append_meta_commands(requests: &mut Vec<Vec<u8>>, target: &[u8], meta: &EntryMeta) {
    requests.push(debugfs_request(
        "sif",
        target,
        &format!(" uid {}", meta.uid),
    ));
    requests.push(debugfs_request(
        "sif",
        target,
        &format!(" gid {}", meta.gid),
    ));
    requests.push(debugfs_request(
        "sif",
        target,
        &format!(" mode 0{:o}", meta.mode),
    ));
}

fn append_inode_ownership(requests: &mut Vec<Vec<u8>>, target: &[u8], entry: &OwnershipEntry) {
    if !entry.path.as_os_str().is_empty() {
        if let Some(meta) = &entry.meta {
            append_meta_commands(requests, target, meta);
            return;
        }
    }
    requests.push(debugfs_request("sif", target, " uid 0"));
    requests.push(debugfs_request("sif", target, " gid 0"));
    if let Some(meta) = &entry.meta {
        requests.push(debugfs_request(
            "sif",
            target,
            &format!(" mode 0{:o}", meta.mode),
        ));
    }
}

#[derive(Debug)]
struct ImageDirectoryEntry {
    inode: u32,
    mode: u32,
    name: Vec<u8>,
}

/// Read only the documented parseable listing format. Fields are separated
/// by '/', which cannot occur in a basename; LF/CR and non-UTF8 remain bytes.
/// See e2fsprogs debugfs/ls.c (PARSE_OPT), not human-readable ls/stat output.
fn read_debugfs_directory(
    reader: &mut impl Read,
    inode: u32,
    max_entries: usize,
) -> Result<Vec<ImageDirectoryEntry>, OciError> {
    fn expect(reader: &mut impl Read, expected: &[u8]) -> Result<(), OciError> {
        let mut bytes = vec![0; expected.len()];
        reader.read_exact(&mut bytes)?;
        if bytes != expected {
            return Err(OciError::InvalidLayout(
                "unexpected debugfs directory framing".into(),
            ));
        }
        Ok(())
    }
    fn field(reader: &mut impl Read, limit: usize) -> Result<Vec<u8>, OciError> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = [0];
            reader.read_exact(&mut byte)?;
            if byte[0] == b'/' {
                return Ok(bytes);
            }
            if bytes.len() == limit || byte[0] == 0 {
                return Err(OciError::InvalidLayout(
                    "invalid debugfs directory field".into(),
                ));
            }
            bytes.push(byte[0]);
        }
    }
    fn number(bytes: &[u8], radix: u32) -> Result<u32, OciError> {
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|value| u32::from_str_radix(value, radix).ok())
            .ok_or_else(|| OciError::InvalidLayout("invalid debugfs inode or mode".into()))
    }
    expect(reader, format!("debugfs: ls -p <{inode}>\n").as_bytes())?;
    let mut entries = Vec::new();
    loop {
        let mut first = [0];
        reader.read_exact(&mut first)?;
        if first[0] == b'\n' {
            expect(reader, DEBUGFS_DIRECTORY_END)?;
            return Ok(entries);
        }
        if first[0] != b'/' {
            return Err(OciError::InvalidLayout(
                "invalid or oversized debugfs directory listing".into(),
            ));
        }
        let inode = number(&field(reader, 10)?, 10)?;
        let mode = number(&field(reader, 6)?, 8)?;
        // e2fsprogs prints owner IDs with signed %d; they are not used for
        // mapping and can legitimately be negative for high-bit IDs.
        for _ in 0..2 {
            let value = field(reader, 11)?;
            if std::str::from_utf8(&value)
                .ok()
                .and_then(|value| value.parse::<i32>().ok())
                .is_none()
            {
                return Err(OciError::InvalidLayout(
                    "invalid debugfs owner field".into(),
                ));
            }
        }
        let name = field(reader, 255)?;
        let size = field(reader, 20)?;
        if (!size.is_empty() && !size.iter().all(u8::is_ascii_digit))
            || (inode != 0 && name.is_empty())
        {
            return Err(OciError::InvalidLayout(
                "invalid debugfs name or size".into(),
            ));
        }
        expect(reader, b"\n")?;
        // ls -p includes empty slots (notably preallocated lost+found
        // blocks). Bound stored live entries, not harmless empty records.
        if inode != 0 {
            if entries.len() == max_entries {
                return Err(OciError::InvalidLayout(
                    "oversized debugfs directory listing".into(),
                ));
            }
            entries.push(ImageDirectoryEntry { inode, mode, name });
        }
    }
}

struct DebugfsDirectoryReader {
    child: std::process::Child,
    input: Option<std::process::ChildStdin>,
    output: std::io::BufReader<std::process::ChildStdout>,
    stderr_path: PathBuf,
}

impl DebugfsDirectoryReader {
    fn open(image: &Path, workspace: &Path) -> Result<Self, OciError> {
        let stderr_path = workspace.join("ownership.stderr");
        let stderr = File::create(&stderr_path)?;
        let mut child = Command::new("debugfs")
            .args(["-f", "-"])
            .arg(image)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(stderr)
            .spawn()?;
        let input = child.stdin.take();
        let output = std::io::BufReader::new(child.stdout.take().expect("piped debugfs stdout"));
        Ok(Self {
            child,
            input,
            output,
            stderr_path,
        })
    }

    fn list(
        &mut self,
        inode: u32,
        max_entries: usize,
    ) -> Result<Vec<ImageDirectoryEntry>, OciError> {
        let input = self.input.as_mut().expect("active debugfs reader");
        writeln!(input, "ls -p <{inode}>")?;
        input.write_all(DEBUGFS_DIRECTORY_END)?;
        input.flush()?;
        // The comment barrier also arrives when ls fails, preventing a wait
        // for a listing terminator that a failed command never emits.
        read_debugfs_directory(&mut self.output, inode, max_entries)
    }

    fn finish(mut self) -> Result<(), OciError> {
        drop(self.input.take());
        let mut extra = [0];
        if self.output.read(&mut extra)? != 0 {
            return Err(OciError::InvalidLayout(
                "unexpected trailing debugfs output".into(),
            ));
        }
        let status = self.child.wait()?;
        check_debugfs_output(status, &std::fs::read(&self.stderr_path)?)
    }
}

impl Drop for DebugfsDirectoryReader {
    fn drop(&mut self) {
        // Covers parse errors, failed writes, and early-return validation.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn map_image_inodes(
    plan: &OwnershipPlan,
    image: &Path,
    workspace: &Path,
) -> Result<HashMap<PathBuf, u32>, OciError> {
    let expected: HashMap<_, _> = plan
        .entries
        .iter()
        .map(|entry| (entry.path.as_path(), entry))
        .collect();
    let mut reader = DebugfsDirectoryReader::open(image, workspace)?;
    let mut image_inodes = HashMap::from([(PathBuf::new(), 2)]);
    let mut host_to_image = HashMap::new();
    let mut image_to_host = HashMap::new();
    let root = expected[Path::new("")];
    host_to_image.insert(root.identity, 2);
    image_to_host.insert(2, root.identity);
    let mut stack = vec![(PathBuf::new(), 2, 2)];
    while let Some((directory, inode, parent_inode)) = stack.pop() {
        let entries = reader.list(inode, plan.entries.len() + 3)?;
        let mut names = HashSet::new();
        for entry in entries {
            if !names.insert(entry.name.clone()) {
                return Err(OciError::InvalidLayout(
                    "duplicate image directory entry".into(),
                ));
            }
            if entry.name == b"." || entry.name == b".." {
                let expected_inode = if entry.name == b"." {
                    inode
                } else {
                    parent_inode
                };
                if entry.inode != expected_inode
                    || entry.mode & image_mode(libc::S_IFMT) != image_mode(libc::S_IFDIR)
                {
                    return Err(OciError::InvalidLayout(
                        "invalid image directory parent".into(),
                    ));
                }
                continue;
            }
            let path = directory.join(std::ffi::OsStr::from_bytes(&entry.name));
            if path == Path::new("lost+found") && !expected.contains_key(path.as_path()) {
                continue;
            }
            let staged = expected
                .get(path.as_path())
                .ok_or_else(|| OciError::InvalidLayout("unexpected image path".into()))?;
            if entry.mode & image_mode(libc::S_IFMT)
                != staged.staged_mode & image_mode(libc::S_IFMT)
                || host_to_image
                    .get(&staged.identity)
                    .is_some_and(|known| *known != entry.inode)
                || image_to_host
                    .get(&entry.inode)
                    .is_some_and(|known| *known != staged.identity)
                || image_inodes.insert(path.clone(), entry.inode).is_some()
            {
                return Err(OciError::InvalidLayout(
                    "image inode identity or type differs from staging".into(),
                ));
            }
            host_to_image.insert(staged.identity, entry.inode);
            image_to_host.insert(entry.inode, staged.identity);
            if entry.mode & image_mode(libc::S_IFMT) == image_mode(libc::S_IFDIR) {
                stack.push((path, entry.inode, inode));
            }
        }
        if !names.contains(b".".as_slice()) || !names.contains(b"..".as_slice()) {
            return Err(OciError::InvalidLayout(
                "missing image directory parent entries".into(),
            ));
        }
    }
    reader.finish()?;
    if image_inodes.len() != expected.len() {
        return Err(OciError::InvalidLayout(
            "image is missing staged paths".into(),
        ));
    }
    Ok(image_inodes)
}

fn resolved_output_path(output_path: &Path) -> Result<PathBuf, OciError> {
    let file_name = output_path.file_name().ok_or_else(|| {
        OciError::InvalidOutput(format!("{} has no file name", output_path.display()))
    })?;
    if file_name.as_bytes().contains(&0) {
        return Err(OciError::InvalidOutput("file name contains NUL".into()));
    }
    let parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = std::fs::canonicalize(parent).map_err(|error| {
        OciError::InvalidOutput(format!(
            "resolve output directory {}: {error}",
            parent.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(&parent)?;
    if !metadata.is_dir() {
        return Err(OciError::InvalidOutput(format!(
            "{} is not a directory",
            parent.display()
        )));
    }
    Ok(parent.join(file_name))
}

fn create_oci_workspace(parent: &Path) -> Result<tempfile::TempDir, OciError> {
    let workspace = tempfile::Builder::new()
        .prefix(".tarit-oci-")
        .tempdir_in(parent)
        .map_err(OciError::Io)?;
    #[cfg(unix)]
    std::fs::set_permissions(workspace.path(), std::fs::Permissions::from_mode(0o700))?;
    Ok(workspace)
}

fn publish_disk_image(staged: &Path, destination: &Path) -> Result<(), OciError> {
    let metadata = std::fs::symlink_metadata(staged)?;
    if !metadata.file_type().is_file() {
        return Err(OciError::UnsafePath(format!(
            "staged disk is not a regular file: {}",
            staged.display()
        )));
    }
    #[cfg(unix)]
    std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o600))?;

    // Open without following links and flush the completed filesystem before
    // atomically publishing it. The private workspace makes this setup-only;
    // it does not add anything to VM boot or restore latency.
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let staged_file = options.open(staged)?;
    if !staged_file.metadata()?.file_type().is_file() {
        return Err(OciError::UnsafePath(format!(
            "staged disk changed type: {}",
            staged.display()
        )));
    }
    staged_file.sync_all()?;

    if let Ok(existing) = std::fs::symlink_metadata(destination) {
        if existing.file_type().is_dir() {
            return Err(OciError::InvalidOutput(format!(
                "refusing to replace output directory {}",
                destination.display()
            )));
        }
    }
    std::fs::rename(staged, destination)?;
    #[cfg(target_os = "linux")]
    File::open(
        destination
            .parent()
            .expect("resolved destination has a parent"),
    )?
    .sync_all()?;
    Ok(())
}

/// Copy the exec agent into an unpacked rootfs and make it the init. Every
/// path component is traversed relative to an already-open directory fd with
/// `O_NOFOLLOW`; image-controlled symlinks cannot redirect writes onto the
/// host. The final executable is written under a unique name and renamed over
/// any untrusted leaf atomically.
fn inject_agent_init(rootfs: &Path, agent: &Path) -> Result<(), OciError> {
    #[cfg(unix)]
    {
        inject_agent_init_unix(rootfs, agent)
    }
    #[cfg(not(unix))]
    {
        let _ = (rootfs, agent);
        Err(OciError::UnsafePath(
            "agent injection requires Unix openat confinement".into(),
        ))
    }
}

#[cfg(unix)]
fn inject_agent_init_unix(rootfs: &Path, agent: &Path) -> Result<(), OciError> {
    let mut root_options = OpenOptions::new();
    root_options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let root = root_options.open(rootfs).map_err(|error| {
        OciError::UnsafePath(format!("open rootfs {}: {error}", rootfs.display()))
    })?;
    if !root.metadata()?.is_dir() {
        return Err(OciError::UnsafePath(format!(
            "rootfs is not a directory: {}",
            rootfs.display()
        )));
    }

    let usr = open_or_create_directory_at(&root, "usr")?;
    let usr_sbin = match entry_kind_at(&usr, "sbin")? {
        None => open_or_create_directory_at(&usr, "sbin")?,
        Some(EntryKind::Directory) => open_directory_at(&usr, "sbin")?,
        Some(EntryKind::Symlink) => {
            let target = read_link_at(&usr, "sbin")?;
            if target.as_bytes() != b"bin" && target.as_bytes() != b"/usr/bin" {
                return Err(OciError::UnsafePath(format!(
                    "unsupported /usr/sbin symlink target {:?}; expected bin",
                    target
                )));
            }
            open_directory_at(&usr, "bin")?
        }
        Some(EntryKind::Other) => {
            return Err(OciError::UnsafePath(
                "/usr/sbin exists but is neither a directory nor a safe symlink".into(),
            ))
        }
    };
    install_agent_at(&usr_sbin, agent)?;

    let init_dir = match entry_kind_at(&root, "sbin")? {
        None => open_or_create_directory_at(&root, "sbin")?,
        Some(EntryKind::Directory) => open_directory_at(&root, "sbin")?,
        Some(EntryKind::Symlink) => {
            let target = read_link_at(&root, "sbin")?;
            if target.as_bytes() != b"usr/sbin" && target.as_bytes() != b"/usr/sbin" {
                return Err(OciError::UnsafePath(format!(
                    "unsupported /sbin symlink target {:?}; expected usr/sbin",
                    target
                )));
            }
            usr_sbin.try_clone()?
        }
        Some(EntryKind::Other) => {
            return Err(OciError::UnsafePath(
                "/sbin exists but is neither a directory nor a safe symlink".into(),
            ))
        }
    };
    ensure_init_link_at(&init_dir)?;
    log::info!("oci: injected exec agent as init (/usr/sbin/vmm-agent)");
    Ok(())
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Directory,
    Symlink,
    Other,
}

#[cfg(unix)]
fn c_name(name: &str) -> Result<std::ffi::CString, OciError> {
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| OciError::UnsafePath(format!("path component contains NUL: {name:?}")))
}

#[cfg(unix)]
fn entry_kind_at(parent: &File, name: &str) -> Result<Option<EntryKind>, OciError> {
    let name = c_name(name)?;
    // SAFETY: stat is immediately initialized by fstatat before being read.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: parent and name are valid for the duration of fstatat, and stat
    // is valid writable storage.
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    let file_type = stat.st_mode & libc::S_IFMT;
    Ok(Some(if file_type == libc::S_IFDIR {
        EntryKind::Directory
    } else if file_type == libc::S_IFLNK {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    }))
}

#[cfg(unix)]
fn open_or_create_directory_at(parent: &File, name: &str) -> Result<File, OciError> {
    match entry_kind_at(parent, name)? {
        None => {
            let c_name = c_name(name)?;
            // SAFETY: parent fd and component name are valid; mkdirat retains
            // no pointer after returning.
            let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o755) };
            if rc < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
        }
        Some(EntryKind::Directory) => {}
        Some(kind) => {
            return Err(OciError::UnsafePath(format!(
                "component {name:?} is {kind:?}, not a directory"
            )))
        }
    }
    open_directory_at(parent, name)
}

#[cfg(unix)]
fn open_directory_at(parent: &File, name: &str) -> Result<File, OciError> {
    let name = c_name(name)?;
    // SAFETY: parent fd and name are valid for openat; the returned fd is
    // checked and uniquely transferred to File.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(OciError::UnsafePath(format!(
            "open directory component {:?}: {}",
            name,
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: fd is a fresh successful openat result and is uniquely owned.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn read_link_at(parent: &File, name: &str) -> Result<std::ffi::OsString, OciError> {
    let name = c_name(name)?;
    let mut buffer = vec![0u8; 4096];
    // SAFETY: parent/name are valid and buffer is writable for its full length.
    let len = unsafe {
        libc::readlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if len < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let len =
        usize::try_from(len).map_err(|_| OciError::UnsafePath("negative symlink length".into()))?;
    if len == buffer.len() {
        return Err(OciError::UnsafePath("/sbin symlink target too long".into()));
    }
    buffer.truncate(len);
    Ok(std::ffi::OsString::from_vec(buffer))
}

#[cfg(unix)]
fn install_agent_at(destination_dir: &File, agent: &Path) -> Result<(), OciError> {
    const MAX_AGENT_BYTES: u64 = 64 * 1024 * 1024;
    static AGENT_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    let mut source_options = OpenOptions::new();
    source_options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut source = source_options.open(agent).map_err(|error| {
        OciError::UnsafePath(format!("open agent {}: {error}", agent.display()))
    })?;
    let source_metadata = source.metadata()?;
    if !source_metadata.file_type().is_file()
        || source_metadata.len() == 0
        || source_metadata.len() > MAX_AGENT_BYTES
    {
        return Err(OciError::UnsafePath(format!(
            "agent must be a non-empty regular file no larger than {MAX_AGENT_BYTES} bytes"
        )));
    }

    let sequence = AGENT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temporary_name = format!(".vmm-agent-{}-{sequence}.tmp", std::process::id());
    let temporary = c_name(&temporary_name)?;
    let destination = c_name("vmm-agent")?;
    // SAFETY: directory fd and name are valid, mode is supplied with O_CREAT.
    let fd = unsafe {
        libc::openat(
            destination_dir.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o700,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fd is a fresh successful openat result and is uniquely owned.
    let mut staged = unsafe { File::from_raw_fd(fd) };
    let result = (|| -> Result<(), OciError> {
        let copied = std::io::copy(&mut source, &mut staged)?;
        if copied != source_metadata.len() {
            return Err(OciError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "agent changed size while being copied",
            )));
        }
        staged.flush()?;
        staged.sync_all()?;
        // SAFETY: staged is a valid file descriptor and 0755 is a valid mode.
        if unsafe { libc::fchmod(staged.as_raw_fd(), 0o755) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: both names are valid components relative to the same held
        // directory descriptor. renameat replaces a leaf symlink, never follows it.
        if unsafe {
            libc::renameat(
                destination_dir.as_raw_fd(),
                temporary.as_ptr(),
                destination_dir.as_raw_fd(),
                destination.as_ptr(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        #[cfg(target_os = "linux")]
        destination_dir.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: temporary is a valid leaf name relative to destination_dir;
        // unlinkat cannot traverse outside that held directory.
        unsafe {
            libc::unlinkat(destination_dir.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result
}

#[cfg(unix)]
fn ensure_init_link_at(sbin_dir: &File) -> Result<(), OciError> {
    static INIT_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    let target = c_name("/usr/sbin/vmm-agent")?;
    let init = c_name("init")?;
    let sequence = INIT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temporary_name = format!(".tarit-init-{}-{sequence}.tmp", std::process::id());
    let temporary = c_name(&temporary_name)?;
    // SAFETY: target and leaf name are valid for symlinkat and the held fd
    // confines creation to the image's /sbin directory. O_EXCL-like behavior
    // from symlinkat prevents an image-controlled temporary leaf from being
    // followed or overwritten.
    if unsafe { libc::symlinkat(target.as_ptr(), sbin_dir.as_raw_fd(), temporary.as_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: both names are leaf components relative to the held /sbin fd.
    // renameat atomically replaces a regular file or symlink without following
    // it. A hostile directory at /sbin/init is rejected by the kernel.
    if unsafe {
        libc::renameat(
            sbin_dir.as_raw_fd(),
            temporary.as_ptr(),
            sbin_dir.as_raw_fd(),
            init.as_ptr(),
        )
    } < 0
    {
        let error = std::io::Error::last_os_error();
        // SAFETY: this is the exact private leaf just created above.
        unsafe {
            libc::unlinkat(sbin_dir.as_raw_fd(), temporary.as_ptr(), 0);
        }
        return Err(error.into());
    }
    #[cfg(target_os = "linux")]
    sbin_dir.sync_all()?;
    Ok(())
}

/// Check if a tool is available on PATH.
fn check_tool(name: &str) -> Result<(), OciError> {
    match name {
        "skopeo" if which("skopeo").is_none() => Err(OciError::SkopeoNotFound),
        "umoci" if which("umoci").is_none() => Err(OciError::UmociNotFound),
        "mke2fs" if which("mke2fs").is_none() => Err(OciError::Mke2fsNotFound),
        _ => Ok(()),
    }
}

fn preflight_oci_layout(
    oci_dir: &Path,
    tag: &str,
    limits: OciResourceLimits,
) -> Result<OciPreflightStats, OciError> {
    let index_path = oci_dir.join("index.json");
    let index: OciIndex = read_bounded_json(&index_path, MAX_OCI_JSON_BYTES)?;
    let manifest_descriptor = index
        .manifests
        .iter()
        .find(|descriptor| {
            descriptor
                .annotations
                .get("org.opencontainers.image.ref.name")
                .is_some_and(|name| name == tag)
        })
        .or_else(|| (index.manifests.len() == 1).then(|| &index.manifests[0]))
        .ok_or_else(|| {
            OciError::InvalidLayout(format!(
                "index does not contain an unambiguous {tag:?} manifest"
            ))
        })?;
    if manifest_descriptor.media_type != "application/vnd.oci.image.manifest.v1+json"
        && manifest_descriptor.media_type != "application/vnd.docker.distribution.manifest.v2+json"
    {
        return Err(OciError::InvalidLayout(format!(
            "unsupported manifest media type {}",
            manifest_descriptor.media_type
        )));
    }
    if manifest_descriptor.size > MAX_OCI_JSON_BYTES {
        return Err(OciError::ResourceLimit(format!(
            "image manifest is {} bytes; limit is {}",
            manifest_descriptor.size, MAX_OCI_JSON_BYTES
        )));
    }
    let manifest_path = verify_descriptor_blob(oci_dir, manifest_descriptor)?;
    let manifest: OciManifest = read_bounded_json(&manifest_path, MAX_OCI_JSON_BYTES)?;
    if manifest.config.media_type != "application/vnd.oci.image.config.v1+json"
        && manifest.config.media_type != "application/vnd.docker.container.image.v1+json"
    {
        return Err(OciError::InvalidLayout(format!(
            "unsupported image config media type {}",
            manifest.config.media_type
        )));
    }
    if manifest.config.size > MAX_OCI_JSON_BYTES {
        return Err(OciError::ResourceLimit(format!(
            "image config is {} bytes; limit is {}",
            manifest.config.size, MAX_OCI_JSON_BYTES
        )));
    }
    let config_path = verify_descriptor_blob(oci_dir, &manifest.config)?;
    let _: serde_json::Value = read_bounded_json(&config_path, MAX_OCI_JSON_BYTES)?;
    if manifest.layers.len() > limits.max_layers {
        return Err(OciError::ResourceLimit(format!(
            "{} layers exceeds limit {}",
            manifest.layers.len(),
            limits.max_layers
        )));
    }

    let mut stats = OciPreflightStats {
        layers: manifest.layers.len(),
        ..OciPreflightStats::default()
    };
    for descriptor in &manifest.layers {
        stats.compressed_bytes = stats
            .compressed_bytes
            .checked_add(descriptor.size)
            .ok_or_else(|| OciError::ResourceLimit("compressed byte count overflows".into()))?;
        if stats.compressed_bytes > limits.max_compressed_bytes {
            return Err(OciError::ResourceLimit(format!(
                "compressed layers require {} bytes; limit is {}",
                stats.compressed_bytes, limits.max_compressed_bytes
            )));
        }
        let blob = verify_descriptor_blob(oci_dir, descriptor)?;
        scan_oci_layer(&blob, descriptor, limits, &mut stats)?;
    }
    Ok(stats)
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    max_bytes: u64,
) -> Result<T, OciError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(OciError::InvalidLayout(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if metadata.len() > max_bytes {
        return Err(OciError::ResourceLimit(format!(
            "JSON document {} is {} bytes; limit is {max_bytes}",
            path.display(),
            metadata.len()
        )));
    }
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| OciError::InvalidLayout(format!("parse {}: {error}", path.display())))
}

fn verify_descriptor_blob(oci_dir: &Path, descriptor: &OciDescriptor) -> Result<PathBuf, OciError> {
    let encoded = descriptor
        .digest
        .strip_prefix("sha256:")
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .ok_or_else(|| {
            OciError::InvalidLayout(format!("unsupported digest {}", descriptor.digest))
        })?;
    let path = oci_dir.join("blobs").join("sha256").join(encoded);
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() {
        return Err(OciError::InvalidLayout(format!(
            "blob {} is not a regular file",
            path.display()
        )));
    }
    if metadata.len() != descriptor.size {
        return Err(OciError::InvalidLayout(format!(
            "blob {} has size {}; descriptor requires {}",
            path.display(),
            metadata.len(),
            descriptor.size
        )));
    }
    let mut file = File::open(&path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != encoded {
        return Err(OciError::InvalidLayout(format!(
            "blob {} digest mismatch",
            path.display()
        )));
    }
    Ok(path)
}

fn scan_oci_layer(
    path: &Path,
    descriptor: &OciDescriptor,
    limits: OciResourceLimits,
    stats: &mut OciPreflightStats,
) -> Result<(), OciError> {
    let file = File::open(path)?;
    let reader: Box<dyn Read> = match descriptor.media_type.as_str() {
        "application/vnd.oci.image.layer.v1.tar"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar"
        | "application/vnd.docker.image.rootfs.diff.tar" => Box::new(file),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => {
            // umoci decodes every gzip member, so the scan must too.
            Box::new(MultiGzDecoder::new(file))
        }
        "application/vnd.oci.image.layer.v1.tar+zstd"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd" => {
            Box::new(zstd::stream::read::Decoder::new(file)?)
        }
        other => {
            return Err(OciError::InvalidLayout(format!(
                "unsupported layer media type {other}"
            )))
        }
    };
    let remaining = limits
        .max_expanded_bytes
        .checked_sub(stats.expanded_bytes)
        .ok_or_else(|| OciError::ResourceLimit("expanded byte budget exhausted".into()))?;
    let mut limited = ExpandedLimitReader::new(reader, remaining);
    let scan_result = {
        let mut archive = tar::Archive::new(&mut limited);
        (|| -> Result<(), OciError> {
            for entry in archive.entries()? {
                let mut entry = entry?;
                stats.entries = stats
                    .entries
                    .checked_add(1)
                    .ok_or_else(|| OciError::ResourceLimit("OCI entry count overflows".into()))?;
                if stats.entries > limits.max_entries {
                    return Err(OciError::ResourceLimit(format!(
                        "layer entries exceed limit {}",
                        limits.max_entries
                    )));
                }
                if is_sparse_entry(&mut entry)? {
                    return Err(OciError::InvalidLayout(
                        "sparse layer entries are not supported".into(),
                    ));
                }
                let size = entry.size();
                if size > limits.max_single_file_bytes {
                    return Err(OciError::ResourceLimit(format!(
                        "layer entry is {size} bytes; per-file limit is {}",
                        limits.max_single_file_bytes
                    )));
                }
                let path_bytes = entry.path_bytes();
                if path_bytes.len() > limits.max_path_bytes {
                    return Err(OciError::ResourceLimit(format!(
                        "layer path is {} bytes; limit is {}",
                        path_bytes.len(),
                        limits.max_path_bytes
                    )));
                }
            }
            Ok(())
        })()
    };
    let scan_result = scan_result.and_then(|()| drain_tar_padding(&mut limited));
    if limited.exceeded {
        return Err(OciError::ResourceLimit(format!(
            "expanded layers exceed limit {}",
            limits.max_expanded_bytes
        )));
    }
    scan_result?;
    stats.expanded_bytes = stats
        .expanded_bytes
        .checked_add(limited.consumed)
        .ok_or_else(|| OciError::ResourceLimit("expanded byte count overflows".into()))?;
    Ok(())
}

/// GNU sparse entries, in the old header form or the PAX form, unpack to a
/// logical size far larger than the stream bytes the expanded budget counts.
fn is_sparse_entry<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<bool, OciError> {
    if entry.header().entry_type().is_gnu_sparse() {
        return Ok(true);
    }
    let Some(extensions) = entry.pax_extensions()? else {
        return Ok(false);
    };
    for extension in extensions {
        if extension?.key_bytes().starts_with(b"GNU.sparse.") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Read the rest of the decompressed layer after the tar end-of-archive block.
/// The bytes still count toward the expanded budget and may only be the zero
/// padding archivers append.
fn drain_tar_padding(reader: &mut impl Read) -> Result<(), OciError> {
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        if buffer[..read].iter().any(|byte| *byte != 0) {
            return Err(OciError::InvalidLayout(
                "layer has data after the tar end-of-archive marker".into(),
            ));
        }
    }
}

struct ExpandedLimitReader<R> {
    inner: R,
    limit: u64,
    consumed: u64,
    exceeded: bool,
}

impl<R> ExpandedLimitReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            consumed: 0,
            exceeded: false,
        }
    }
}

impl<R: Read> Read for ExpandedLimitReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.consumed);
        if remaining == 0 {
            let mut probe = [0u8; 1];
            if self.inner.read(&mut probe)? == 0 {
                return Ok(0);
            }
            self.exceeded = true;
            return Err(std::io::Error::other("expanded OCI byte limit exceeded"));
        }
        let allowed = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.consumed = self.consumed.saturating_add(read as u64);
        Ok(read)
    }
}

fn which(cmd: &str) -> Option<PathBuf> {
    let output = Command::new("which").arg(cmd).output().ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn run_command(mut cmd: Command, name: &str) -> Result<(), OciError> {
    let output = cmd.output().map_err(|e| OciError::CommandFailed {
        cmd: name.to_string(),
        stderr: e.to_string(),
    })?;
    if !output.status.success() {
        return Err(OciError::CommandFailed {
            cmd: name.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    Ok(())
}

/// Whether this process can give files any owner: root in a user namespace
/// that maps every ID (the initial one, or a full mapping). Root in a user
/// namespace that maps only its own IDs (as `unshare -Ur` does) cannot chown
/// to an image's other owners, so it unpacks the rootless way and records
/// them for the image instead.
fn can_set_any_owner() -> bool {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return false;
    }
    let full = |path: &str| std::fs::read_to_string(path).is_ok_and(|map| maps_every_id(&map));
    full("/proc/self/uid_map") && full("/proc/self/gid_map")
}

/// A `uid_map`/`gid_map` that maps IDs 0..2^32-1 onto themselves.
fn maps_every_id(map: &str) -> bool {
    let fields: Vec<&str> = map.split_whitespace().collect();
    fields == ["0", "0", "4294967295"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_references_select_a_local_engine() {
        assert_eq!(
            engine_image_source("docker-daemon:python:3.12-slim"),
            Some(("docker", "python:3.12-slim"))
        );
        assert_eq!(
            engine_image_source("podman:ghcr.io/o/i:t"),
            Some(("podman", "ghcr.io/o/i:t"))
        );
        assert_eq!(engine_image_source("docker://alpine:3.20"), None);
    }

    #[test]
    fn only_a_full_id_mapping_counts_as_able_to_set_any_owner() {
        assert!(maps_every_id("         0          0 4294967295\n"));
        assert!(
            !maps_every_id("         0       1000          1\n"),
            "unshare -Ur"
        );
        assert!(!maps_every_id("0 0 65536\n"));
        assert!(!maps_every_id("0 0 4294967295\n1 2 3\n"));
        assert!(!maps_every_id(""));
    }

    #[test]
    fn rootless_unpack_records_real_metadata_and_stages_a_usable_tree() {
        use std::os::unix::fs::PermissionsExt;

        let mut builder = tar::Builder::new(Vec::new());
        let mut add = |path: &str, kind: tar::EntryType, mode: u32, data: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_mode(mode);
            header.set_uid(0);
            header.set_gid(42);
            header.set_size(data.len() as u64);
            if kind == tar::EntryType::Char {
                header.set_device_major(1).unwrap();
                header.set_device_minor(3).unwrap();
            }
            builder.append_data(&mut header, path, data).unwrap();
        };
        add("ro", tar::EntryType::Directory, 0o555, b"");
        add("ro/secret", tar::EntryType::Regular, 0o000, b"s3cret");
        add("passwd", tar::EntryType::Regular, 0o4755, b"bin");
        add("null", tar::EntryType::Char, 0o666, b"");
        let tarball = builder.into_inner().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let limits = test_limits(1 << 20, 64);
        let meta = unpack_export(&tarball[..], dir.path(), limits, true).unwrap();

        let entry = |path: &str| &meta.inodes[&meta.paths[Path::new(path)]];
        let secret = entry("ro/secret");
        assert_eq!(
            (secret.uid, secret.gid, secret.mode),
            (0, 42, image_mode(libc::S_IFREG))
        );
        assert_eq!(entry("passwd").mode, image_mode(libc::S_IFREG) | 0o4755);
        assert_eq!(entry("ro").mode, image_mode(libc::S_IFDIR) | 0o555);
        assert_eq!(meta.special[Path::new("null")].device, Some((1, 3)));
        // The staged copy is readable by the build user and the device node
        // is left for debugfs.
        assert_eq!(
            std::fs::read(dir.path().join("ro/secret")).unwrap(),
            b"s3cret"
        );
        let staged = std::fs::metadata(dir.path().join("passwd")).unwrap();
        assert_eq!(
            staged.permissions().mode() & 0o7000,
            0,
            "no setuid on the host"
        );
        assert!(!dir.path().join("null").exists());
    }

    #[test]
    fn rootless_hardlinks_keep_the_exported_inode_owner() {
        use std::os::unix::fs::MetadataExt;

        let mut builder = tar::Builder::new(Vec::new());
        let mut original = tar::Header::new_gnu();
        original.set_entry_type(tar::EntryType::Regular);
        original.set_mode(0o640);
        original.set_uid(1000);
        original.set_gid(1001);
        original.set_size(4);
        builder
            .append_data(&mut original, "original", &b"data"[..])
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Link);
        // A hardlink header's owner/mode are not the shared inode metadata.
        link.set_mode(0o777);
        link.set_uid(0);
        link.set_gid(0);
        link.set_size(0);
        link.set_link_name("original").unwrap();
        builder.append_data(&mut link, "alias", &b""[..]).unwrap();
        let tarball = builder.into_inner().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let metadata =
            unpack_export(&tarball[..], dir.path(), test_limits(1 << 20, 64), true).unwrap();

        assert_eq!(
            std::fs::metadata(dir.path().join("original"))
                .unwrap()
                .ino(),
            std::fs::metadata(dir.path().join("alias")).unwrap().ino(),
        );
        let script = image_ownership_script(dir.path(), &metadata).unwrap();
        assert!(script.contains("sif \"/alias\" uid 1000\n"));
        assert!(script.contains("sif \"/alias\" gid 1001\n"));
        assert!(script.contains("sif \"/alias\" mode 0100640\n"));
        assert!(
            !script.contains("/original"),
            "each shared inode must receive metadata only once"
        );
    }

    #[test]
    fn rootless_hardlink_retains_root_owner_after_target_replacement() {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, uid, gid, mode, link) in [
            ("./target", 0, 0, 0o644, None),
            ("alias", 1000, 1001, 0o777, Some("target")),
            ("target", 2000, 2001, 0o600, None),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_uid(uid);
            header.set_gid(gid);
            header.set_mode(mode);
            let data = if let Some(target) = link {
                header.set_entry_type(tar::EntryType::Link);
                header.set_link_name(target).unwrap();
                &b""[..]
            } else {
                header.set_entry_type(tar::EntryType::Regular);
                &b"data"[..]
            };
            header.set_size(data.len() as u64);
            builder.append_data(&mut header, path, data).unwrap();
        }
        let tarball = builder.into_inner().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let metadata =
            unpack_export(&tarball[..], dir.path(), test_limits(1 << 20, 64), true).unwrap();
        assert_ne!(
            metadata.paths[Path::new("alias")],
            metadata.paths[Path::new("target")]
        );
        let script = image_ownership_script(dir.path(), &metadata).unwrap();
        assert!(script.contains("sif \"/alias\" uid 0\n"));
        assert!(script.contains("sif \"/alias\" gid 0\n"));
        assert!(script.contains("sif \"/alias\" mode 0100644\n"));
        assert!(script.contains("sif \"/target\" uid 2000\n"));
        assert!(script.contains("sif \"/target\" gid 2001\n"));
        assert!(script.contains("sif \"/target\" mode 0100600\n"));

        // A build-injected replacement at an exported pathname is a new
        // inode and must receive root ownership, not the image entry's UID.
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"agent").unwrap();
        std::fs::rename(&replacement, dir.path().join("target")).unwrap();
        let script = image_ownership_script(dir.path(), &metadata).unwrap();
        assert!(script.contains("sif \"/target\" uid 0\nsif \"/target\" gid 0\n"));
        assert!(!script.contains("uid 2000"));
    }

    // Keep the human-readable assertions above focused on inode ownership.
    fn image_ownership_script(
        rootfs: &Path,
        metadata: &ExportMetadata,
    ) -> Result<String, OciError> {
        let plan = image_ownership_plan(rootfs, metadata)?;
        let mut commands = Vec::new();
        let mut seen = HashSet::new();
        for entry in plan.entries {
            if seen.insert(entry.identity) {
                append_inode_ownership(&mut commands, &debugfs_path(&entry.path)?, &entry);
            }
        }
        let mut script = commands.join(&b'\n');
        script.push(b'\n');
        Ok(String::from_utf8(script).unwrap())
    }

    #[test]
    fn debugfs_paths_preserve_unusual_bytes_without_ending_the_token() {
        let path = Path::new(std::ffi::OsStr::from_bytes(
            b"quote\" slash\\ line\n\r\t\x01-\xff",
        ));
        assert_eq!(
            debugfs_path(path).unwrap(),
            b"\"/quote\"\" slash\\ line\n\r\t\x01-\xff\""
        );
        assert!(debugfs_path(Path::new(std::ffi::OsStr::from_bytes(b"bad\0name"))).is_err());
        assert_eq!(
            export_relative_path(Path::new("/./a//b")).unwrap(),
            Path::new("a/b")
        );
        assert!(export_relative_path(Path::new("a/../b")).is_err());
    }

    #[test]
    fn debugfs_directory_parser_preserves_bytes_and_rejects_bad_framing() {
        let name = b"name\n\ndebugfs: ls -p <2>\n# tarit-directory-end\n\r\xff\\\"";
        let mut output = b"debugfs: ls -p <2>\n/12/100644/-1/-2147483648/".to_vec();
        output.extend_from_slice(name);
        output.extend_from_slice(b"/4/\n\n# tarit-directory-end\n");
        let entries = read_debugfs_directory(&mut &output[..], 2, 1).unwrap();
        assert_eq!(entries[0].inode, 12);
        assert_eq!(entries[0].name, name);
        assert!(read_debugfs_directory(&mut &output[..], 2, 0).is_err());
        for malformed in [
            b"debugfs: ls -p <2>\n# tarit-directory-end\n".as_slice(),
            b"debugfs: ls -p <2>\n/4294967296/100644/0/0/a/1/\n\n# tarit-directory-end\n",
            b"debugfs: ls -p <2>\n/12/100644/0/0/partial-name",
            b"debugfs: ls -p <2>\n/12/100644/0/0/a/1/\n\nwrong-end\n",
        ] {
            assert!(read_debugfs_directory(&mut &malformed[..], 2, 4).is_err());
        }
        let mut too_long = b"debugfs: ls -p <2>\n/12/100644/0/0/".to_vec();
        too_long.extend_from_slice(&[b'a'; 256]);
        too_long.extend_from_slice(b"/1/\n\n# tarit-directory-end\n");
        assert!(read_debugfs_directory(&mut &too_long[..], 2, 1).is_err());
    }

    #[test]
    fn rootless_special_process_budget_rejects_before_starting_debugfs() {
        let workspace = tempfile::tempdir().unwrap();
        let rootfs = workspace.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        let mut metadata = ExportMetadata::default();
        for index in 0..MAX_OCI_UNBATCHED_SPECIAL_LINKS {
            metadata.special.insert(
                PathBuf::from(format!("node-{index}\n")),
                EntryMeta {
                    uid: 0,
                    gid: 0,
                    mode: image_mode(libc::S_IFIFO) | 0o600,
                    device: None,
                },
            );
        }
        assert_eq!(
            image_ownership_plan(&rootfs, &metadata)
                .unwrap()
                .special
                .len(),
            MAX_OCI_UNBATCHED_SPECIAL_LINKS
        );
        metadata.special.insert(
            PathBuf::from("one-too-many\r"),
            EntryMeta {
                uid: 0,
                gid: 0,
                mode: image_mode(libc::S_IFIFO) | 0o600,
                device: None,
            },
        );
        let image = workspace.path().join("unpublished.ext4");
        std::fs::write(&image, b"unchanged").unwrap();
        let error =
            apply_image_ownership(&rootfs, &metadata, &image, workspace.path()).unwrap_err();
        assert!(
            matches!(error, OciError::ResourceLimit(message) if message.contains("maximum is 64"))
        );
        assert_eq!(std::fs::read(&image).unwrap(), b"unchanged");
        assert!(!workspace.path().join("ownership.stderr").exists());
        assert!(!workspace.path().join("ownership.debugfs").exists());
    }

    #[test]
    fn rootless_special_collisions_fail_before_mutating_the_image() {
        for (original, destination) in [
            (Some(tar::EntryType::Directory), "node"),
            (Some(tar::EntryType::Regular), "node"),
            (Some(tar::EntryType::Symlink), "node"),
            (None, "lost+found"),
            (Some(tar::EntryType::Symlink), "node/lost+found"),
            (Some(tar::EntryType::Regular), "node/child"),
            (None, "missing/child"),
        ] {
            for special in [tar::EntryType::Char, tar::EntryType::Fifo] {
                let workspace = tempfile::tempdir().unwrap();
                let rootfs = workspace.path().join("rootfs");
                std::fs::create_dir(&rootfs).unwrap();
                let mut builder = tar::Builder::new(Vec::new());
                let entries = original
                    .into_iter()
                    .map(|kind| (kind, "node"))
                    .chain(std::iter::once((special, destination)));
                for (kind, path) in entries {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(kind);
                    header.set_uid(1000);
                    header.set_gid(1001);
                    header.set_mode(0o755);
                    if kind == tar::EntryType::Symlink {
                        // Cover both a dangling final symlink and an image
                        // absolute parent alias to mke2fs's lost+found.
                        let target = if destination == "node" {
                            "missing-target"
                        } else {
                            "/"
                        };
                        header.set_link_name(target).unwrap();
                    }
                    if kind == tar::EntryType::Char {
                        header.set_device_major(1).unwrap();
                        header.set_device_minor(3).unwrap();
                    }
                    let data = if kind == tar::EntryType::Regular {
                        &b"old file"[..]
                    } else {
                        &b""[..]
                    };
                    header.set_size(data.len() as u64);
                    builder.append_data(&mut header, path, data).unwrap();
                }
                let tarball = builder.into_inner().unwrap();
                let metadata =
                    unpack_export(&tarball[..], &rootfs, test_limits(1 << 20, 64), true).unwrap();
                assert_eq!(
                    std::fs::symlink_metadata(rootfs.join("node")).is_ok(),
                    original.is_some()
                );
                let image = workspace.path().join("unpublished.ext4");
                let unchanged = b"image has not been opened or modified";
                std::fs::write(&image, unchanged).unwrap();
                let error = restore_image_ownership(&rootfs, &metadata, &image, workspace.path())
                    .unwrap_err();
                assert!(
                    matches!(error, OciError::UnsafePath(ref message)
                    if message.contains("special inode destination must be absent")),
                    "{original:?} -> {special:?} at {destination}: {error}"
                );
                assert_eq!(std::fs::read(&image).unwrap(), unchanged);
                assert!(!workspace.path().join("ownership.debugfs").exists());
            }
        }
    }

    fn e2fsprogs_available() -> bool {
        let available = ["mke2fs", "debugfs", "e2fsck"]
            .iter()
            .all(|tool| which(tool).is_some());
        if !available {
            assert_ne!(
                std::env::var("TARIT_REQUIRE_E2FSPROGS").as_deref(),
                Ok("1"),
                "TARIT_REQUIRE_E2FSPROGS=1 requires mke2fs, debugfs and e2fsck on PATH"
            );
            eprintln!("skipping ext4 integration test: e2fsprogs unavailable");
        }
        available
    }

    #[test]
    fn rootless_ext4_maps_explicit_empty_lost_found() {
        if !e2fsprogs_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let rootfs = workspace.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("lost+found")).unwrap();
        let image = workspace.path().join("disk.ext4");
        let output = Command::new("mke2fs")
            .args(["-q", "-t", "ext4", "-b", "1024", "-d"])
            .arg(&rootfs)
            .arg("-F")
            .arg(&image)
            .arg("16M")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            apply_image_ownership(
                &rootfs,
                &ExportMetadata::default(),
                &image,
                workspace.path()
            )
            .unwrap(),
            2
        );
        let output = Command::new("e2fsck")
            .args(["-f", "-n"])
            .arg(&image)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn rootless_ext4_reserves_space_for_reordered_special_links() {
        if !e2fsprogs_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let rootfs = workspace.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("devices")).unwrap();
        let mut metadata = ExportMetadata::default();
        for group in 0..10 {
            // In sorted plan order, each group fits in 984 bytes. Execution
            // batches all small ordinary links before the large LF links,
            // whose packing needs more blocks than that plan would reserve.
            for name in (0..3)
                .map(|index| format!("{group:02}-a{index}\n{}", "x".repeat(249)))
                .chain((0..8).map(|index| format!("{group:02}-b{index}-1234567")))
            {
                metadata.special.insert(
                    Path::new("devices").join(name),
                    EntryMeta {
                        uid: 123,
                        gid: 456,
                        mode: image_mode(libc::S_IFIFO) | 0o640,
                        device: None,
                    },
                );
            }
        }
        let image = workspace.path().join("disk.ext4");
        let output = Command::new("mke2fs")
            .args(["-q", "-t", "ext4", "-b", "1024", "-d"])
            .arg(&rootfs)
            .arg("-F")
            .arg(&image)
            .arg("16M")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            apply_image_ownership(&rootfs, &metadata, &image, workspace.path()).unwrap(),
            34
        );
        for path in metadata.special.keys() {
            let request = debugfs_request("stat", &debugfs_path(path).unwrap(), "");
            let output = Command::new("debugfs")
                .arg("-R")
                .arg(std::ffi::OsStr::from_bytes(&request))
                .arg(&image)
                .output()
                .unwrap();
            check_debugfs_output(output.status, &output.stderr).unwrap();
            let output = String::from_utf8(output.stdout).unwrap();
            let words: Vec<_> = output.split_whitespace().collect();
            let value = |key: &str| words[words.iter().position(|word| *word == key).unwrap() + 1];
            assert_eq!(value("Type:"), "FIFO", "{path:?}");
            assert_eq!(value("Mode:"), "0640", "{path:?}");
            assert_eq!(value("User:"), "123", "{path:?}");
            assert_eq!(value("Group:"), "456", "{path:?}");
        }
        let output = Command::new("e2fsck")
            .args(["-f", "-n"])
            .arg(&image)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Runs in required Linux CI with real e2fsprogs. Developers without the
    /// tools can run the pure unit tests; the required flag forbids a skip.
    #[test]
    fn rootless_ext4_preserves_unusual_names_owners_and_special_inodes() {
        if !e2fsprogs_available() {
            return;
        }

        let workspace = tempfile::tempdir().unwrap();
        let rootfs = workspace.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::set_permissions(&rootfs, std::fs::Permissions::from_mode(0o755)).unwrap();
        let directory = Path::new("odd\"\\\n\r\t\x01");
        let target = directory.join("target\"\n");
        let alias = directory.join("alias\\\n");
        let injected = directory.join("injected\"\n");
        let symlink = directory.join("symlink\"\n");
        let mut special = vec![
            (
                directory.join("null\"\n"),
                tar::EntryType::Char,
                "character special",
            ),
            (
                directory.join("disk\\\r"),
                tar::EntryType::Block,
                "block special",
            ),
            (directory.join("fifo\t"), tar::EntryType::Fifo, "FIFO"),
            (
                PathBuf::from(std::ffi::OsStr::from_bytes(b"raw-\xff")),
                tar::EntryType::Char,
                "character special",
            ),
            (
                PathBuf::from(".tarit-oci-node-1"),
                tar::EntryType::Char,
                "character special",
            ),
        ];
        for index in 0..128 {
            special.push((
                PathBuf::from(format!("devices/node-{index:03}")),
                tar::EntryType::Char,
                "character special",
            ));
        }
        // Maximum-length basenames require multiple small directory blocks;
        // the reservation must account for per-block packing loss.
        for index in 0..11 {
            special.push((
                PathBuf::from(format!("devices/long-{index:02}-{}", "x".repeat(247))),
                tar::EntryType::Char,
                "character special",
            ));
        }
        // The three special paths under `directory` are already exceptional.
        for index in 0..MAX_OCI_UNBATCHED_SPECIAL_LINKS - 3 {
            special.push((
                PathBuf::from(format!("devices/odd-{index:03}\n")),
                tar::EntryType::Fifo,
                "FIFO",
            ));
        }
        // Doubling quotes makes this exceed debugfs's 1024-byte Darwin line
        // buffer while the host path remains short enough for APFS.
        let mut long_path = PathBuf::new();
        for _ in 0..4 {
            long_path.push("\"".repeat(150));
        }
        long_path.push("leaf");
        let mut ordinary = vec![
            (PathBuf::from("aaa"), 501, 502),
            (PathBuf::from("paired\"name"), 503, 504),
            (PathBuf::from("pairedname"), 505, 506),
            (PathBuf::from("back\\slash\t\x01"), 507, 508),
            // If this ever enters a line-oriented script unquoted, it would
            // change the already-restored aaa inode instead of this file.
            (PathBuf::from("attack\"\nsif /aaa uid 123\n#"), 509, 510),
            (long_path, 511, 512),
        ];
        let mut bulk = Vec::new();
        for index in 0..64 {
            let path = PathBuf::from(format!("bulk-{index:03}\"\\\n\r"));
            let file = path.join("file\n\ndebugfs: ls -p <2>\n# tarit-directory-end");
            ordinary.push((file.clone(), 10000 + index, 20000 + index));
            bulk.push((path.clone(), file, path.join("link\"\n"), index));
        }
        let mut builder = tar::Builder::new(Vec::new());
        let mut add = |path: &Path,
                       kind: tar::EntryType,
                       owner: (u64, u64),
                       mode: u32,
                       link: Option<&Path>| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_uid(owner.0);
            header.set_gid(owner.1);
            header.set_mode(mode);
            if let Some(link) = link {
                header.set_link_name(link).unwrap();
            }
            if kind == tar::EntryType::Char || kind == tar::EntryType::Block {
                header.set_device_major(1).unwrap();
                header.set_device_minor(3).unwrap();
            }
            let data = if kind == tar::EntryType::Regular {
                &b"data"[..]
            } else {
                &b""[..]
            };
            header.set_size(data.len() as u64);
            builder.append_data(&mut header, path, data).unwrap();
        };
        add(
            directory,
            tar::EntryType::Directory,
            (600, 601),
            0o751,
            None,
        );
        for (path, uid, gid) in &ordinary {
            add(path, tar::EntryType::Regular, (*uid, *gid), 0o640, None);
        }
        for (directory, file, link, index) in &bulk {
            add(
                directory,
                tar::EntryType::Directory,
                (10000 + index, 20000 + index),
                0o751,
                None,
            );
            add(
                link,
                tar::EntryType::Symlink,
                (10000 + index, 20000 + index),
                0o777,
                Some(Path::new(file.file_name().unwrap())),
            );
        }
        add(
            Path::new("devices"),
            tar::EntryType::Directory,
            (800, 801),
            0o755,
            None,
        );
        add(&target, tar::EntryType::Regular, (0, 42), 0o4755, None);
        add(
            &alias,
            tar::EntryType::Link,
            (999, 999),
            0o777,
            Some(&target),
        );
        add(&target, tar::EntryType::Regular, (2000, 2001), 0o600, None);
        add(
            &injected,
            tar::EntryType::Regular,
            (3000, 3001),
            0o600,
            None,
        );
        add(
            &symlink,
            tar::EntryType::Symlink,
            (61000, 61001),
            0o777,
            Some(Path::new(target.file_name().unwrap())),
        );
        // Reserve the first temporary-name candidate with an exported file.
        add(
            Path::new(".tarit-oci-node-0"),
            tar::EntryType::Regular,
            (700, 701),
            0o640,
            None,
        );
        for (path, kind, _) in &special {
            add(path, *kind, (800, 801), 0o660, None);
        }
        // APFS rejects invalid UTF-8 before mke2fs; Linux staging filesystems
        // support these byte names. Special nodes above test raw bytes on both.
        #[cfg(target_os = "linux")]
        let raw_file = PathBuf::from(std::ffi::OsStr::from_bytes(b"raw-dir-\xff/regular-\xfe"));
        #[cfg(target_os = "linux")]
        add(&raw_file, tar::EntryType::Regular, (900, 901), 0o640, None);
        let tarball = builder.into_inner().unwrap();
        let mut limits = test_limits(1 << 20, 512);
        limits.max_path_bytes = 4096;
        let metadata = unpack_export(&tarball[..], &rootfs, limits, true).unwrap();
        let replacement = workspace.path().join("new-agent");
        std::fs::write(&replacement, b"agent").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::rename(&replacement, rootfs.join(&injected)).unwrap();
        let image = workspace.path().join("disk.ext4");
        let output = Command::new("mke2fs")
            .args(["-q", "-t", "ext4", "-b", "1024", "-d"])
            .arg(&rootfs)
            .arg("-F")
            .arg(&image)
            .arg("16M")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let processes =
            apply_image_ownership(&rootfs, &metadata, &image, workspace.path()).unwrap();
        assert_eq!(processes, 4 + MAX_OCI_UNBATCHED_SPECIAL_LINKS,
            "one mapper, one allocation batch, one link/expansion batch, one metadata batch, and one process per exceptional special link");

        let stat = |path: &Path| -> String {
            let request = debugfs_request("stat", &debugfs_path(path).unwrap(), "");
            let output = Command::new("debugfs")
                .arg("-R")
                .arg(std::ffi::OsStr::from_bytes(&request))
                .arg(&image)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8_lossy(&output.stderr).lines().count(),
                1,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        let check = |path: &Path, uid: u64, gid: u64, mode: u32, kind: &str| -> u64 {
            let output = stat(path);
            let words: Vec<_> = output.split_whitespace().collect();
            let value = |key: &str| words[words.iter().position(|word| *word == key).unwrap() + 1];
            assert_eq!(
                value("User:").parse::<u64>().unwrap(),
                uid,
                "{path:?}: {output}"
            );
            assert_eq!(
                value("Group:").parse::<u64>().unwrap(),
                gid,
                "{path:?}: {output}"
            );
            assert_eq!(
                u32::from_str_radix(value("Mode:"), 8).unwrap(),
                mode,
                "{path:?}: {output}"
            );
            assert!(
                output
                    .lines()
                    .next()
                    .unwrap()
                    .contains(&format!("Type: {kind}")),
                "{path:?}: {output}"
            );
            value("Inode:").parse().unwrap()
        };
        check(Path::new(""), 0, 0, 0o755, "directory");
        check(directory, 600, 601, 0o751, "directory");
        for (path, uid, gid) in &ordinary {
            check(path, *uid, *gid, 0o640, "regular");
        }
        for (directory, _, link, index) in &bulk {
            check(directory, 10000 + index, 20000 + index, 0o751, "directory");
            check(link, 10000 + index, 20000 + index, 0o777, "symlink");
        }
        let alias_inode = check(&alias, 0, 42, 0o4755, "regular");
        let target_inode = check(&target, 2000, 2001, 0o600, "regular");
        assert_ne!(
            alias_inode, target_inode,
            "replacement must not change the old hardlink's owner"
        );
        check(&injected, 0, 0, 0o644, "regular");
        check(&symlink, 61000, 61001, 0o777, "symlink");
        check(Path::new(".tarit-oci-node-0"), 700, 701, 0o640, "regular");
        for (path, kind, type_name) in &special {
            check(path, 800, 801, 0o660, type_name);
            let output = stat(path);
            assert!(output.contains("Links: 1"), "{output}");
            if *kind != tar::EntryType::Fifo {
                assert!(
                    output.contains("Device major/minor number: 01:03"),
                    "{output}"
                );
            }
        }
        #[cfg(target_os = "linux")]
        check(&raw_file, 900, 901, 0o640, "regular");
        let output = Command::new("e2fsck")
            .args(["-fn"])
            .arg(&image)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn test_limits(max_expanded_bytes: u64, max_entries: u64) -> OciResourceLimits {
        OciResourceLimits {
            max_layers: 4,
            max_compressed_bytes: 1024 * 1024,
            max_expanded_bytes,
            max_entries,
            max_single_file_bytes: 1024 * 1024,
            max_path_bytes: 256,
        }
    }

    fn tar_layer(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(contents.len() as u64);
            header.set_cksum();
            builder.append_data(&mut header, path, *contents).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn write_blob(root: &Path, media_type: &str, bytes: &[u8]) -> OciDescriptor {
        let digest = format!("{:x}", Sha256::digest(bytes));
        let blob_dir = root.join("blobs/sha256");
        std::fs::create_dir_all(&blob_dir).unwrap();
        std::fs::write(blob_dir.join(&digest), bytes).unwrap();
        OciDescriptor {
            media_type: media_type.into(),
            digest: format!("sha256:{digest}"),
            size: bytes.len() as u64,
            annotations: HashMap::new(),
        }
    }

    fn write_layout(root: &Path, layers: Vec<(&str, Vec<u8>)>) {
        let layer_descriptors = layers
            .iter()
            .map(|(media_type, bytes)| write_blob(root, media_type, bytes))
            .collect();
        let config = write_blob(
            root,
            "application/vnd.oci.image.config.v1+json",
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#,
        );
        let manifest = serde_json::to_vec(&OciManifest {
            config,
            layers: layer_descriptors,
        })
        .unwrap();
        let mut descriptor = write_blob(
            root,
            "application/vnd.oci.image.manifest.v1+json",
            &manifest,
        );
        descriptor
            .annotations
            .insert("org.opencontainers.image.ref.name".into(), "default".into());
        std::fs::write(
            root.join("index.json"),
            serde_json::to_vec(&OciIndex {
                manifests: vec![descriptor],
            })
            .unwrap(),
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn test_agent(dir: &Path) -> PathBuf {
        let path = dir.join("vmm-agent");
        std::fs::write(&path, b"test-agent").unwrap();
        path
    }

    #[test]
    fn oci_image_ref_serializes() {
        let r = OciImageRef {
            reference: "docker://ubuntu:22.04".into(),
            auth_file: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("docker://ubuntu:22.04"));
    }

    #[test]
    fn oci_preflight_accepts_digest_verified_gzip_and_zstd_layers() {
        let temp = tempfile::tempdir().unwrap();
        let first = tar_layer(&[("etc/one", b"one"), ("etc/two", b"two")]);
        let second = tar_layer(&[("usr/bin/tool", b"tool")]);
        write_layout(
            temp.path(),
            vec![
                ("application/vnd.oci.image.layer.v1.tar+gzip", gzip(&first)),
                (
                    "application/vnd.oci.image.layer.v1.tar+zstd",
                    zstd::stream::encode_all(second.as_slice(), 1).unwrap(),
                ),
            ],
        );

        let stats =
            preflight_oci_layout(temp.path(), "default", test_limits(64 * 1024, 10)).unwrap();

        assert_eq!(stats.layers, 2);
        assert_eq!(stats.entries, 3);
        assert!(stats.compressed_bytes > 0);
        assert!(stats.expanded_bytes > 0);
    }

    #[test]
    fn oci_resource_limits_reject_zero_and_overflowing_disk_sizes() {
        assert!(matches!(
            OciResourceLimits::for_disk_size(0),
            Err(OciError::ResourceLimit(_))
        ));
        assert!(matches!(
            OciResourceLimits::for_disk_size(u64::MAX),
            Err(OciError::ResourceLimit(_))
        ));
    }

    #[test]
    fn oci_preflight_rejects_inode_exhaustion_before_unpack() {
        let temp = tempfile::tempdir().unwrap();
        let layer = tar_layer(&[("a", b""), ("b", b""), ("c", b"")]);
        write_layout(
            temp.path(),
            vec![("application/vnd.oci.image.layer.v1.tar+gzip", gzip(&layer))],
        );

        let error =
            preflight_oci_layout(temp.path(), "default", test_limits(64 * 1024, 2)).unwrap_err();

        assert!(matches!(error, OciError::ResourceLimit(_)));
        assert!(error.to_string().contains("entries exceed limit"));
    }

    #[test]
    fn oci_preflight_rejects_expansion_bomb_before_unpack() {
        let temp = tempfile::tempdir().unwrap();
        let payload = vec![0u8; 16 * 1024];
        let layer = tar_layer(&[("large-zero-file", &payload)]);
        let compressed = gzip(&layer);
        assert!(compressed.len() < 1024);
        write_layout(
            temp.path(),
            vec![("application/vnd.oci.image.layer.v1.tar+gzip", compressed)],
        );

        let error =
            preflight_oci_layout(temp.path(), "default", test_limits(4096, 10)).unwrap_err();

        assert!(matches!(error, OciError::ResourceLimit(_)));
        assert!(error.to_string().contains("expanded layers exceed limit"));
    }

    #[test]
    fn oci_preflight_scans_every_gzip_member() {
        let temp = tempfile::tempdir().unwrap();
        let payload = vec![0x41u8; 16 * 1024];
        let layer = tar_layer(&[("etc/one", b"one"), ("large-file", &payload)]);
        // The first member ends exactly after the first entry, so a decoder
        // that stops after one member sees a small, well-formed archive.
        let mut blob = gzip(&layer[..1024]);
        blob.extend_from_slice(&gzip(&layer[1024..]));
        write_layout(
            temp.path(),
            vec![("application/vnd.oci.image.layer.v1.tar+gzip", blob)],
        );

        let stats =
            preflight_oci_layout(temp.path(), "default", test_limits(64 * 1024, 10)).unwrap();
        assert_eq!(stats.entries, 2);
        assert!(stats.expanded_bytes >= layer.len() as u64);

        let error =
            preflight_oci_layout(temp.path(), "default", test_limits(4096, 10)).unwrap_err();
        assert!(matches!(error, OciError::ResourceLimit(_)));
        assert!(error.to_string().contains("expanded layers exceed limit"));
    }

    #[test]
    fn oci_preflight_allows_zero_padding_but_rejects_data_after_tar_end() {
        let mut padded = tar_layer(&[("etc/one", b"one")]);
        padded.resize(10 * 1024, 0);
        let temp = tempfile::tempdir().unwrap();
        write_layout(
            temp.path(),
            vec![("application/vnd.oci.image.layer.v1.tar+gzip", gzip(&padded))],
        );
        let stats =
            preflight_oci_layout(temp.path(), "default", test_limits(64 * 1024, 10)).unwrap();
        assert_eq!(stats.entries, 1);

        let mut smuggled = gzip(&padded);
        smuggled.extend_from_slice(&gzip(&tar_layer(&[("etc/two", b"two")])));
        let temp = tempfile::tempdir().unwrap();
        write_layout(
            temp.path(),
            vec![("application/vnd.oci.image.layer.v1.tar+gzip", smuggled)],
        );
        let error =
            preflight_oci_layout(temp.path(), "default", test_limits(64 * 1024, 10)).unwrap_err();
        assert!(matches!(error, OciError::InvalidLayout(_)));
        assert!(error.to_string().contains("after the tar end-of-archive"));
    }

    #[test]
    fn oci_preflight_rejects_sparse_entries() {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::GNUSparse);
        header.set_mode(0o644);
        header.set_size(512);
        let gnu = header.as_gnu_mut().unwrap();
        gnu.set_real_size(512 * 1024);
        gnu.sparse[0].set_offset(512 * 1024 - 512);
        gnu.sparse[0].set_length(512);
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_data(&mut header, "var/gnu-sparse", &[0x5a; 512][..])
            .unwrap();
        let gnu_sparse = builder.into_inner().unwrap();

        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_pax_extensions([
                ("GNU.sparse.major", &b"1"[..]),
                ("GNU.sparse.minor", &b"0"[..]),
                ("GNU.sparse.realsize", &b"524288"[..]),
            ])
            .unwrap();
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o644);
        header.set_size(512);
        builder
            .append_data(&mut header, "var/pax-sparse", &[0u8; 512][..])
            .unwrap();
        let pax_sparse = builder.into_inner().unwrap();

        for layer in [gnu_sparse, pax_sparse] {
            let temp = tempfile::tempdir().unwrap();
            write_layout(
                temp.path(),
                vec![("application/vnd.oci.image.layer.v1.tar", layer)],
            );
            let error = preflight_oci_layout(temp.path(), "default", test_limits(64 * 1024, 10))
                .unwrap_err();
            assert!(matches!(error, OciError::InvalidLayout(_)));
            assert!(error.to_string().contains("sparse"));
        }
    }

    #[test]
    fn oci_preflight_rejects_blob_changed_after_descriptor() {
        let temp = tempfile::tempdir().unwrap();
        let layer = tar_layer(&[("proof", b"original")]);
        write_layout(
            temp.path(),
            vec![("application/vnd.oci.image.layer.v1.tar", layer)],
        );
        let index: OciIndex =
            read_bounded_json(&temp.path().join("index.json"), MAX_OCI_JSON_BYTES).unwrap();
        let manifest_path = verify_descriptor_blob(temp.path(), &index.manifests[0]).unwrap();
        let manifest: OciManifest = read_bounded_json(&manifest_path, MAX_OCI_JSON_BYTES).unwrap();
        let layer_path = verify_descriptor_blob(temp.path(), &manifest.layers[0]).unwrap();
        let mut bytes = std::fs::read(&layer_path).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&layer_path, bytes).unwrap();

        let error =
            preflight_oci_layout(temp.path(), "default", test_limits(64 * 1024, 10)).unwrap_err();

        assert!(matches!(error, OciError::InvalidLayout(_)));
        assert!(error.to_string().contains("digest mismatch"));
    }

    #[test]
    fn which_finds_known_commands() {
        // `ls` should always be available.
        assert!(which("ls").is_some());
        assert!(which("nonexistent_tool_12345").is_none());
    }

    #[test]
    fn oci_workspaces_are_private_and_unique() {
        let parent = tempfile::tempdir().unwrap();
        let first = create_oci_workspace(parent.path()).unwrap();
        let second = create_oci_workspace(parent.path()).unwrap();
        assert_ne!(first.path(), second.path());
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(first.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_injection_uses_fd_relative_paths_and_supports_usrmerge() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        std::os::unix::fs::symlink("bin", rootfs.join("usr/sbin")).unwrap();
        std::os::unix::fs::symlink("usr/sbin", rootfs.join("sbin")).unwrap();
        let agent = test_agent(temp.path());

        inject_agent_init(&rootfs, &agent).unwrap();

        assert_eq!(
            std::fs::read(rootfs.join("usr/sbin/vmm-agent")).unwrap(),
            b"test-agent"
        );
        let mode = std::fs::metadata(rootfs.join("usr/sbin/vmm-agent"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(
            std::fs::read_link(rootfs.join("usr/sbin/init")).unwrap(),
            Path::new("/usr/sbin/vmm-agent")
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_injection_rejects_image_controlled_parent_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, rootfs.join("usr")).unwrap();
        let agent = test_agent(temp.path());

        let error = inject_agent_init(&rootfs, &agent).unwrap_err();
        assert!(matches!(error, OciError::UnsafePath(_)));
        assert!(!outside.join("sbin/vmm-agent").exists());
    }

    #[cfg(unix)]
    #[test]
    fn agent_injection_replaces_leaf_symlink_without_following_it() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let destination = rootfs.join("usr/sbin");
        std::fs::create_dir_all(&destination).unwrap();
        let victim = temp.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        std::os::unix::fs::symlink(&victim, destination.join("vmm-agent")).unwrap();
        let agent = test_agent(temp.path());

        inject_agent_init(&rootfs, &agent).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        assert!(std::fs::symlink_metadata(destination.join("vmm-agent"))
            .unwrap()
            .file_type()
            .is_file());
        assert_eq!(
            std::fs::read(destination.join("vmm-agent")).unwrap(),
            b"test-agent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_injection_replaces_existing_image_init_without_following_it() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let sbin = rootfs.join("sbin");
        std::fs::create_dir_all(&sbin).unwrap();
        let victim = temp.path().join("image-init");
        std::fs::write(&victim, b"original-init").unwrap();
        std::os::unix::fs::symlink(&victim, sbin.join("init")).unwrap();
        let agent = test_agent(temp.path());

        inject_agent_init(&rootfs, &agent).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"original-init");
        assert_eq!(
            std::fs::read_link(sbin.join("init")).unwrap(),
            Path::new("/usr/sbin/vmm-agent")
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_injection_rejects_unsafe_sbin_symlink_target() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::os::unix::fs::symlink("../../outside", rootfs.join("sbin")).unwrap();
        let agent = test_agent(temp.path());

        let error = inject_agent_init(&rootfs, &agent).unwrap_err();
        assert!(matches!(error, OciError::UnsafePath(_)));
    }

    #[cfg(unix)]
    #[test]
    fn agent_injection_rejects_unsafe_usr_sbin_symlink_target() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(rootfs.join("usr")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, rootfs.join("usr/sbin")).unwrap();
        std::os::unix::fs::symlink("usr/sbin", rootfs.join("sbin")).unwrap();
        let agent = test_agent(temp.path());

        let error = inject_agent_init(&rootfs, &agent).unwrap_err();
        assert!(matches!(error, OciError::UnsafePath(_)));
        assert!(!outside.join("vmm-agent").exists());
    }

    #[cfg(unix)]
    #[test]
    fn publish_replaces_output_symlink_without_touching_target() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join("staged.ext4");
        let output = temp.path().join("output.ext4");
        let victim = temp.path().join("victim");
        std::fs::write(&staged, b"disk").unwrap();
        std::fs::write(&victim, b"untouched").unwrap();
        std::os::unix::fs::symlink(&victim, &output).unwrap();

        publish_disk_image(&staged, &output).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        assert_eq!(std::fs::read(&output).unwrap(), b"disk");
        assert!(std::fs::symlink_metadata(output)
            .unwrap()
            .file_type()
            .is_file());
    }
}
