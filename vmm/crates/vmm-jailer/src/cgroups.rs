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

/// Enable `limits`' required controllers in every ancestor of `path`, from
/// the cgroup v2 mount root down to `path`'s immediate parent.
///
/// A controller only shows up in a cgroup's own `cgroup.controllers` (i.e.
/// becomes "available" there) once its *parent* has enabled it in the
/// parent's `cgroup.subtree_control`. Enabling only the immediate parent —
/// what this function used to do — silently no-ops whenever a *higher*
/// ancestor (e.g. a systemd service's own delegated cgroup, sitting between
/// the mount root and the per-VM parent dir) never enabled that controller
/// itself: the immediate parent's `cgroup.controllers` stays empty no matter
/// what gets written to its `cgroup.subtree_control`, and `apply_limits`
/// fails with "controller is not available" even though the top-level mount
/// root has always had it. Observed in production: `taritd.service`'s own
/// cgroup (delegated via systemd `Delegate=yes`) has `cpu`/`cpuset`/etc.
/// available but had never enabled them for its own children, so every
/// per-VM cgroup two levels down (`taritd.service/vms/<id>`) failed this
/// check on every restart until something manually re-primed
/// `taritd.service`'s `cgroup.subtree_control` — a step nothing repeated
/// automatically, so it silently regressed on the very next service
/// restart. Walking the whole chain top-down makes that priming permanent
/// and restart-safe, however many delegated levels sit in between.
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

    for level in ancestor_chain_from_cgroup_root(parent) {
        ensure_cgroup2_dir(&level)?;
        let available = read_word_set(level.join("cgroup.controllers"))?;
        let enabled = read_word_set(level.join("cgroup.subtree_control"))?;
        for controller in &required {
            if !available.contains(*controller) {
                return Err(CgroupError::Path(format!(
                    "cgroup v2 controller '{controller}' is not available in \
                     {}/cgroup.controllers (available: {}). Enable/delegate it from \
                     the parent subtree before launching the VMM.",
                    level.display(),
                    format_word_set(&available)
                )));
            }
            if !enabled.contains(*controller) {
                let subtree_control = level.join("cgroup.subtree_control");
                let value = format!("+{controller}");
                fs::write(&subtree_control, value.as_bytes()).map_err(|e| {
                    CgroupError::Path(format!(
                        "failed to enable cgroup v2 controller '{controller}' for child {} \
                         by writing '+{controller}' to {}: {e}. Ensure this cgroup \
                         is delegated/writable and contains no processes when \
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
    }
    Ok(())
}

/// Return `dir` and every cgroup v2 ancestor of it, ordered from the
/// mount-root-most ancestor down to `dir` itself (top-down, matching the
/// order controllers must be enabled in for delegation to propagate).
/// Stops climbing at the first ancestor whose own parent is no longer a
/// cgroup v2 directory (i.e. `dir`'s controlling mount root).
fn ancestor_chain_from_cgroup_root(dir: &Path) -> Vec<PathBuf> {
    let mut chain = vec![dir.to_path_buf()];
    let mut cur = dir.to_path_buf();
    while let Some(parent) = cur.parent() {
        if !parent.join("cgroup.controllers").exists() {
            break;
        }
        chain.push(parent.to_path_buf());
        cur = parent.to_path_buf();
    }
    chain.reverse();
    chain
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

    /// A scratch directory unique to this test process + test name, cleaned
    /// up on drop. Standing in for a cgroup v2 tree: `cgroup.controllers` /
    /// `cgroup.subtree_control` / `cgroup.procs` are plain files here (the
    /// functions under test only ever read/write them as such — real kernel
    /// enforcement of what a child's `cgroup.controllers` reflects isn't
    /// something a plain directory can simulate, so these tests check what
    /// this module actually controls: which levels it visits, in what
    /// order, and what it writes/errors on at each one).
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "vmm-jailer-cgroup-test-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Seed `dir` as a fake cgroup v2 directory with the given `controllers`
    /// (space-separated) available and `subtree_control` already enabled.
    fn seed_cgroup_dir(dir: &Path, controllers: &str, subtree_control: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("cgroup.controllers"), controllers).unwrap();
        fs::write(dir.join("cgroup.subtree_control"), subtree_control).unwrap();
        fs::write(dir.join("cgroup.procs"), "").unwrap();
    }

    #[test]
    fn ancestor_chain_walks_root_to_leaf_and_stops_at_the_mount_boundary() {
        let scratch = ScratchDir::new("ancestor-chain");
        // scratch.path() itself is NOT a cgroup dir (no cgroup.controllers),
        // so it marks the mount boundary the walk must stop at.
        let root = scratch.path().join("root");
        let level1 = root.join("level1");
        let level2 = level1.join("level2");
        for d in [&root, &level1, &level2] {
            seed_cgroup_dir(d, "cpu cpuset memory pids", "");
        }

        let chain = ancestor_chain_from_cgroup_root(&level2);
        assert_eq!(chain, vec![root, level1, level2]);
    }

    #[test]
    fn enable_parent_controllers_enables_every_level_top_down() {
        let scratch = ScratchDir::new("enable-multi-level");
        // Simulates taritd.service (root, "cpu" available but never
        // enabled for its children) -> vms (child, also has "cpu"
        // available for this test's purposes) -> the per-VM leaf whose
        // *parent* (vms) is what enable_parent_controllers is asked about.
        let root = scratch.path().join("taritd.service");
        let vms = root.join("vms");
        seed_cgroup_dir(&root, "cpu cpuset memory pids", "");
        seed_cgroup_dir(&vms, "cpu cpuset memory pids", "");
        let leaf = vms.join("vm-123"); // never created on disk; only used for .parent()

        let limits = CgroupLimits {
            cpu_weight: Some(100),
            ..Default::default()
        };
        enable_parent_controllers(&leaf, &limits).unwrap();

        assert_eq!(
            fs::read_to_string(root.join("cgroup.subtree_control")).unwrap(),
            "+cpu",
            "the higher ancestor (taritd.service) must get enabled, not just the immediate parent"
        );
        assert_eq!(
            fs::read_to_string(vms.join("cgroup.subtree_control")).unwrap(),
            "+cpu"
        );
    }

    #[test]
    fn enable_parent_controllers_skips_writing_when_already_enabled() {
        let scratch = ScratchDir::new("enable-already-on");
        let root = scratch.path().join("root");
        let vms = root.join("vms");
        seed_cgroup_dir(&root, "cpu", "cpu"); // already enabled
        seed_cgroup_dir(&vms, "cpu", "cpu"); // already enabled
        let leaf = vms.join("vm-123");

        let limits = CgroupLimits {
            cpu_weight: Some(100),
            ..Default::default()
        };
        enable_parent_controllers(&leaf, &limits).unwrap();

        // Untouched: still exactly what we seeded, no "+cpu" appended again.
        assert_eq!(
            fs::read_to_string(root.join("cgroup.subtree_control")).unwrap(),
            "cpu"
        );
        assert_eq!(
            fs::read_to_string(vms.join("cgroup.subtree_control")).unwrap(),
            "cpu"
        );
    }

    #[test]
    fn enable_parent_controllers_errors_when_a_higher_ancestor_never_had_the_controller() {
        let scratch = ScratchDir::new("enable-missing-higher");
        let root = scratch.path().join("root");
        let vms = root.join("vms");
        // root never had "cpu" delegated to it at all -- the exact bug this
        // fix targets: the immediate parent (vms) looking fine in isolation
        // doesn't matter if a level above it is the real blocker.
        seed_cgroup_dir(&root, "memory pids", "");
        seed_cgroup_dir(&vms, "cpu memory pids", "");
        let leaf = vms.join("vm-123");

        let limits = CgroupLimits {
            cpu_weight: Some(100),
            ..Default::default()
        };
        let err = enable_parent_controllers(&leaf, &limits).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(root.to_str().unwrap()),
            "error should name the actual blocking ancestor (root), got: {msg}"
        );
    }

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
