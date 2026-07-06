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

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
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
/// filesystem — no kernel, no init system (the container runtime runs the
/// entrypoint as PID 1). To boot one as a microVM we supply the kernel and drop
/// in the agent as `/usr/sbin/vmm-agent`, pointing `/sbin/init` at it (unless
/// the image already has an init). Booted with the agent as PID 1 it mounts the
/// pseudo-filesystems and serves the exec channel, so `node:20`, `python:3` and
/// friends become directly runnable sandboxes.
pub fn pull_and_convert_with_agent(
    image: &OciImageRef,
    output_path: &Path,
    size_mb: u64,
    agent_path: Option<&Path>,
) -> Result<OciPullResult, OciError> {
    let start = std::time::Instant::now();

    // Check for required tools.
    check_tool("skopeo")?;
    check_tool("umoci")?;
    check_tool("mke2fs")?;

    let work_dir = output_path.parent().unwrap_or(Path::new("."));
    let oci_dir = work_dir.join("oci-image");
    let rootfs_dir = work_dir.join("rootfs");

    // Clean up any previous attempt.
    let _ = std::fs::remove_dir_all(&oci_dir);
    let _ = std::fs::remove_dir_all(&rootfs_dir);

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
        .arg(output_path)
        .arg(format!("{}M", size_mb));
    run_command(mke2fs, "mke2fs")?;

    // Clean up intermediate dirs.
    let _ = std::fs::remove_dir_all(&oci_dir);
    let _ = std::fs::remove_dir_all(&rootfs_dir);

    let size_bytes = std::fs::metadata(output_path)?.len();
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

/// Copy the exec agent into an unpacked rootfs and make it the init: install to
/// `/usr/sbin/vmm-agent` and point `/sbin/init` at it when the image has no init
/// of its own. The image can then be booted with the default cmdline (the agent
/// runs as PID 1) or with an explicit `init=/usr/sbin/vmm-agent`.
fn inject_agent_init(rootfs: &Path, agent: &Path) -> Result<(), OciError> {
    use std::os::unix::fs::PermissionsExt;

    let sbin = rootfs.join("usr/sbin");
    std::fs::create_dir_all(&sbin)?;
    let dst = sbin.join("vmm-agent");
    std::fs::copy(agent, &dst)?;
    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;

    // Point /sbin/init at the agent unless the image already provides one, so
    // the default `root=/dev/vda rw` cmdline (no init=) boots straight to it.
    let sbin_dir = rootfs.join("sbin");
    std::fs::create_dir_all(&sbin_dir)?;
    let init = sbin_dir.join("init");
    if !init.exists() {
        let _ = std::os::unix::fs::symlink("/usr/sbin/vmm-agent", &init);
    }
    log::info!("oci: injected exec agent as init (/usr/sbin/vmm-agent)");
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn which_finds_known_commands() {
        // `ls` should always be available.
        assert!(which("ls").is_some());
        assert!(which("nonexistent_tool_12345").is_none());
    }
}
