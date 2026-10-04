use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const KERNEL_RELEASE_ENV: &str = include_str!("../guest/kernel-version.env");

#[derive(Debug)]
struct KernelRelease {
    version: String,
    sha256: String,
    repository: String,
    tag: String,
}

enum InstalledKernel {
    Missing,
    Valid,
    ChecksumMismatch(String),
}

impl KernelRelease {
    fn load() -> Result<Self> {
        let release = Self {
            version: metadata_value("KERNEL_VERSION")?,
            sha256: metadata_value("KERNEL_ARTIFACT_SHA256")?,
            repository: metadata_value("KERNEL_RELEASE_REPOSITORY")?,
            tag: metadata_value("KERNEL_RELEASE_TAG")?,
        };
        anyhow::ensure!(
            release
                .version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')),
            "embedded guest kernel version is invalid"
        );
        anyhow::ensure!(
            release.sha256.len() == 64
                && release
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "embedded guest kernel SHA-256 is invalid"
        );
        anyhow::ensure!(
            release.repository.split_once('/').is_some()
                && release
                    .repository
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric()
                        || matches!(byte, b'.' | b'-' | b'_' | b'/')),
            "embedded guest kernel repository is invalid"
        );
        anyhow::ensure!(
            release
                .tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')),
            "embedded guest kernel release tag is invalid"
        );
        Ok(release)
    }

    fn artifact(&self) -> String {
        format!("vmlinux-{}-x86_64", self.version)
    }

    fn url(&self) -> String {
        format!(
            "https://github.com/{}/releases/download/{}/{}",
            self.repository,
            self.tag,
            self.artifact()
        )
    }
}

fn metadata_value(name: &str) -> Result<String> {
    KERNEL_RELEASE_ENV
        .lines()
        .filter_map(|line| line.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then(|| value.trim().to_string()))
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{name} is missing from embedded guest kernel metadata"))
}

fn default_path(release: &KernelRelease) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("TARIT_KERNEL") {
        anyhow::ensure!(!path.is_empty(), "TARIT_KERNEL must not be empty");
        return Ok(PathBuf::from(path));
    }

    // SAFETY: geteuid has no preconditions.
    let base = if unsafe { libc::geteuid() } == 0 {
        PathBuf::from("/var/lib/tarit")
    } else if let Some(path) = std::env::var_os("XDG_DATA_HOME").filter(|path| !path.is_empty()) {
        PathBuf::from(path).join("tarit")
    } else {
        let home = std::env::var_os("HOME")
            .filter(|path| !path.is_empty())
            .context("HOME or XDG_DATA_HOME is required to choose a kernel install path")?;
        PathBuf::from(home).join(".local/share/tarit")
    };
    Ok(base.join("kernels").join(&release.version).join("vmlinux"))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("open guest kernel for hashing: {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("read guest kernel: {}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn inspect_installed(path: &Path, release: &KernelRelease) -> Result<InstalledKernel> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(InstalledKernel::Missing)
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect guest kernel path: {}", path.display()))
        }
    };
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "guest kernel path must be a regular file, not a symlink: {}",
        path.display()
    );
    let actual = sha256_file(path)?;
    if actual == release.sha256 {
        Ok(InstalledKernel::Valid)
    } else {
        Ok(InstalledKernel::ChecksumMismatch(actual))
    }
}

pub fn resolve(kernel: Option<String>) -> Result<String> {
    if let Some(kernel) = kernel {
        return Ok(kernel);
    }

    let release = KernelRelease::load()?;
    let path = default_path(&release)?;
    match inspect_installed(&path, &release)? {
        InstalledKernel::Valid => return path_to_string(path),
        InstalledKernel::Missing => {}
        InstalledKernel::ChecksumMismatch(actual) => {
            anyhow::bail!(
                "guest kernel checksum mismatch at {}: expected {}, got {}; run `vmm kernel install --force` to repair it",
                path.display(),
                release.sha256,
                actual
            )
        }
    }

    if !io::stdin().is_terminal() {
        anyhow::bail!(
            "--kernel is required in non-interactive mode; run `vmm kernel install` or pass --kernel PATH"
        );
    }

    eprint!(
        "No guest kernel was found at {}.\nDownload pinned Linux {} vmlinux (sha256 {})? [y/N] ",
        path.display(),
        release.version,
        release.sha256
    );
    io::stderr().flush().context("flush kernel prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read kernel download confirmation")?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        anyhow::bail!("guest kernel installation declined; pass --kernel PATH");
    }

    install_at(path.clone(), false, &release)?;
    path_to_string(path)
}

pub fn install(output: Option<PathBuf>, force: bool) -> Result<PathBuf> {
    let release = KernelRelease::load()?;
    let path = match output {
        Some(path) => path,
        None => default_path(&release)?,
    };
    install_at(path, force, &release)
}

fn install_at(path: PathBuf, force: bool, release: &KernelRelease) -> Result<PathBuf> {
    let replace_existing;
    match inspect_installed(&path, release)? {
        InstalledKernel::Valid => {
            println!(
                "guest kernel {} already installed at {}",
                release.version,
                path.display()
            );
            return Ok(path);
        }
        InstalledKernel::Missing => {
            replace_existing = false;
        }
        InstalledKernel::ChecksumMismatch(actual) if force => {
            replace_existing = true;
            eprintln!(
                "replacing guest kernel with checksum {actual}; expected {}",
                release.sha256
            );
        }
        InstalledKernel::ChecksumMismatch(actual) => {
            anyhow::bail!(
                "guest kernel checksum mismatch at {}: expected {}, got {}; rerun with --force to repair it",
                path.display(),
                release.sha256,
                actual
            )
        }
    }

    let parent = install_parent(&path);
    fs::create_dir_all(parent)
        .with_context(|| format!("create kernel install directory: {}", parent.display()))?;
    let temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary kernel file in {}", parent.display()))?;

    println!(
        "downloading Linux {} guest kernel from {}",
        release.version,
        release.url()
    );
    let status = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--retry",
            "3",
            "--retry-all-errors",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(temp.path())
        .arg(release.url())
        .status()
        .context("run curl to download guest kernel")?;
    anyhow::ensure!(status.success(), "guest kernel download failed");

    let actual = sha256_file(temp.path())?;
    anyhow::ensure!(
        actual == release.sha256,
        "downloaded guest kernel checksum mismatch: expected {}, got {}",
        release.sha256,
        actual
    );
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o644))
        .context("set guest kernel permissions")?;
    temp.as_file()
        .sync_all()
        .context("sync downloaded guest kernel")?;

    if replace_existing {
        temp.persist(&path)
            .map_err(|error| error.error)
            .with_context(|| format!("replace guest kernel: {}", path.display()))?;
    } else {
        temp.persist_noclobber(&path)
            .map_err(|error| error.error)
            .with_context(|| format!("install guest kernel: {}", path.display()))?;
    }
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync kernel install directory: {}", parent.display()))?;
    println!(
        "installed Linux {} guest kernel at {}",
        release.version,
        path.display()
    );
    Ok(path)
}

fn install_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn path_to_string(path: PathBuf) -> Result<String> {
    path.into_os_string()
        .into_string()
        .map_err(|path| anyhow::anyhow!("kernel path is not valid UTF-8: {path:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_with_sha256(sha256: &str) -> KernelRelease {
        KernelRelease {
            version: "1.2.3".into(),
            sha256: sha256.into(),
            repository: "owner/repository".into(),
            tag: "guest-kernel-v1.2.3".into(),
        }
    }

    #[test]
    fn embedded_release_metadata_is_valid() {
        let release = KernelRelease::load().unwrap();
        assert!(release.artifact().starts_with("vmlinux-"));
        assert!(release.url().starts_with("https://github.com/"));
    }

    #[test]
    fn hashes_kernel_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"abc").unwrap();
        file.flush().unwrap();
        assert_eq!(
            sha256_file(file.path()).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn reports_installed_checksum_mismatch() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"abc").unwrap();
        file.flush().unwrap();
        let release = release_with_sha256(&"0".repeat(64));
        match inspect_installed(file.path(), &release).unwrap() {
            InstalledKernel::ChecksumMismatch(actual) => {
                assert_eq!(
                    actual,
                    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                );
            }
            _ => panic!("expected checksum mismatch"),
        }
    }

    #[test]
    fn rejects_kernel_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        fs::write(&target, b"abc").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let release =
            release_with_sha256("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let error = inspect_installed(&link, &release)
            .err()
            .expect("symlink must be rejected");
        assert!(error.to_string().contains("not a symlink"));
    }

    #[test]
    fn bare_kernel_output_uses_current_directory() {
        assert_eq!(install_parent(Path::new("vmlinux")), Path::new("."));
        assert_eq!(
            install_parent(Path::new("/var/lib/tarit/vmlinux")),
            Path::new("/var/lib/tarit")
        );
    }
}
