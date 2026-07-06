//! cgroup v2 resource limits for a VMM process.
//!
//! The caller provides a cgroup v2 path and optional per-VM limits. This module
//! creates the cgroup, enables required parent controllers when possible, writes
//! the limit files, and can move the current process into the cgroup.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CgroupError {
    #[error("cgroup read {key}: {source}")]
    Read { key: String, source: std::io::Error },
    #[error("cgroup write {key}: {source}")]
    Write { key: String, source: std::io::Error },
    #[error("cgroup path: {0}")]
    Path(String),
}

/// Resource limits for a single VM, expressed as cgroup v2 control files.
/// All fields are optional — the caller sets only what it wants enforced.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CgroupLimits {
    /// cpu.weight (1-10000, default 100)
    pub cpu_weight: Option<u64>,
    /// cpu.max in "quota period" format, e.g. "200000 100000" for 2 CPUs
    pub cpu_max: Option<String>,
    /// cpuset.cpus, e.g. "2,3" or "2-3"
    pub cpuset_cpus: Option<String>,
    /// cpuset.mems, e.g. "0"
    pub cpuset_mems: Option<String>,
    /// memory.max in bytes
    pub memory_max: Option<u64>,
    /// memory.swap.max in bytes (0 = no swap)
    pub memory_swap_max: Option<u64>,
    /// memory.high (soft throttle threshold)
    pub memory_high: Option<u64>,
    /// pids.max
    pub pids_max: Option<u64>,
    /// io.weight (1-10000, default 100)
    pub io_weight: Option<u64>,
    /// io.max per device, e.g. "8:0 rbps=104857600 wbps=104857600 riops=1000 wiops=1000"
    pub io_max: Option<String>,
}

impl CgroupLimits {
    /// Returns true if no limits are set.
    pub fn is_empty(&self) -> bool {
        let map = self.to_file_map();
        map.is_empty()
    }

    /// Returns a map of cgroup v2 file names → string values to write.
    pub fn to_file_map(&self) -> BTreeMap<&'static str, String> {
        let mut map = BTreeMap::new();
        if let Some(v) = &self.cpu_weight {
            map.insert("cpu.weight", v.to_string());
        }
        if let Some(v) = &self.cpu_max {
            map.insert("cpu.max", v.clone());
        }
        if let Some(v) = &self.cpuset_cpus {
            map.insert("cpuset.cpus", v.clone());
        }
        if let Some(v) = &self.cpuset_mems {
            map.insert("cpuset.mems", v.clone());
        }
        if let Some(v) = self.memory_max {
            map.insert("memory.max", v.to_string());
        }
        if let Some(v) = self.memory_swap_max {
            map.insert("memory.swap.max", v.to_string());
        }
        if let Some(v) = self.memory_high {
            map.insert("memory.high", v.to_string());
        }
        if let Some(v) = self.pids_max {
            map.insert("pids.max", v.to_string());
        }
        if let Some(v) = self.io_weight {
            map.insert("io.weight", v.to_string());
        }
        if let Some(v) = &self.io_max {
            map.insert("io.max", v.clone());
        }
        map
    }
}

/// Write cgroup v2 limits to a cgroup directory.
///
/// `cgroup_path` is the full path under `/sys/fs/cgroup/`, e.g.
/// `/sys/fs/cgroup/vmm/vm-abc123`. The directory must already exist
/// (created by the caller). Each limit is written to the corresponding
/// control file inside that directory.
pub fn apply_limits(cgroup_path: &str, limits: &CgroupLimits) -> Result<(), CgroupError> {
    let dir = PathBuf::from(cgroup_path);
    ensure_cgroup2_dir(&dir)?;
    enable_parent_controllers(&dir, limits)?;

    for (key, val) in limits.to_file_map() {
        let file_path = dir.join(key);
        if !file_path.exists() {
            let controller = controller_for_key(key).unwrap_or("unknown");
            return Err(CgroupError::Path(format!(
                "missing cgroup v2 control file {} for controller '{controller}'. \
                 Ensure '{controller}' is listed in the parent cgroup.controllers \
                 and enabled in parent cgroup.subtree_control, or launch under a \
                 delegated writable subtree.",
                file_path.display()
            )));
        }
        match fs::write(&file_path, val.as_bytes()) {
            Ok(()) => {
                log::info!("cgroup: {key}={val} → {}", file_path.display());
            }
            Err(e) => {
                log::warn!("cgroup: write {key}={val} failed: {e}");
                return Err(CgroupError::Write {
                    key: key.to_string(),
                    source: e,
                });
            }
        }
    }
    Ok(())
}

/// Write the current process PID into a cgroup's cgroup.procs file.
/// This is how a process is moved into a cgroup.
pub fn add_pid(cgroup_path: &str, pid: u32) -> Result<(), CgroupError> {
    let dir = PathBuf::from(cgroup_path);
    ensure_cgroup2_dir(&dir)?;
    let procs = dir.join("cgroup.procs");
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(&procs)
        .map_err(|e| CgroupError::Write {
            key: format!(
                "{} (move pid {pid}; ensure the cgroup subtree is delegated and writable)",
                procs.display()
            ),
            source: e,
        })?;
    // Write the pid as a single write() with no trailing newline. `writeln!`
    // emits two writes (the number, then "\n"); cgroup.procs processes each
    // write() independently and rejects the empty trailing-newline write with
    // EINVAL even though the pid move already succeeded.
    f.write_all(pid.to_string().as_bytes())
        .map_err(|e| CgroupError::Write {
            key: format!(
                "{} (move pid {pid}; ensure the cgroup subtree is delegated and writable)",
                procs.display()
            ),
            source: e,
        })?;
    log::info!("cgroup: pid {pid} added to {cgroup_path}");
    Ok(())
}

/// Create a cgroup v2 directory (if it doesn't exist).
pub fn create_cgroup(path: &str) -> Result<(), CgroupError> {
    let p = PathBuf::from(path);
    if !p.is_absolute() {
        return Err(CgroupError::Path(format!(
            "cgroup path {path} must be absolute and under a cgroup v2 mount"
        )));
    }
    let existing = nearest_existing_ancestor(&p)
        .ok_or_else(|| CgroupError::Path(format!("no existing ancestor for cgroup path {path}")))?;
    ensure_cgroup2_dir(&existing)?;
    if !p.exists() {
        fs::create_dir_all(&p).map_err(|e| CgroupError::Path(format!("mkdir {path}: {e}")))?;
        log::info!("cgroup: created {path}");
    }
    ensure_cgroup2_dir(&p)?;
    Ok(())
}

/// Create a cgroup, write limits if provided, and move the current process into it.
pub fn apply_current_process(
    cgroup_path: &str,
    limits: Option<&CgroupLimits>,
) -> Result<(), CgroupError> {
    create_cgroup(cgroup_path)?;
    if let Some(limits) = limits {
        apply_limits(cgroup_path, limits)?;
    }
    // SAFETY: `getpid` takes no arguments and does not access Rust-managed
    // memory.
    let pid = unsafe { libc::getpid() } as u32;
    add_pid(cgroup_path, pid)?;
    Ok(())
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cur = path;
    loop {
        if cur.exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

fn ensure_cgroup2_dir(path: &Path) -> Result<(), CgroupError> {
    if !path.exists() {
        return Err(CgroupError::Path(format!(
            "cgroup dir {} does not exist",
            path.display()
        )));
    }
    if !path.is_dir() {
        return Err(CgroupError::Path(format!(
            "cgroup path {} is not a directory",
            path.display()
        )));
    }
    let controllers = path.join("cgroup.controllers");
    let procs = path.join("cgroup.procs");
    if !controllers.exists() || !procs.exists() {
        return Err(CgroupError::Path(format!(
            "{} is not a cgroup v2 directory (missing cgroup.controllers or \
             cgroup.procs); pass a path under a cgroup v2 mount such as \
             /sys/fs/cgroup",
            path.display()
        )));
    }
    Ok(())
}

fn enable_parent_controllers(path: &Path, limits: &CgroupLimits) -> Result<(), CgroupError> {
    let required = required_controllers(limits);
    if required.is_empty() {
        return Ok(());
    }

    let parent = path.parent().ok_or_else(|| {
        CgroupError::Path(format!(
            "cgroup path {} has no parent to enable controllers in",
            path.display()
        ))
    })?;
    ensure_cgroup2_dir(parent)?;

    let available = read_word_set(parent.join("cgroup.controllers"))?;
    let enabled = read_word_set(parent.join("cgroup.subtree_control"))?;
    for controller in required {
        if !available.contains(controller) {
            return Err(CgroupError::Path(format!(
                "cgroup v2 controller '{controller}' is not available in \
                 {}/cgroup.controllers (available: {}). Enable/delegate it from \
                 the parent subtree before launching the VMM.",
                parent.display(),
                format_word_set(&available)
            )));
        }
        if !enabled.contains(controller) {
            let subtree_control = parent.join("cgroup.subtree_control");
            let value = format!("+{controller}");
            fs::write(&subtree_control, value.as_bytes()).map_err(|e| {
                CgroupError::Path(format!(
                    "failed to enable cgroup v2 controller '{controller}' for child {} \
                     by writing '+{controller}' to {}: {e}. Ensure the parent \
                     cgroup is delegated/writable and contains no processes when \
                     enabling domain controllers.",
                    path.display(),
                    subtree_control.display()
                ))
            })?;
            log::info!(
                "cgroup: enabled controller {controller} in {}",
                subtree_control.display()
            );
        }
    }
    Ok(())
}

fn read_word_set(path: PathBuf) -> Result<BTreeSet<String>, CgroupError> {
    let content = fs::read_to_string(&path).map_err(|e| CgroupError::Read {
        key: path.display().to_string(),
        source: e,
    })?;
    Ok(content.split_whitespace().map(str::to_string).collect())
}

fn format_word_set(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        "(none)".to_string()
    } else {
        set.iter().cloned().collect::<Vec<_>>().join(",")
    }
}

fn required_controllers(limits: &CgroupLimits) -> BTreeSet<&'static str> {
    limits
        .to_file_map()
        .keys()
        .filter_map(|key| controller_for_key(key))
        .collect()
}

fn controller_for_key(key: &str) -> Option<&'static str> {
    match key.split_once('.').map(|(controller, _)| controller) {
        Some("cpu") => Some("cpu"),
        Some("cpuset") => Some("cpuset"),
        Some("io") => Some("io"),
        Some("memory") => Some("memory"),
        Some("pids") => Some("pids"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_none() {
        let l = CgroupLimits::default();
        assert!(l.is_empty());
    }

    #[test]
    fn limits_round_trip_json() {
        let l = CgroupLimits {
            cpu_weight: Some(100),
            cpu_max: Some("200000 100000".into()),
            cpuset_cpus: Some("2,3".into()),
            memory_max: Some(256 * 1024 * 1024),
            pids_max: Some(64),
            io_weight: None,
            ..Default::default()
        };
        let s = serde_json::to_string(&l).unwrap();
        let back: CgroupLimits = serde_json::from_str(&s).unwrap();
        assert_eq!(back, l);
        assert!(!back.is_empty());
    }

    #[test]
    fn file_map_contains_expected_keys() {
        let l = CgroupLimits {
            cpu_weight: Some(100),
            memory_max: Some(1073741824),
            pids_max: Some(64),
            ..Default::default()
        };
        let map = l.to_file_map();
        assert_eq!(map.get("cpu.weight"), Some(&"100".to_string()));
        assert_eq!(map.get("memory.max"), Some(&"1073741824".to_string()));
        assert_eq!(map.get("pids.max"), Some(&"64".to_string()));
        assert!(!map.contains_key("io.weight"));
    }

    #[test]
    fn empty_limits_produce_empty_map() {
        let l = CgroupLimits::default();
        assert!(l.to_file_map().is_empty());
    }

    #[test]
    fn full_limits_round_trip() {
        let l = CgroupLimits {
            cpu_weight: Some(10000),
            cpu_max: Some("max".into()),
            cpuset_cpus: Some("0-7".into()),
            cpuset_mems: Some("0".into()),
            memory_max: Some(1073741824),
            memory_swap_max: Some(0),
            memory_high: Some(966367641),
            pids_max: Some(256),
            io_weight: Some(500),
            io_max: Some("8:0 rbps=104857600 wbps=104857600 riops=1000 wiops=1000".into()),
        };
        let s = serde_json::to_string(&l).unwrap();
        let back: CgroupLimits = serde_json::from_str(&s).unwrap();
        assert_eq!(back, l);
        assert_eq!(back.to_file_map().len(), 10);
    }
}
