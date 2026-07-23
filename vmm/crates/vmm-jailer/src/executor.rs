//! Real jailer execution — chroot + namespaces + cgroups + privilege drop.
//!
//! Jailer wrapper: chroot, mount/UTS/IPC/network namespaces,
//! cgroups (CPU/mem/IO/PID limits), drop to unprivileged uid/gid.

#![cfg(target_os = "linux")]

use crate::jailer::{JailerConfig, JailerError};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

/// Execute the jailer: set up namespaces, chroot, cgroups, drop privs,
/// then return (the caller exec's the VMM or runs it in-process).
pub fn jail(cfg: &JailerConfig) -> Result<(), JailerError> {
    log::info!(
        "jail: chroot={} uid={} gid={} cgroup={} netns={} nofile={}",
        cfg.chroot_dir,
        cfg.uid,
        cfg.gid,
        cfg.cgroup,
        cfg.netns,
        cfg.rlimit_nofile
    );

    // Finding 6: reject uid=0 / gid=0 — running as root defeats the jailer.
    if cfg.uid == 0 {
        return Err(JailerError::PrivDrop(
            "uid=0 is not allowed — jailer must drop to an unprivileged uid".into(),
        ));
    }
    if cfg.gid == 0 {
        return Err(JailerError::PrivDrop(
            "gid=0 is not allowed — jailer must drop to an unprivileged gid".into(),
        ));
    }

    // Finding 5: chroot_dir must exist — hard-fail instead of warn+continue.
    if !Path::new(&cfg.chroot_dir).exists() {
        return Err(JailerError::Setup(format!(
            "chroot dir {} does not exist — refusing to run without confinement",
            cfg.chroot_dir
        )));
    }

    let last_capability = read_last_capability()?;

    // 1. Set resource limits (RLIMIT_NOFILE, RLIMIT_AS).
    set_rlimits(cfg)?;

    // 1b. Apply cgroup v2 limits (CPU, memory, IO, PIDs).
    if !cfg.cgroup.is_empty() {
        apply_cgroup(cfg)?;
    }

    // 2. Enter the assigned network namespace, or create a fresh empty one for
    // no-NIC workloads. A jailed VMM must never retain the host net namespace.
    if !cfg.netns.is_empty() {
        enter_netns(&cfg.netns)?;
    } else {
        unshare_empty_netns()?;
    }

    // 3. Create private mount, UTS, and IPC namespaces. PID namespaces need a
    // fork to take effect and are therefore the launcher's responsibility.
    unshare_namespaces()?;
    make_mounts_private()?;

    // 4. Chroot into the jail directory.
    do_chroot(&cfg.chroot_dir)?;

    // 5. Prevent privilege gains and clear the capability bounding/ambient
    // sets while CAP_SETPCAP is still effective. Clearing the effective set
    // first would also remove CAP_SETUID/CAP_SETGID and make the identity
    // transition fail.
    set_no_new_privileges()?;
    drop_capability_bounding_set(last_capability)?;
    clear_ambient_capabilities()?;

    // 6. Drop supplementary groups and all real/effective/saved IDs.
    drop_privileges(cfg.uid, cfg.gid)?;

    // 7. Clear and verify every remaining capability set after setresuid.
    clear_process_capabilities()?;
    verify_confinement(cfg.uid, cfg.gid, last_capability)?;

    // Files created after confinement must never be group/world accessible,
    // even if a caller inherited a permissive umask.
    // SAFETY: umask takes and returns scalar mode values only.
    unsafe { libc::umask(0o077) };

    log::info!("jail: all confinement applied successfully");
    Ok(())
}

fn set_rlimits(cfg: &JailerConfig) -> Result<(), JailerError> {
    // RLIMIT_NOFILE
    let rlim = libc::rlimit {
        rlim_cur: cfg.rlimit_nofile,
        rlim_max: cfg.rlimit_nofile,
    };
    // SAFETY: `rlim` points to a valid `libc::rlimit` that lives for the
    // duration of the syscall.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) };
    if rc < 0 {
        return Err(JailerError::PrivDrop(format!(
            "setrlimit NOFILE: {}",
            std::io::Error::last_os_error()
        )));
    }

    // RLIMIT_AS (address space)
    if cfg.rlimit_as > 0 {
        let rlim_as = libc::rlimit {
            rlim_cur: cfg.rlimit_as,
            rlim_max: cfg.rlimit_as,
        };
        // SAFETY: `rlim_as` points to a valid `libc::rlimit` that lives for
        // the duration of the syscall.
        let rc = unsafe { libc::setrlimit(libc::RLIMIT_AS, &rlim_as) };
        if rc < 0 {
            return Err(JailerError::PrivDrop(format!(
                "setrlimit AS: {}",
                std::io::Error::last_os_error()
            )));
        }
    }

    log::info!(
        "jail: rlimits set (nofile={}, as={})",
        cfg.rlimit_nofile,
        cfg.rlimit_as
    );
    Ok(())
}

fn apply_cgroup(cfg: &JailerConfig) -> Result<(), JailerError> {
    use crate::cgroups;

    cgroups::apply_current_process(&cfg.cgroup, cfg.cgroup_limits.as_ref())
        .map_err(|e| JailerError::Setup(format!("cgroup apply: {e}")))?;

    log::info!("jail: cgroup applied: {}", cfg.cgroup);
    Ok(())
}

fn enter_netns(path: &str) -> Result<(), JailerError> {
    let c_path = CString::new(path)
        .map_err(|_| JailerError::Namespace(format!("netns path contains NUL: {path:?}")))?;
    // SAFETY: `c_path` is a valid NUL-terminated C string and `open` does not
    // retain the pointer after returning.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(JailerError::Namespace(format!(
            "open netns {path}: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `fd` is an open network-namespace file descriptor and no pointer
    // arguments are passed to `setns`.
    let rc = unsafe { libc::setns(fd, libc::CLONE_NEWNET) };
    // SAFETY: `fd` was opened above and is no longer needed after `setns`
    // returns; ignoring close errors preserves existing behavior.
    unsafe { libc::close(fd) };
    if rc < 0 {
        return Err(JailerError::Namespace(format!(
            "setns netns: {}",
            std::io::Error::last_os_error()
        )));
    }
    log::info!("jail: entered netns {path}");
    Ok(())
}

fn unshare_empty_netns() -> Result<(), JailerError> {
    // SAFETY: unshare is called with a namespace flag and no pointer arguments.
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } < 0 {
        return Err(JailerError::Namespace(format!(
            "unshare(CLONE_NEWNET) for empty network namespace: {}",
            std::io::Error::last_os_error()
        )));
    }
    log::info!("jail: fresh empty network namespace created");
    Ok(())
}

fn unshare_namespaces() -> Result<(), JailerError> {
    let flags = libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC;
    // SAFETY: `unshare` is called with namespace flags and no pointer arguments.
    let rc = unsafe { libc::unshare(flags) };
    if rc < 0 {
        return Err(JailerError::Namespace(format!(
            "unshare(CLONE_NEWNS|CLONE_NEWUTS|CLONE_NEWIPC) failed: {} — refusing to run without private namespaces",
            std::io::Error::last_os_error()
        )));
    }
    log::info!("jail: private mount, UTS, and IPC namespaces created");
    Ok(())
}

fn make_mounts_private() -> Result<(), JailerError> {
    let root = CString::new("/").expect("static root path contains no NUL");
    // SAFETY: the target is a valid NUL-terminated string, source/fs/data are
    // null as required for a propagation-only mount operation.
    let rc = unsafe {
        libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    };
    if rc < 0 {
        return Err(JailerError::Namespace(format!(
            "make mount tree private: {}",
            std::io::Error::last_os_error()
        )));
    }
    log::info!("jail: mount propagation made recursively private");
    Ok(())
}

fn do_chroot(dir: &str) -> Result<(), JailerError> {
    let c_dir = CString::new(dir)
        .map_err(|_| JailerError::PrivDrop(format!("chroot path contains NUL: {dir:?}")))?;
    // Open the jail root without following its final component, then operate
    // on the descriptor. This prevents a concurrent final-component symlink
    // swap between validation and chroot.
    // SAFETY: c_dir is a valid NUL-terminated path and open retains no pointer.
    let fd = unsafe {
        libc::open(
            c_dir.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(JailerError::PrivDrop(format!(
            "open jail root {dir}: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: fd was returned by open above and is uniquely owned here.
    let jail_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: jail_fd is an open directory descriptor.
    if unsafe { libc::fchdir(jail_fd.as_raw_fd()) } < 0 {
        return Err(JailerError::PrivDrop(format!(
            "fchdir jail root {dir}: {}",
            std::io::Error::last_os_error()
        )));
    }
    let dot = CString::new(".").expect("static dot path contains no NUL");
    // SAFETY: dot is valid and the current directory is the opened jail root.
    if unsafe { libc::chroot(dot.as_ptr()) } < 0 {
        return Err(JailerError::PrivDrop(format!(
            "chroot {dir}: {}",
            std::io::Error::last_os_error()
        )));
    }
    let root = CString::new("/").expect("static root path contains no NUL");
    // SAFETY: `root` is a valid NUL-terminated C string that lives for the
    // duration of the `chdir` call.
    let rc = unsafe { libc::chdir(root.as_ptr()) };
    if rc < 0 {
        return Err(JailerError::PrivDrop(format!(
            "chdir / after chroot: {}",
            std::io::Error::last_os_error()
        )));
    }
    log::info!("jail: chrooted to {dir}");
    Ok(())
}

// Linux capability constants not always in libc, define them here.
mod cap {
    #![allow(dead_code)]
    pub const CAP_CHOWN: u32 = 0;
    pub const CAP_DAC_OVERRIDE: u32 = 1;
    pub const CAP_DAC_READ_SEARCH: u32 = 2;
    pub const CAP_FOWNER: u32 = 3;
    pub const CAP_FSETID: u32 = 4;
    pub const CAP_KILL: u32 = 5;
    pub const CAP_SETGID: u32 = 6;
    pub const CAP_SETUID: u32 = 7;
    pub const CAP_SETPCAP: u32 = 8;
    pub const CAP_LINUX_IMMUTABLE: u32 = 9;
    pub const CAP_NET_BIND_SERVICE: u32 = 10;
    pub const CAP_NET_BROADCAST: u32 = 11;
    pub const CAP_NET_ADMIN: u32 = 12;
    pub const CAP_NET_RAW: u32 = 13;
    pub const CAP_IPC_LOCK: u32 = 14;
    pub const CAP_IPC_OWNER: u32 = 15;
    pub const CAP_SYS_MODULE: u32 = 16;
    pub const CAP_SYS_RAWIO: u32 = 17;
    pub const CAP_SYS_CHROOT: u32 = 18;
    pub const CAP_SYS_PTRACE: u32 = 19;
    pub const CAP_SYS_PACCT: u32 = 20;
    pub const CAP_SYS_ADMIN: u32 = 21;
    pub const CAP_SYS_BOOT: u32 = 22;
    pub const CAP_SYS_NICE: u32 = 23;
    pub const CAP_SYS_RESOURCE: u32 = 24;
    pub const CAP_SYS_TIME: u32 = 25;
    pub const CAP_SYS_TTY_CONFIG: u32 = 26;
    pub const CAP_MKNOD: u32 = 27;
    pub const CAP_LEASE: u32 = 28;
    pub const CAP_AUDIT_WRITE: u32 = 29;
    pub const CAP_AUDIT_CONTROL: u32 = 30;
    pub const CAP_SETFCAP: u32 = 31;
    pub const CAP_MAC_OVERRIDE: u32 = 32;
    pub const CAP_MAC_ADMIN: u32 = 33;
    pub const CAP_SYSLOG: u32 = 34;
    pub const CAP_WAKE_ALARM: u32 = 35;
    pub const CAP_BLOCK_SUSPEND: u32 = 36;
    pub const CAP_AUDIT_READ: u32 = 37;
    pub const CAP_PERFMON: u32 = 38;
    pub const CAP_BPF: u32 = 39;
    pub const CAP_CHECKPOINT_RESTORE: u32 = 40;

    pub const LAST: u32 = CAP_CHECKPOINT_RESTORE;
}

const PR_CAPBSET_DROP: libc::c_int = 24;
const PR_CAPBSET_READ: libc::c_int = 23;
const PR_CAP_AMBIENT: libc::c_int = 47;
const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;
const PR_GET_NO_NEW_PRIVS: libc::c_int = 39;

#[repr(C)]
struct CapUserHeader {
    version: u32,
    pid: libc::pid_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct CapUserData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const LINUX_CAPABILITY_VERSION_3: u32 = 0x20080922;

fn read_last_capability() -> Result<u32, JailerError> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap").map_err(|e| {
        JailerError::PrivDrop(format!(
            "read /proc/sys/kernel/cap_last_cap before chroot: {e}"
        ))
    })?;
    let last = raw
        .trim()
        .parse::<u32>()
        .map_err(|e| JailerError::PrivDrop(format!("parse /proc/sys/kernel/cap_last_cap: {e}")))?;
    if last > 4096 {
        return Err(JailerError::PrivDrop(format!(
            "implausible cap_last_cap value {last}"
        )));
    }
    Ok(last)
}

fn set_no_new_privileges() -> Result<(), JailerError> {
    // SAFETY: `prctl` is called with scalar arguments only and does not access
    // Rust-managed memory.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc < 0 {
        return Err(JailerError::PrivDrop(format!(
            "PR_SET_NO_NEW_PRIVS: {}",
            std::io::Error::last_os_error()
        )));
    }
    log::info!("jail: PR_SET_NO_NEW_PRIVS set");
    Ok(())
}

fn drop_capability_bounding_set(last_capability: u32) -> Result<(), JailerError> {
    for capability in 0..=last_capability {
        // SAFETY: `prctl` is called with scalar arguments only and does not
        // access Rust-managed memory.
        let rc = unsafe { libc::prctl(PR_CAPBSET_DROP, capability as libc::c_ulong, 0, 0, 0) };
        if rc < 0 {
            return Err(JailerError::PrivDrop(format!(
                "PR_CAPBSET_DROP({capability}): {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    log::info!("jail: capability bounding set cleared");
    Ok(())
}

fn clear_ambient_capabilities() -> Result<(), JailerError> {
    // SAFETY: `prctl` is called with scalar arguments only and does not access
    // Rust-managed memory.
    let rc = unsafe { libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) };
    if rc < 0 {
        let error = std::io::Error::last_os_error();
        // Kernels predating ambient capabilities return EINVAL; in that case
        // there is no ambient set to clear. Every other error is unsafe.
        if error.raw_os_error() != Some(libc::EINVAL) {
            return Err(JailerError::PrivDrop(format!(
                "PR_CAP_AMBIENT_CLEAR_ALL: {error}"
            )));
        }
    }
    Ok(())
}

fn clear_process_capabilities() -> Result<(), JailerError> {
    let header = CapUserHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0, // 0 = self
    };
    let data = [CapUserData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2]; // v3 needs 2 data entries

    // SAFETY: `header` and `data` have the Linux capset-compatible layout and
    // live for the duration of the syscall.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_capset,
            &header as *const CapUserHeader,
            data.as_ptr(),
        )
    };
    if rc < 0 {
        // Zeroing the effective/permitted/inheritable sets is a required step.
        // The bounding set was already cleared, but we must not claim full
        // confinement if this failed, so fail closed.
        return Err(JailerError::PrivDrop(format!(
            "capset (drop effective/permitted/inheritable caps) failed: {} — refusing to run with residual capabilities",
            std::io::Error::last_os_error()
        )));
    }
    log::info!("jail: effective/permitted/inheritable capabilities cleared");

    Ok(())
}

fn drop_privileges(uid: u32, gid: u32) -> Result<(), JailerError> {
    // Finding 6: uid=0/gid=0 are rejected in jail() above, so we assert here.
    if uid == 0 || gid == 0 {
        return Err(JailerError::PrivDrop(
            "refusing to drop privileges to uid=0 or gid=0".into(),
        ));
    }

    // SAFETY: passing a null group pointer is valid when the group count is 0.
    let rc = unsafe { libc::setgroups(0, std::ptr::null()) };
    if rc < 0 {
        return Err(JailerError::PrivDrop(format!(
            "setgroups(0): {}",
            std::io::Error::last_os_error()
        )));
    }

    // setresgid/setresuid clear real, effective, and saved IDs. A saved root ID
    // would make the privilege drop reversible.
    // SAFETY: setresgid takes scalar IDs only.
    let rc = unsafe { libc::setresgid(gid, gid, gid) };
    if rc < 0 {
        return Err(JailerError::PrivDrop(format!(
            "setresgid({gid}): {}",
            std::io::Error::last_os_error()
        )));
    }

    // SAFETY: setresuid takes scalar IDs only.
    let rc = unsafe { libc::setresuid(uid, uid, uid) };
    if rc < 0 {
        return Err(JailerError::PrivDrop(format!(
            "setresuid({uid}): {}",
            std::io::Error::last_os_error()
        )));
    }

    log::info!("jail: privileges dropped to uid={uid} gid={gid}");
    Ok(())
}

fn verify_confinement(uid: u32, gid: u32, last_capability: u32) -> Result<(), JailerError> {
    let mut real_uid = 0;
    let mut effective_uid = 0;
    let mut saved_uid = 0;
    // SAFETY: pointers refer to valid uid_t storage.
    if unsafe { libc::getresuid(&mut real_uid, &mut effective_uid, &mut saved_uid) } < 0 {
        return Err(JailerError::PrivDrop(format!(
            "getresuid: {}",
            std::io::Error::last_os_error()
        )));
    }
    let mut real_gid = 0;
    let mut effective_gid = 0;
    let mut saved_gid = 0;
    // SAFETY: pointers refer to valid gid_t storage.
    if unsafe { libc::getresgid(&mut real_gid, &mut effective_gid, &mut saved_gid) } < 0 {
        return Err(JailerError::PrivDrop(format!(
            "getresgid: {}",
            std::io::Error::last_os_error()
        )));
    }
    if [real_uid, effective_uid, saved_uid] != [uid, uid, uid]
        || [real_gid, effective_gid, saved_gid] != [gid, gid, gid]
    {
        return Err(JailerError::PrivDrop(format!(
            "identity verification failed: uid={real_uid}/{effective_uid}/{saved_uid}, gid={real_gid}/{effective_gid}/{saved_gid}"
        )));
    }

    // SAFETY: a zero-size getgroups query accepts a null pointer.
    let group_count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if group_count != 0 {
        return Err(JailerError::PrivDrop(format!(
            "supplementary groups remain after drop: {group_count}"
        )));
    }

    let header = CapUserHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapUserData {
        effective: u32::MAX,
        permitted: u32::MAX,
        inheritable: u32::MAX,
    }; 2];
    // SAFETY: header/data have the capget ABI layout and valid lifetimes.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_capget,
            &header as *const CapUserHeader,
            data.as_mut_ptr(),
        )
    };
    if rc < 0 {
        return Err(JailerError::PrivDrop(format!(
            "capget verification: {}",
            std::io::Error::last_os_error()
        )));
    }
    if data
        .iter()
        .any(|set| set.effective != 0 || set.permitted != 0 || set.inheritable != 0)
    {
        return Err(JailerError::PrivDrop(
            "effective, permitted, or inheritable capabilities remain".into(),
        ));
    }

    for capability in 0..=last_capability {
        // SAFETY: prctl is called with scalar arguments only.
        let present = unsafe { libc::prctl(PR_CAPBSET_READ, capability as libc::c_ulong, 0, 0, 0) };
        if present != 0 {
            return Err(JailerError::PrivDrop(format!(
                "capability {capability} remains in bounding set"
            )));
        }
    }

    // SAFETY: PR_GET_NO_NEW_PRIVS accepts scalar zero arguments.
    if unsafe { libc::prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1 {
        return Err(JailerError::PrivDrop(
            "PR_SET_NO_NEW_PRIVS verification failed".into(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jailer::JailerConfig;

    #[test]
    fn jailer_config_round_trips() {
        let cfg = JailerConfig {
            chroot_dir: "/tmp/jail".into(),
            uid: 1000,
            gid: 1000,
            cgroup: "/sys/fs/cgroup/vmm".into(),
            cgroup_limits: None,
            rlimit_nofile: 1024,
            rlimit_as: 1 << 30,
            netns: "/var/run/netns/vmm0".into(),
        };
        assert_eq!(cfg.uid, 1000);
        assert_eq!(cfg.rlimit_nofile, 1024);
    }

    #[test]
    fn jail_rejects_uid_zero() {
        // Finding 6: uid=0 must be rejected, not silently skipped.
        let cfg = JailerConfig {
            chroot_dir: "/tmp/jail".into(),
            uid: 0,
            gid: 1000,
            cgroup: "".into(),
            rlimit_nofile: 1024,
            rlimit_as: 0,
            netns: "".into(),
            cgroup_limits: None,
        };
        let result = jail(&cfg);
        assert!(result.is_err(), "jail() with uid=0 must return an error");
        let err = result.unwrap_err();
        match err {
            JailerError::PrivDrop(msg) => {
                assert!(
                    msg.contains("uid=0"),
                    "error should mention uid=0, got: {msg}"
                );
            }
            _ => panic!("expected PrivDrop error, got {err:?}"),
        }
    }

    #[test]
    fn jail_rejects_gid_zero() {
        // Finding 6: gid=0 must be rejected.
        let cfg = JailerConfig {
            chroot_dir: "/tmp/jail".into(),
            uid: 1000,
            gid: 0,
            cgroup: "".into(),
            rlimit_nofile: 1024,
            rlimit_as: 0,
            netns: "".into(),
            cgroup_limits: None,
        };
        let result = jail(&cfg);
        assert!(result.is_err(), "jail() with gid=0 must return an error");
    }

    #[test]
    fn jail_rejects_missing_chroot_dir() {
        // Finding 5: missing chroot dir must hard-fail, not warn+continue.
        let cfg = JailerConfig {
            chroot_dir: "/nonexistent/path/that/should/not/exist".into(),
            uid: 1000,
            gid: 1000,
            cgroup: "".into(),
            rlimit_nofile: 1024,
            rlimit_as: 0,
            netns: "".into(),
            cgroup_limits: None,
        };
        let result = jail(&cfg);
        assert!(
            result.is_err(),
            "jail() with missing chroot dir must return an error"
        );
        let err = result.unwrap_err();
        match err {
            JailerError::Setup(msg) => {
                assert!(
                    msg.contains("does not exist"),
                    "error should mention missing dir, got: {msg}"
                );
            }
            _ => panic!("expected Setup error, got {err:?}"),
        }
    }

    #[test]
    fn drop_privileges_rejects_zero() {
        // Finding 6: drop_privileges itself must also reject uid=0/gid=0.
        let result = drop_privileges(0, 1000);
        assert!(result.is_err());
        let result = drop_privileges(1000, 0);
        assert!(result.is_err());
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn cap_constants_defined() {
        // Verify the cap constants are sensible.
        assert_eq!(cap::CAP_SYS_ADMIN, 21);
        assert_eq!(cap::CAP_NET_RAW, 13);
        assert_eq!(cap::CAP_DAC_READ_SEARCH, 2);
        assert!(cap::LAST >= 37);
    }

    #[test]
    fn capset_struct_sizes() {
        // The capset syscall with v3 expects header=8 bytes, data=2*12=24 bytes.
        assert_eq!(std::mem::size_of::<CapUserHeader>(), 8);
        assert_eq!(std::mem::size_of::<CapUserData>(), 12);
    }
}
