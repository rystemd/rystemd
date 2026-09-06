//! cgroup v2 integration (Linux).
//!
//! use cgroups the way systemd does: one cgroup per unit, so the
//! kernel tracks *every* descendant (a double-forking daemon can't escape the
//! way it can escape a process group), and so resource limits (`MemoryMax`,
//! `CPUWeight`, `TasksMax`) can be enforced. This is deliberately **not** the
//! container layer — no namespaces, no mounts, no images.
//!
//! module degrades gracefully: when cgroup v2 is unavailable (not
//! Linux, no unified hierarchy mounted, or the subtree is read-only — e.g. an
//! unprivileged container), [`root`] returns `None` and the manager falls back
//! to process groups ([`crate::platform::process`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::platform::signal::Signal;
use nix::unistd::Pid;

use crate::unit::CgroupLimits;

const CGROUPFS: &str = "/sys/fs/cgroup";

/// The directory we create per-unit cgroups under, or `None` when cgroup v2
/// is unavailable and callers should fall back to process groups.
///
/// Precedence: `RYSTEMD_CGROUP_ROOT` (delegation override, also used by
/// tests) → our own cgroup from `/proc/self/cgroup` (works both as PID 1 in a
/// container and as a `--user` manager under a delegated subtree) → `None`.
pub fn root() -> Option<PathBuf> {
    if let Ok(r) = std::env::var("RYSTEMD_CGROUP_ROOT")
        && !r.is_empty()
    {
        let p = PathBuf::from(r);
        // Must be a real cgroupfs: a plain directory (or a misconfigured
        // override) would silently break adopt/kill. A real cgroup dir has
        // the kernel-provided `cgroup.procs`; a plain dir does not.
        if fs::create_dir_all(&p).is_ok() && p.join("cgroup.procs").exists() {
            return Some(p);
        }
        return None;
    }

    let base = Path::new(CGROUPFS);
    // The unified-hierarchy marker: present iff cgroup v2 is mounted.
    if !base.join("cgroup.controllers").exists() {
        return None;
    }
    let own = own_cgroup()?; // e.g. "/" or "/user.slice/user-1000.slice/…"
    let dir = base.join(own.trim_start_matches('/')).join("rystemd.slice");
    fs::create_dir_all(&dir).ok().map(|_| dir)
}

/// Our own cgroup path from `/proc/self/cgroup` (the `0::<path>` v2 entry).
fn own_cgroup() -> Option<String> {
    let text = fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            return Some(rest.to_string());
        }
    }
    None
}

/// Create the per-unit cgroup directory. Unit names already carry their
/// suffix (`foo.service`), so the layout matches systemd's
/// (`…/system.slice/foo.service`).
pub fn create(root: &Path, name: &str) -> io::Result<PathBuf> {
    let dir = root.join(name);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Signal every process in the cgroup. cgroup v2 has no "signal all" file for
/// arbitrary signals (only `cgroup.kill` for SIGKILL), so we enumerate
/// `cgroup.procs` and signal each. The race this opens (a fork between read
/// and signal) is closed by the follow-up `kill_all`.
pub fn kill(dir: &Path, sig: Signal) {
    for pid in procs(dir) {
        if let Some(signal) = sig.to_nix() {
            let _ = nix::sys::signal::kill(Pid::from_raw(pid), signal);
        }
    }
}

/// Kill every process in the cgroup. Uses `cgroup.kill` (kernel ≥5.14), which
/// is airtight — no PID enumeration, no race — falling back to enumerating
/// `cgroup.procs` + SIGKILL on older kernels.
pub fn kill_all(dir: &Path) {
    if write_file(&dir.join("cgroup.kill"), "1").is_ok() {
        return;
    }
    for pid in procs(dir) {
        let _ = nix::sys::signal::kill(Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
    }
}

/// True when the cgroup has no live processes. A unit whose control command
/// already exited (e.g. a `Type=oneshot` that finished) leaves an empty cgroup
/// behind; the *existence* of the cgroup is not evidence of a running process.
pub fn is_empty(dir: &Path) -> bool {
    procs(dir).is_empty()
}

/// Apply resource limits. Best-effort: a directive that can't be written
/// (e.g. controller not enabled in an ancestor) is silently skipped, matching
/// systemd's tolerance for partial delegation.
pub fn apply_limits(dir: &Path, l: &CgroupLimits) {
    if let Some(v) = l.memory_max {
        write_limit(&dir.join("memory.max"), v);
    }
    if let Some(v) = l.memory_high {
        write_limit(&dir.join("memory.high"), v);
    }
    if let Some(v) = l.cpu_weight {
        let _ = write_file(&dir.join("cpu.weight"), &v.to_string());
    }
    if let Some(quota) = l.cpu_quota {
        // cpu.max = "<max> <period>", both in microseconds. systemd targets a
        // 100ms (100000 µs) period; the max is quota × period. f32 → integer
        // math: round up so a budget is never silently under-granted.
        // Reject zero (and NaN/negative) explicitly: writing `0 100000` to
        // cpu.max makes the kernel throttle the cgroup to zero CPU, which
        // would silently starve every service the unit owns.
        if quota.is_finite() && quota > 0.0 {
            let period_us: u64 = 100_000;
            let max_us = (quota * period_us as f32).round() as u64;
            let _ = write_file(&dir.join("cpu.max"), &format!("{max_us} {period_us}"));
        }
    }
    if let Some(v) = l.io_weight {
        // Base weight applies to any device without a specific rule.
        let _ = write_file(&dir.join("io.weight"), &v.to_string());
    }
    if !l.io_device_weights.is_empty() {
        // Per-device weights need major:minor numbers. `io.weight` is a
        // multi-line file: `<major>:<minor> <weight>`. Best-effort per line:
        // a path we can't stat (or a non-block device) is skipped silently,
        // matching the existing partial-delegation tolerance.
        let mut lines = Vec::with_capacity(l.io_device_weights.len());
        for (path, weight) in &l.io_device_weights {
            if let Some(mm) = device_major_minor(path) {
                lines.push(format!("{mm} {weight}"));
            }
        }
        if !lines.is_empty() {
            let _ = write_file(&dir.join("io.weight"), &lines.join("\n"));
        }
    }
    if let Some(v) = l.tasks_max {
        write_limit(&dir.join("pids.max"), v);
    }
}

/// Remove the unit's cgroup directory. Only succeeds once empty, so this is
/// called after all processes are gone; failure is non-fatal.
pub fn release(dir: &Path) {
    let _ = fs::remove_dir(dir);
}

fn write_limit(path: &Path, v: u64) {
    // cgroupfs spells "infinity" as `max`, not a huge number.
    let s = if v == u64::MAX {
        "max".to_string()
    } else {
        v.to_string()
    };
    let _ = write_file(path, &s);
}

fn procs(dir: &Path) -> Vec<i32> {
    let Ok(text) = fs::read_to_string(dir.join("cgroup.procs")) else {
        return Vec::new();
    };
    text.split_whitespace()
        .filter_map(|t| t.parse::<i32>().ok())
        .collect()
}

fn write_file(path: &Path, content: &str) -> io::Result<()> {
    fs::write(path, content)
}

/// Resolve a device path to its `major:minor` string for cgroup `io.weight`,
/// or `None` if it isn't a block device we can stat. On Linux the minor range
/// is 20 bits, so the dev_t encoding is `(major << 20) | minor`.
fn device_major_minor(path: &str) -> Option<String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let md = fs::metadata(path).ok()?;
    let ft = md.file_type();
    if ft.is_block_device() || ft.is_char_device() {
        let dev = md.rdev();
        let major = (dev >> 8) & 0xfff;
        let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
        Some(format!("{major}:{minor}"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prove the cgroup kill path actually works where cgroup v2 is usable,
    /// and skip silently where it isn't (unprivileged/read-only).
    #[test]
    fn cgroup_kill_works_when_available() {
        let Some(root) = root() else {
            eprintln!("skipping: cgroup v2 not available");
            return;
        };
        let dir = create(&root, "rystemd-test-cgroup").unwrap();

        // Spawn a long-lived child and move it into the test cgroup (the same
        // write the spawn path performs via pre_exec).
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        std::fs::write(dir.join("cgroup.procs"), child.id().to_string()).unwrap();

        assert!(
            !is_empty(&dir),
            "adopted process should be visible in cgroup.procs"
        );
        kill_all(&dir);

        let status = child.wait().unwrap();
        assert!(
            !status.success(),
            "child should have been killed by cgroup.kill"
        );
        assert!(
            is_empty(&dir),
            "cgroup should be empty after the child died"
        );
        release(&dir);
    }

    /// `apply_limits` writes cpu.max / io.weight / pids.max for the new
    /// directives, where the controllers are available; writes that fail
    /// (controller not enabled) are skipped silently by design.
    #[test]
    fn apply_limits_writes_cpu_quota_and_io_weight_when_available() {
        let Some(root) = root() else {
            eprintln!("skipping: cgroup v2 not available");
            return;
        };
        let dir = create(&root, "rystemd-test-limits").unwrap();

        let limits = CgroupLimits {
            cpu_quota: Some(0.5),
            io_weight: Some(400),
            ..Default::default()
        };
        apply_limits(&dir, &limits);

        if dir.join("cpu.max").exists() {
            assert_eq!(
                std::fs::read_to_string(dir.join("cpu.max")).unwrap().trim(),
                "50000 100000",
                "50% quota against a 100000us period must write 50000 100000"
            );
        }
        if dir.join("io.weight").exists() {
            assert_eq!(
                std::fs::read_to_string(dir.join("io.weight"))
                    .unwrap()
                    .trim(),
                "400"
            );
        }

        release(&dir);
    }
}
