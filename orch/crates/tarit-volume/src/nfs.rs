use crate::{
    AccessMode, AttachmentTransport, BlockVolumeProvider, DurabilityClass, LifecycleCapabilities,
    LocalBlockProvider, PlacementConstraint, PreparedBlockAttachment, ProviderProfile,
    ProviderVolume, StorageClass, VolumeCapabilities, VolumeError, MIN_BLOCK_VOLUME_BYTES,
};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfsDialect {
    GenericV4_1,
    AwsEfs,
    AzureFiles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfsSecurityFlavor {
    Sys,
    Krb5,
    Krb5Integrity,
    Krb5Privacy,
}

impl NfsSecurityFlavor {
    pub fn mount_option(self) -> &'static str {
        match self {
            Self::Sys => "sec=sys",
            Self::Krb5 => "sec=krb5",
            Self::Krb5Integrity => "sec=krb5i",
            Self::Krb5Privacy => "sec=krb5p",
        }
    }
}

/// Credential-free mount intent. The orchestrator executes this in its private
/// mount namespace and passes only a pre-mounted jailed path onward. It never
/// serializes provider credentials into a VM record or guest command line.
#[derive(Clone, PartialEq, Eq)]
pub struct NfsMountSpec {
    pub volume_id: Uuid,
    pub generation: u64,
    pub read_only: bool,
    pub fs_type: &'static str,
    pub source: String,
    pub options: Vec<String>,
}

impl fmt::Debug for NfsMountSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NfsMountSpec")
            .field("volume_id", &self.volume_id)
            .field("generation", &self.generation)
            .field("read_only", &self.read_only)
            .field("fs_type", &self.fs_type)
            .field("source", &"<private-provider-endpoint>")
            .field("options", &self.options)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct NfsProvider {
    dialect: NfsDialect,
    security: NfsSecurityFlavor,
    endpoint: String,
    export: String,
    region: Option<String>,
    zone: Option<String>,
}

/// A shared NFS export used as durable backing storage for private raw block
/// files. The export is mounted only while Tarit creates, opens, or removes a
/// backing file. The resulting file descriptor is passed to virtio-blk, so the
/// guest sees block semantics rather than a filesystem transport Tarit does not
/// implement.
#[derive(Debug, Clone)]
pub struct NfsBackedBlockProvider {
    nfs: NfsProvider,
    mounter: SystemNfsMounter,
    max_size_bytes: u64,
    operation_timeout: Duration,
}

pub struct PreparedFilesystemAttachment {
    pub volume_id: Uuid,
    pub generation: u64,
    pub read_only: bool,
    pub directory: File,
    private_path: PathBuf,
}

impl fmt::Debug for PreparedFilesystemAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedFilesystemAttachment")
            .field("volume_id", &self.volume_id)
            .field("generation", &self.generation)
            .field("read_only", &self.read_only)
            .field("private_path", &"<private>")
            .finish()
    }
}

impl PreparedFilesystemAttachment {
    pub fn private_path(&self) -> &Path {
        &self.private_path
    }
}

/// Privileged host-side NFS mount executor. It invokes fixed, root-owned
/// helpers without a shell, enforces a deadline, verifies the resulting kernel
/// mount table entry, and returns an already-open directory descriptor.
#[derive(Debug, Clone)]
pub struct SystemNfsMounter {
    root: PathBuf,
    mount_helper: PathBuf,
    unmount_helper: PathBuf,
}

impl SystemNfsMounter {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, VolumeError> {
        let root = root.into();
        crate::ensure_private_root(&root)?;
        let mount_helper = PathBuf::from("/usr/bin/mount");
        let unmount_helper = PathBuf::from("/usr/bin/umount");
        validate_root_helper(&mount_helper)?;
        validate_root_helper(&unmount_helper)?;
        Ok(Self {
            root,
            mount_helper,
            unmount_helper,
        })
    }

    fn target(&self, spec: &NfsMountSpec) -> PathBuf {
        self.root
            .join(format!("{}-{}", spec.volume_id, spec.generation))
    }

    pub fn mount(
        &self,
        spec: &NfsMountSpec,
        timeout: Duration,
    ) -> Result<PreparedFilesystemAttachment, VolumeError> {
        if timeout.is_zero() {
            return Err(VolumeError::Invalid(
                "NFS mount timeout must be positive".into(),
            ));
        }
        let target = self.target(spec);
        if let Some(entry) = find_mount(&target)? {
            verify_mount(spec, &entry)?;
            return open_prepared(spec, target);
        }
        match fs::create_dir(&target) {
            Ok(()) => fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&target)?;
                if !metadata.file_type().is_dir()
                    || metadata.file_type().is_symlink()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.mode() & 0o077 != 0
                    || fs::read_dir(&target)?.next().is_some()
                {
                    return Err(VolumeError::UnsafeObject);
                }
            }
            Err(error) => return Err(error.into()),
        }
        let mut command = Command::new(&self.mount_helper);
        command
            .arg("-t")
            .arg(spec.fs_type)
            .arg("-o")
            .arg(spec.options.join(","))
            .arg("--")
            .arg(&spec.source)
            .arg(&target);
        let status = run_bounded(&mut command, timeout);
        match status {
            Ok(status) if status.success() => {}
            Ok(_) => {
                let _ = fs::remove_dir(&target);
                return Err(VolumeError::Io(io::Error::other("NFS mount helper failed")));
            }
            Err(error) => {
                let _ = fs::remove_dir(&target);
                return Err(error);
            }
        }
        let entry = find_mount(&target)?.ok_or_else(|| {
            VolumeError::Io(io::Error::other(
                "NFS helper succeeded without the expected mount",
            ))
        })?;
        if let Err(error) = verify_mount(spec, &entry) {
            let _ = self.unmount_target(&target, timeout);
            return Err(error);
        }
        open_prepared(spec, target)
    }

    pub fn unmount(
        &self,
        attachment: PreparedFilesystemAttachment,
        timeout: Duration,
    ) -> Result<(), VolumeError> {
        let expected = self.root.join(format!(
            "{}-{}",
            attachment.volume_id, attachment.generation
        ));
        if attachment.private_path != expected {
            return Err(VolumeError::IdentityMismatch);
        }
        // Our directory descriptor is itself a mount reference. Close it
        // before asking the kernel whether any *external* user keeps the mount
        // busy; otherwise every valid detach would self-report EBUSY.
        drop(attachment);
        self.unmount_target(&expected, timeout)?;
        fs::remove_dir(&expected)?;
        crate::sync_directory(&self.root)?;
        Ok(())
    }

    /// Detach a private mount after opening a block backing file from it. The
    /// open file descriptor deliberately keeps the NFS superblock alive for
    /// the VMM, while the mount path immediately disappears from the worker's
    /// namespace. This is not used for filesystem attachments, whose busy
    /// detach must remain observable and fail closed.
    fn detach_open_file(
        &self,
        attachment: PreparedFilesystemAttachment,
        timeout: Duration,
    ) -> Result<(), VolumeError> {
        if timeout.is_zero() {
            return Err(VolumeError::Invalid(
                "NFS detach timeout must be positive".into(),
            ));
        }
        let expected = self.root.join(format!(
            "{}-{}",
            attachment.volume_id, attachment.generation
        ));
        if attachment.private_path != expected {
            return Err(VolumeError::IdentityMismatch);
        }
        drop(attachment);
        let mut command = Command::new(&self.unmount_helper);
        command.arg("--lazy").arg("--").arg(&expected);
        let status = run_bounded(&mut command, timeout)?;
        if !status.success() || find_mount(&expected)?.is_some() {
            return Err(VolumeError::Busy);
        }
        fs::remove_dir(&expected)?;
        crate::sync_directory(&self.root)?;
        Ok(())
    }

    fn unmount_target(&self, target: &Path, timeout: Duration) -> Result<(), VolumeError> {
        if timeout.is_zero() {
            return Err(VolumeError::Invalid(
                "NFS unmount timeout must be positive".into(),
            ));
        }
        let mut command = Command::new(&self.unmount_helper);
        command.arg("--").arg(target);
        match run_bounded(&mut command, timeout)? {
            status if status.success() => Ok(()),
            _ if find_mount(target)?.is_some() => Err(VolumeError::Busy),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountEntry {
    mount_point: PathBuf,
    fs_type: String,
    source: String,
    mount_options: Vec<String>,
    super_options: Vec<String>,
}

fn run_bounded(command: &mut Command, timeout: Duration) -> Result<ExitStatus, VolumeError> {
    let mut child = command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(VolumeError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn validate_root_helper(path: &Path) -> Result<(), VolumeError> {
    let metadata = fs::symlink_metadata(path).map_err(VolumeError::Io)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
    {
        return Err(VolumeError::UnsafeObject);
    }
    Ok(())
}

fn open_prepared(
    spec: &NfsMountSpec,
    target: PathBuf,
) -> Result<PreparedFilesystemAttachment, VolumeError> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&target)?;
    Ok(PreparedFilesystemAttachment {
        volume_id: spec.volume_id,
        generation: spec.generation,
        read_only: spec.read_only,
        directory,
        private_path: target,
    })
}

fn find_mount(target: &Path) -> Result<Option<MountEntry>, VolumeError> {
    let contents = fs::read_to_string("/proc/self/mountinfo")?;
    Ok(parse_mountinfo(&contents)
        .into_iter()
        .find(|entry| entry.mount_point == target))
}

fn parse_mountinfo(contents: &str) -> Vec<MountEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let mount_point = before.split_whitespace().nth(4)?;
            let mount_options = before.split_whitespace().nth(5)?;
            let mut after = after.split_whitespace();
            let fs_type = after.next()?;
            let source = after.next()?;
            let super_options = after.next()?;
            Some(MountEntry {
                mount_point: PathBuf::from(decode_mount_field(mount_point)),
                fs_type: fs_type.to_string(),
                source: decode_mount_field(source),
                mount_options: mount_options.split(',').map(str::to_string).collect(),
                super_options: super_options.split(',').map(str::to_string).collect(),
            })
        })
        .collect()
}

fn decode_mount_field(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn verify_mount(spec: &NfsMountSpec, entry: &MountEntry) -> Result<(), VolumeError> {
    let expected_access = if spec.read_only { "ro" } else { "rw" };
    let has_option = |expected: &str| {
        entry
            .mount_options
            .iter()
            .chain(entry.super_options.iter())
            .any(|option| option == expected)
    };
    let required_protocol_options = spec.options.iter().filter(|option| {
        option.as_str() == "hard"
            || option.as_str() == "proto=tcp"
            || option.starts_with("vers=")
            || option.starts_with("minorversion=")
            || option.starts_with("sec=")
    });
    if entry.fs_type != "nfs4"
        || entry.source != spec.source
        || !has_option(expected_access)
        || ["nosuid", "nodev", "noexec"]
            .iter()
            .any(|required| !has_option(required))
        || required_protocol_options
            .into_iter()
            .any(|required| !has_option(required))
    {
        return Err(VolumeError::IdentityMismatch);
    }
    Ok(())
}

impl NfsProvider {
    pub fn new(
        dialect: NfsDialect,
        endpoint: impl Into<String>,
        export: impl Into<String>,
        region: Option<String>,
        zone: Option<String>,
    ) -> Result<Self, VolumeError> {
        let endpoint = endpoint.into();
        let export = export.into();
        validate_endpoint(&endpoint)?;
        validate_export(&export)?;
        match dialect {
            NfsDialect::AwsEfs
                if !endpoint.contains(".efs.") || !endpoint.ends_with(".amazonaws.com") =>
            {
                return Err(VolumeError::Invalid(
                    "AWS EFS endpoint must be an efs.*.amazonaws.com DNS name".into(),
                ));
            }
            NfsDialect::AzureFiles if !endpoint.ends_with(".file.core.windows.net") => {
                return Err(VolumeError::Invalid(
                    "Azure Files endpoint must end in .file.core.windows.net".into(),
                ));
            }
            _ => {}
        }
        Ok(Self {
            dialect,
            security: NfsSecurityFlavor::Sys,
            endpoint,
            export,
            region,
            zone,
        })
    }

    pub fn with_security(mut self, security: NfsSecurityFlavor) -> Result<Self, VolumeError> {
        if self.dialect != NfsDialect::GenericV4_1 && security != NfsSecurityFlavor::Sys {
            return Err(VolumeError::Unsupported(
                "managed NFS transport security requires its provider mount helper".into(),
            ));
        }
        self.security = security;
        Ok(self)
    }

    pub fn provider_name(&self) -> &'static str {
        match self.dialect {
            NfsDialect::GenericV4_1 => "nfs_v4_1",
            NfsDialect::AwsEfs => "aws_efs",
            NfsDialect::AzureFiles => "azure_files_nfs",
        }
    }

    pub fn profile(&self) -> ProviderProfile {
        ProviderProfile {
            capabilities: VolumeCapabilities {
                storage_class: StorageClass::Filesystem,
                read_only_many: true,
                read_write_once: true,
                read_write_many: true,
                snapshots: false,
                clones: false,
            },
            durability: match self.dialect {
                NfsDialect::GenericV4_1 => DurabilityClass::Regional,
                NfsDialect::AwsEfs | NfsDialect::AzureFiles => DurabilityClass::MultiZone,
            },
            transport: AttachmentTransport::PremountedFilesystem,
            lifecycle: LifecycleCapabilities {
                hibernate: true,
                cross_node_resume: true,
                live_migration: true,
                atomic_fork_clone: false,
            },
        }
    }

    pub fn prepare(
        &self,
        volume_id: Uuid,
        generation: u64,
        read_only: bool,
    ) -> Result<NfsMountSpec, VolumeError> {
        if generation == 0 {
            return Err(VolumeError::Invalid(
                "NFS attachment generation must be positive".into(),
            ));
        }
        let mut options = vec![
            "hard".into(),
            "timeo=600".into(),
            "retrans=2".into(),
            "noresvport".into(),
            "proto=tcp".into(),
            "nosuid".into(),
            "nodev".into(),
            "noexec".into(),
        ];
        match self.dialect {
            NfsDialect::GenericV4_1 | NfsDialect::AwsEfs => {
                options.push("vers=4.1".into());
                options.push("rsize=1048576".into());
                options.push("wsize=1048576".into());
                options.push(self.security.mount_option().into());
            }
            NfsDialect::AzureFiles => {
                // Azure documents split major/minor options for distributions
                // that do not accept a dotted `vers` value.
                options.push("vers=4".into());
                options.push("minorversion=1".into());
                options.push("sec=sys".into());
                options.push("nolock".into());
                options.push("nconnect=4".into());
                options.push("rsize=1048576".into());
                options.push("wsize=1048576".into());
                options.push("actimeo=30".into());
            }
        }
        options.push(if read_only { "ro".into() } else { "rw".into() });
        Ok(NfsMountSpec {
            volume_id,
            generation,
            read_only,
            fs_type: "nfs4",
            source: format!("{}:{}", self.endpoint, self.export),
            options,
        })
    }

    pub fn placement(&self) -> (Option<&str>, Option<&str>) {
        (self.region.as_deref(), self.zone.as_deref())
    }
}

impl NfsBackedBlockProvider {
    pub fn open(
        nfs: NfsProvider,
        mount_root: impl Into<PathBuf>,
        max_size_bytes: u64,
        operation_timeout: Duration,
    ) -> Result<Self, VolumeError> {
        if max_size_bytes < MIN_BLOCK_VOLUME_BYTES {
            return Err(VolumeError::Invalid(
                "maximum NFS-backed block volume size is below one MiB".into(),
            ));
        }
        if operation_timeout.is_zero() {
            return Err(VolumeError::Invalid(
                "NFS-backed block operation timeout must be positive".into(),
            ));
        }
        Ok(Self {
            nfs,
            mounter: SystemNfsMounter::open(mount_root)?,
            max_size_bytes,
            operation_timeout,
        })
    }

    fn placement_constraint(&self) -> PlacementConstraint {
        let (region, zone) = self.nfs.placement();
        PlacementConstraint {
            host_id: None,
            region: region.map(str::to_owned),
            zone: zone.map(str::to_owned),
        }
    }

    fn with_mounted_volume<T>(
        &self,
        volume_id: Uuid,
        generation: u64,
        read_only: bool,
        operation: impl FnOnce(&LocalBlockProvider) -> Result<T, VolumeError>,
    ) -> Result<T, VolumeError> {
        let spec = self.nfs.prepare(volume_id, generation, read_only)?;
        let attachment = self.mounter.mount(&spec, self.operation_timeout)?;
        let block_root = attachment.private_path().join(".tarit-block-volumes");
        let result =
            LocalBlockProvider::open(block_root, self.provider_name(), self.max_size_bytes)
                .and_then(|provider| operation(&provider));
        let detach = self.mounter.unmount(attachment, self.operation_timeout);
        match (result, detach) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(operation_error), Err(detach_error)) => Err(VolumeError::Io(io::Error::other(
                format!(
                    "NFS-backed block operation failed: {operation_error}; detach failed: {detach_error}"
                ),
            ))),
        }
    }
}

impl BlockVolumeProvider for NfsBackedBlockProvider {
    fn provider_name(&self) -> &'static str {
        match self.nfs.dialect {
            NfsDialect::GenericV4_1 => "nfs_v4_1_block",
            NfsDialect::AwsEfs => "aws_efs_block",
            NfsDialect::AzureFiles => "azure_files_nfs_block",
        }
    }

    fn capabilities(&self) -> VolumeCapabilities {
        VolumeCapabilities {
            storage_class: StorageClass::Block,
            read_only_many: true,
            read_write_once: true,
            read_write_many: false,
            snapshots: false,
            clones: false,
        }
    }

    fn create(&self, volume_id: Uuid, size_bytes: u64) -> Result<ProviderVolume, VolumeError> {
        self.with_mounted_volume(volume_id, 1, false, |provider| {
            provider.create(volume_id, size_bytes)
        })?;
        Ok(ProviderVolume {
            volume_id,
            size_bytes,
            constraint: self.placement_constraint(),
        })
    }

    fn delete(&self, volume_id: Uuid) -> Result<(), VolumeError> {
        self.with_mounted_volume(volume_id, 1, false, |provider| provider.delete(volume_id))
    }

    fn prepare(
        &self,
        volume_id: Uuid,
        size_bytes: u64,
        mode: AccessMode,
        generation: u64,
    ) -> Result<PreparedBlockAttachment, VolumeError> {
        if mode == AccessMode::ReadWriteMany {
            return Err(VolumeError::Unsupported(
                "NFS-backed raw block volumes do not support concurrent writers".into(),
            ));
        }
        let read_only = mode == AccessMode::ReadOnlyMany;
        let spec = self.nfs.prepare(volume_id, generation, read_only)?;
        let filesystem = self.mounter.mount(&spec, self.operation_timeout)?;
        let block_root = filesystem.private_path().join(".tarit-block-volumes");
        let result =
            LocalBlockProvider::open(block_root, self.provider_name(), self.max_size_bytes)
                .and_then(|provider| provider.prepare(volume_id, size_bytes, mode, generation));
        match result {
            Ok(attachment) => {
                if let Err(error) = self
                    .mounter
                    .detach_open_file(filesystem, self.operation_timeout)
                {
                    drop(attachment);
                    return Err(error);
                }
                Ok(attachment)
            }
            Err(error) => match self.mounter.unmount(filesystem, self.operation_timeout) {
                Ok(()) => Err(error),
                Err(detach_error) => Err(VolumeError::Io(io::Error::other(format!(
                    "NFS-backed block prepare failed: {error}; detach failed: {detach_error}"
                )))),
            },
        }
    }
}

fn validate_endpoint(endpoint: &str) -> Result<(), VolumeError> {
    if endpoint.is_empty()
        || endpoint.len() > 253
        || endpoint.starts_with('-')
        || endpoint.ends_with('-')
        || endpoint.contains("..")
        || !endpoint
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err(VolumeError::Invalid("invalid NFS endpoint".into()));
    }
    Ok(())
}

fn validate_export(export: &str) -> Result<(), VolumeError> {
    if !export.starts_with('/')
        || export.len() > 1024
        || export
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b',' || byte == b':')
        || export.split('/').any(|component| component == "..")
    {
        return Err(VolumeError::Invalid("invalid NFS export path".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::io::{Read, Write};
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::FileExt;

    #[test]
    fn dialects_have_safe_explicit_mount_semantics() {
        let id = Uuid::new_v4();
        let generic = NfsProvider::new(
            NfsDialect::GenericV4_1,
            "nfs.internal",
            "/exports/workspace",
            Some("region-a".into()),
            None,
        )
        .unwrap();
        let spec = generic.prepare(id, 3, false).unwrap();
        for required in [
            "vers=4.1",
            "hard",
            "noresvport",
            "nosuid",
            "nodev",
            "noexec",
            "rw",
            "sec=sys",
        ] {
            assert!(spec.options.iter().any(|option| option == required));
        }
        assert!(!format!("{spec:?}").contains("nfs.internal"));
        assert!(generic.profile().capabilities.read_write_many);

        let efs = NfsProvider::new(
            NfsDialect::AwsEfs,
            "fs-01234567.efs.us-east-1.amazonaws.com",
            "/",
            Some("us-east-1".into()),
            None,
        )
        .unwrap();
        assert_eq!(efs.provider_name(), "aws_efs");
        let spec = efs.prepare(id, 4, false).unwrap();
        assert!(spec.options.contains(&"rsize=1048576".into()));
        assert!(spec.options.contains(&"wsize=1048576".into()));
        assert!(!spec
            .options
            .iter()
            .any(|option| option.starts_with("nconnect=")));

        let azure = NfsProvider::new(
            NfsDialect::AzureFiles,
            "account.file.core.windows.net",
            "/account/share",
            Some("eastus".into()),
            None,
        )
        .unwrap();
        let spec = azure.prepare(id, 4, true).unwrap();
        for required in [
            "vers=4",
            "minorversion=1",
            "proto=tcp",
            "sec=sys",
            "nolock",
            "noresvport",
            "nconnect=4",
            "rsize=1048576",
            "wsize=1048576",
            "actimeo=30",
            "ro",
        ] {
            assert!(spec.options.iter().any(|option| option == required));
        }
    }

    #[test]
    fn rejects_option_and_source_injection() {
        for endpoint in ["", "server,soft", "-bad", "a..b", "server:/escape"] {
            assert!(
                NfsProvider::new(NfsDialect::GenericV4_1, endpoint, "/export", None, None,)
                    .is_err()
            );
        }
        for export in ["relative", "/ok,soft", "/../escape", "/bad:source"] {
            assert!(NfsProvider::new(
                NfsDialect::GenericV4_1,
                "server.internal",
                export,
                None,
                None,
            )
            .is_err());
        }
    }

    #[test]
    fn mountinfo_identity_is_exact_and_escaped_paths_decode() {
        let entries = parse_mountinfo(
            "41 32 0:51 / /run/tarit\\040mount rw,nosuid,nodev,noexec - nfs4 server.internal:/exports/work rw,vers=4.1,hard,proto=tcp,sec=sys\n",
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mount_point, Path::new("/run/tarit mount"));
        let provider = NfsProvider::new(
            NfsDialect::GenericV4_1,
            "server.internal",
            "/exports/work",
            None,
            None,
        )
        .unwrap();
        let spec = provider.prepare(Uuid::new_v4(), 1, false).unwrap();
        assert!(verify_mount(&spec, &entries[0]).is_ok());
        let mut wrong = entries[0].clone();
        wrong.source = "server.internal:/exports/other".into();
        assert!(matches!(
            verify_mount(&spec, &wrong),
            Err(VolumeError::IdentityMismatch)
        ));

        let privacy = NfsProvider::new(
            NfsDialect::GenericV4_1,
            "server.internal",
            "/exports/work",
            None,
            None,
        )
        .unwrap()
        .with_security(NfsSecurityFlavor::Krb5Privacy)
        .unwrap()
        .prepare(Uuid::new_v4(), 1, false)
        .unwrap();
        assert!(matches!(
            verify_mount(&privacy, &entries[0]),
            Err(VolumeError::IdentityMismatch)
        ));
        let mut protected = entries[0].clone();
        protected.super_options.retain(|option| option != "sec=sys");
        protected.super_options.push("sec=krb5p".into());
        assert!(verify_mount(&privacy, &protected).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disposable_nfs_mount_reconnects_and_busy_detach_fails_closed() {
        let Ok(endpoint) = std::env::var("TARIT_TEST_NFS_ENDPOINT") else {
            return;
        };
        let export = std::env::var("TARIT_TEST_NFS_EXPORT").expect("NFS export");
        let mount_root =
            PathBuf::from(std::env::var("TARIT_TEST_NFS_MOUNT_ROOT").expect("NFS mount root"));
        let unit = std::env::var("TARIT_TEST_NFS_SYSTEMD_UNIT").expect("NFS systemd unit");
        assert!(matches!(
            unit.as_str(),
            "nfs-server.service" | "nfs-kernel-server.service"
        ));

        let provider = NfsProvider::new(
            NfsDialect::GenericV4_1,
            endpoint,
            export,
            Some("c8i-local".into()),
            None,
        )
        .unwrap();
        let spec = provider.prepare(Uuid::new_v4(), 11, false).unwrap();
        let mounter = SystemNfsMounter::open(mount_root).unwrap();
        let attachment = mounter.mount(&spec, Duration::from_secs(20)).unwrap();
        assert!(!format!("{attachment:?}").contains(&spec.source));

        let proof_path = attachment.private_path().join("proof");
        let mut proof = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&proof_path)
            .unwrap();
        proof.write_all(b"before-interruption").unwrap();
        proof.sync_all().unwrap();
        drop(proof);

        let systemctl = |action: &str| {
            Command::new("/usr/bin/systemctl")
                .arg(action)
                .arg(&unit)
                .status()
                .expect("run systemctl")
        };
        assert!(systemctl("stop").success());
        assert!(systemctl("start").success());
        assert!(Command::new("/usr/sbin/exportfs")
            .arg("-ra")
            .status()
            .expect("reload NFS exports")
            .success());

        let mut proof = OpenOptions::new().append(true).open(&proof_path).unwrap();
        proof.write_all(b"-after-reconnect").unwrap();
        proof.sync_all().unwrap();
        drop(proof);
        let mut contents = String::new();
        File::open(&proof_path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents, "before-interruption-after-reconnect");

        let mut holder = Command::new("/bin/sleep")
            .arg("30")
            .current_dir(attachment.private_path())
            .spawn()
            .unwrap();
        assert!(matches!(
            mounter.unmount(attachment, Duration::from_secs(5)),
            Err(VolumeError::Busy)
        ));
        holder.kill().unwrap();
        holder.wait().unwrap();
        let attachment = mounter.mount(&spec, Duration::from_secs(20)).unwrap();
        mounter
            .unmount(attachment, Duration::from_secs(20))
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disposable_nfs_backed_block_reopens_durably_without_a_live_mount() {
        let Ok(endpoint) = std::env::var("TARIT_TEST_NFS_ENDPOINT") else {
            return;
        };
        let export = std::env::var("TARIT_TEST_NFS_EXPORT").expect("NFS export");
        let mount_root =
            PathBuf::from(std::env::var("TARIT_TEST_NFS_MOUNT_ROOT").expect("NFS mount root"));
        let nfs = NfsProvider::new(
            NfsDialect::GenericV4_1,
            endpoint,
            export,
            Some("test-region".into()),
            None,
        )
        .unwrap();
        let provider = NfsBackedBlockProvider::open(
            nfs,
            &mount_root,
            8 * MIN_BLOCK_VOLUME_BYTES,
            Duration::from_secs(20),
        )
        .unwrap();
        let id = Uuid::new_v4();
        let volume = provider.create(id, MIN_BLOCK_VOLUME_BYTES).unwrap();
        assert_eq!(volume.constraint.host_id, None);
        assert_eq!(volume.constraint.region.as_deref(), Some("test-region"));

        let attachment = provider
            .prepare(id, MIN_BLOCK_VOLUME_BYTES, AccessMode::ReadWriteOnce, 1)
            .unwrap();
        assert!(find_mount(&mount_root.join(format!("{id}-1")))
            .unwrap()
            .is_none());
        attachment
            .file
            .write_all_at(b"nfs-backed-block-proof", 4096)
            .unwrap();
        attachment.file.sync_all().unwrap();
        drop(attachment);

        let attachment = provider
            .prepare(id, MIN_BLOCK_VOLUME_BYTES, AccessMode::ReadOnlyMany, 1)
            .unwrap();
        assert!(find_mount(&mount_root.join(format!("{id}-1")))
            .unwrap()
            .is_none());
        let mut proof = [0_u8; 22];
        attachment.file.read_exact_at(&mut proof, 4096).unwrap();
        assert_eq!(&proof, b"nfs-backed-block-proof");
        drop(attachment);
        provider.delete(id).unwrap();
        assert!(matches!(
            provider.prepare(id, MIN_BLOCK_VOLUME_BYTES, AccessMode::ReadOnlyMany, 1,),
            Err(VolumeError::NotFound)
        ));
    }
}
