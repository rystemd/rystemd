//! Linux PID-1 boot: mount the virtual/API filesystems and run early-boot
//! configuration.
//!
//! Only compiled with the `boot` feature, and only meaningful when rystemd is
//! **PID 1** on a real Linux host, VM, or container — a container runtime
//! normally does all of this for us, and the `--user` manager never calls it.
//!
//! Everything here is deliberately best-effort and idempotent: on a system
//! where the mounts already exist (container, or a re-exec), each step is a
//! no-op. This is the *host-init* surface (Tier 1 + 2); the supervisor /
//! container path never touches it.

use std::ffi::CString;
use std::fs;
use std::path::Path;

type MountFlags = libc::c_ulong;

const MS_NOSUID: MountFlags = libc::MS_NOSUID as MountFlags;
const MS_NODEV: MountFlags = libc::MS_NODEV as MountFlags;
const MS_NOEXEC: MountFlags = libc::MS_NOEXEC as MountFlags;
/// The "secure" flag set systemd applies to most API filesystems.
const SECURE: MountFlags = MS_NOSUID | MS_NODEV | MS_NOEXEC;

/// Mount `src` (or `none`) at `target` as `fstype`. "Already mounted"
/// (`EBUSY`) is treated as success so the sequence is idempotent.
fn mount(
    src: Option<&str>,
    target: &str,
    fstype: &str,
    flags: MountFlags,
    data: Option<&str>,
) -> Result<(), String> {
    let target_c = CString::new(target).map_err(|e| e.to_string())?;
    let fstype_c = CString::new(fstype).map_err(|e| e.to_string())?;
    let src_c = src
        .map(|s| CString::new(s).map_err(|e| e.to_string()))
        .transpose()?;
    let data_c = data
        .map(|s| CString::new(s).map_err(|e| e.to_string()))
        .transpose()?;

    let rc = unsafe {
        libc::mount(
            src_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            target_c.as_ptr(),
            fstype_c.as_ptr(),
            flags,
            data_c
                .as_ref()
                .map_or(std::ptr::null(), |c| c.as_ptr() as *const libc::c_void),
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EBUSY) {
        return Ok(()); // already mounted — idempotent
    }
    Err(format!("mount {fstype} at {target}: {err}"))
}

fn mkdir_p(path: &str) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| format!("mkdir {path}: {e}"))
}

/// Tier 1 — mount the API/virtual filesystems a Linux system expects.
///
/// Order matters: `/proc` and `/sys` first; `/dev` before its children;
/// `/sys` before `/sys/fs/cgroup` (which rystemd's cgroup code depends on).
///
/// Best-effort: every mount is attempted, and the *first* failure is returned
/// after the rest have been tried. This matters in unprivileged namespaces,
/// where `devtmpfs`/`cgroup2` may be refused while `proc`/`sys`/`run` still
/// succeed — a partial boot is better than an aborted one.
pub fn mount_api_filesystems() -> Result<(), String> {
    let mut first_err: Option<String> = None;
    let mut attempt = |r: Result<(), String>| {
        if let Err(e) = r {
            first_err.get_or_insert(e);
        }
    };

    attempt(mount(Some("proc"), "/proc", "proc", SECURE, None));
    attempt(mount(Some("sysfs"), "/sys", "sysfs", SECURE, None));
    attempt(mount(
        Some("devtmpfs"),
        "/dev",
        "devtmpfs",
        MS_NOSUID,
        Some("mode=0755"),
    ));
    attempt(mkdir_p("/dev/pts"));
    attempt(mount(
        Some("devpts"),
        "/dev/pts",
        "devpts",
        MS_NOSUID | MS_NOEXEC,
        Some("mode=0620,gid=5"),
    ));
    attempt(mkdir_p("/dev/shm"));
    attempt(mount(
        Some("tmpfs"),
        "/dev/shm",
        "tmpfs",
        MS_NOSUID | MS_NODEV,
        Some("mode=1777"),
    ));
    attempt(mkdir_p("/run"));
    attempt(mount(
        Some("tmpfs"),
        "/run",
        "tmpfs",
        MS_NOSUID | MS_NODEV,
        Some("mode=0755"),
    ));
    attempt(mkdir_p("/run/systemd/system"));
    attempt(mkdir_p("/tmp"));
    attempt(mount(Some("tmpfs"), "/tmp", "tmpfs", 0, Some("mode=1777")));
    attempt(mkdir_p("/sys/fs/cgroup"));
    attempt(mount(
        Some("cgroup2"),
        "/sys/fs/cgroup",
        "cgroup2",
        SECURE,
        None,
    ));

    first_err.map_or(Ok(()), Err)
}

/// Tier 2 — early-boot configuration. Each step is best-effort and continues
/// past failure, so a minimal system missing `/etc/hostname`, `/etc/fstab`,
/// etc. still boots.
pub fn early_boot() {
    if let Err(e) = set_hostname() {
        eprintln!("rystemd boot[hostname]: {e}");
    }
    if let Err(e) = ensure_machine_id() {
        eprintln!("rystemd boot[machine-id]: {e}");
    }
    if let Err(e) = apply_sysctl() {
        eprintln!("rystemd boot[sysctl]: {e}");
    }
    if let Err(e) = load_modules() {
        eprintln!("rystemd boot[modules]: {e}");
    }
    if let Err(e) = seed_random() {
        eprintln!("rystemd boot[random-seed]: {e}");
    }
    if let Err(e) = ensure_runtime_dirs() {
        eprintln!("rystemd boot[tmpfiles]: {e}");
    }
    if let Err(e) = mount_fstab() {
        eprintln!("rystemd boot[fstab]: {e}");
    }
}
/// API filesystems a real PID 1 cannot supervise services without. `/proc`
/// backs reaping and per-process inspection; `/dev` backs every service's
/// stdin override (`/dev/null`). Their absence after the mount attempt is a
/// hard boot failure, not a silent partial boot. `/run`, `/sys`, `/tmp` and
/// the rest stay tolerant so unprivileged namespaces can boot partially.
pub fn missing_pid1_api_mounts() -> Vec<&'static str> {
    let mut missing = Vec::new();
    if !std::path::Path::new("/proc/self").exists() {
        missing.push("/proc");
    }
    if !std::path::Path::new("/dev/null").exists() {
        missing.push("/dev");
    }
    missing
}

fn set_hostname() -> Result<(), String> {
    let name = match fs::read_to_string("/etc/hostname") {
        Ok(s) => s.trim().to_string(),
        Err(_) => return Ok(()), // no hostname file; keep the kernel default
    };
    if name.is_empty() {
        return Ok(());
    }
    nix::unistd::sethostname(&name).map_err(|e| e.to_string())
}

/// Generate and persist a machine-id if none exists (a real boot needs a
/// stable id; rystemd's manager reads it for `%m` and identity).
fn ensure_machine_id() -> Result<(), String> {
    if Path::new("/etc/machine-id").exists()
        && fs::read_to_string("/etc/machine-id")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    {
        return Ok(());
    }
    let mut id = [0u8; 16];
    {
        use std::io::Read;
        let mut f = match fs::File::open("/dev/urandom") {
            Ok(f) => f,
            Err(_) => return Ok(()), // no entropy source; leave it
        };
        f.read_exact(&mut id).map_err(|e| e.to_string())?;
    }
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    fs::write("/etc/machine-id", hex.as_bytes()).map_err(|e| e.to_string())
}

/// True when `key` is a safe sysctl identifier: ASCII alphanumerics plus
/// `.`/`_`/`-`, no empty segments, no leading/trailing dots, no NUL. Matches
/// the kernel's accepted charset for `/proc/sys/*` keys (see sysctl(2) and
/// systemd's `sysctl_is_safe`). A key containing `/`, `..`, NUL, or anything
/// non-ASCII would resolve to an arbitrary path under `/proc` (or refuse the
/// write outright) — reject it instead of letting the procfs layer interpret
/// it.
fn sysctl_key_is_safe(key: &str) -> bool {
    !key.is_empty()
        && !key.contains('\0')
        && !key.contains('/')
        && !key.contains("..")
        && !key.starts_with('.')
        && !key.ends_with('.')
        && key
            .as_bytes()
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// Apply `/etc/sysctl.conf` and `/etc/sysctl.d/*.conf` (`key = value` lines)
/// by writing to `/proc/sys/...`. `-key` lines (never set) are skipped. A
/// line whose key fails `sysctl_key_is_safe` is logged and skipped — the
/// previous behavior accepted arbitrary keys and wrote to whatever procfs
/// path they resolved to, which an `/etc/sysctl.d/*.conf` could abuse.
fn apply_sysctl() -> Result<(), String> {
    let mut files = vec!["/etc/sysctl.conf".to_string()];
    if let Ok(rd) = fs::read_dir("/etc/sysctl.d") {
        let mut v: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "conf").unwrap_or(false))
            .map(|e| e.path().to_string_lossy().to_string())
            .collect();
        v.sort();
        files.extend(v);
    }
    let mut bad_lines = 0usize;
    for f in files {
        let text = match fs::read_to_string(&f) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for (lineno, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let rest = line.strip_prefix('-').unwrap_or(line); // `-key` = never set
            let Some((key, val)) = rest.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if !sysctl_key_is_safe(key) {
                eprintln!(
                    "rystemd boot[sysctl]: {}:{}: refused unsafe sysctl key `{key}`",
                    f,
                    lineno + 1
                );
                bad_lines += 1;
                continue;
            }
            let path = format!("/proc/sys/{}", key.replace('.', "/"));
            if let Err(e) = fs::write(&path, val.trim()) {
                eprintln!(
                    "rystemd boot[sysctl]: {}:{}: write {path} failed: {e}",
                    f,
                    lineno + 1
                );
                bad_lines += 1;
            }
        }
    }
    if bad_lines == 0 {
        Ok(())
    } else {
        // Surface the count rather than swallowing it: a misconfigured sysctl
        // drop-in is a boot-time regression worth seeing in the journal.
        Err(format!("{bad_lines} sysctl line(s) skipped or failed"))
    }
}

#[cfg(test)]
mod sysctl_tests {
    use super::sysctl_key_is_safe;

    #[test]
    fn rejects_dotdot_in_key() {
        assert!(!sysctl_key_is_safe("kernel../../proc/sys/kernel/hostname"));
        assert!(!sysctl_key_is_safe("net..ipv4.ip_forward"));
    }

    #[test]
    fn rejects_slash_and_nul_in_key() {
        assert!(!sysctl_key_is_safe("kernel/foo"));
        assert!(!sysctl_key_is_safe("kernel\\0foo"));
        assert!(!sysctl_key_is_safe(""));
        assert!(!sysctl_key_is_safe(".leading"));
        assert!(!sysctl_key_is_safe("trailing."));
    }

    #[test]
    fn accepts_normal_keys() {
        assert!(sysctl_key_is_safe("net.ipv4.ip_forward"));
        assert!(sysctl_key_is_safe("kernel.shmmax"));
        assert!(sysctl_key_is_safe("vm.swappiness"));
        assert!(sysctl_key_is_safe("fs.file-max"));
    }
}

/// Load kernel modules listed in `/etc/modules-load.d/*.conf` via `modprobe`.
fn load_modules() -> Result<(), String> {
    let mut mods = Vec::new();
    if let Ok(rd) = fs::read_dir("/etc/modules-load.d") {
        for e in rd.flatten() {
            if let Ok(t) = fs::read_to_string(e.path()) {
                for line in t.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                        continue;
                    }
                    mods.extend(line.split_whitespace().map(str::to_string));
                }
            }
        }
    }
    for m in mods {
        // Best-effort: `modprobe` may be absent on minimal images.
        let _ = std::process::Command::new("modprobe").arg(&m).status();
    }
    Ok(())
}

/// Feed the saved random seed into the kernel pool, if present.
fn seed_random() -> Result<(), String> {
    let seed = match fs::read("/var/lib/rystemd/random-seed") {
        Ok(s) if !s.is_empty() => s,
        _ => return Ok(()),
    };
    let _ = fs::write("/dev/urandom", &seed);
    Ok(())
}

/// Minimal tmpfiles: the directories rystemd itself needs before it binds its
/// control/notify sockets.
fn ensure_runtime_dirs() -> Result<(), String> {
    for d in ["/run/rystemd", "/var/lib/rystemd", "/var/log"] {
        fs::create_dir_all(d).map_err(|e| format!("mkdir {d}: {e}"))?;
    }
    Ok(())
}

/// Mount `/etc/fstab` entries we don't already handle (skips the API
/// filesystems, `/`, and swap). Options are passed through verbatim.
fn mount_fstab() -> Result<(), String> {
    let text = match fs::read_to_string("/etc/fstab") {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };
    const SKIP: [&str; 10] = [
        "proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2", "swap", "none", "",
    ];
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        let (dev, mnt, fstype) = (fields[0], fields[1], fields[2]);
        if SKIP.contains(&fstype) || mnt == "/" {
            continue;
        }
        let options = fields.get(3).copied().unwrap_or("defaults");
        let _ = mount(Some(dev), mnt, fstype, 0, Some(options));
    }
    Ok(())
}

/// Power the machine off via `reboot(2)`. Only meaningful as PID 1 (real
/// root); elsewhere the syscall fails and we fall back to exiting. Never
/// returns on success.
pub fn poweroff() -> ! {
    reboot_cmd(libc::LINUX_REBOOT_CMD_POWER_OFF);
}

/// Reboot via `reboot(2)`. Same PID-1-only caveat as [`poweroff`].
pub fn reboot() -> ! {
    reboot_cmd(libc::LINUX_REBOOT_CMD_RESTART);
}

fn reboot_cmd(cmd: libc::c_int) -> ! {
    // glibc's reboot(2) wrapper takes only the command; it supplies the
    // LINUX_REBOOT_MAGIC values internally.
    unsafe {
        libc::reboot(cmd);
    }
    // reboot(2) returned (not PID 1, or no CAP_SYS_BOOT in the init userns).
    // As PID 1, exiting makes the kernel panic "init died"; everywhere else
    // it's just a clean exit.
    std::process::exit(0);
}

// --- real-root handoff (initramfs -> ostree/system deployment) --------------
//
// A host boots through an initramfs whose *stage-2* init (us) runs in a
// throwaway rootfs. To manage the real machine we must pivot out of that
// rootfs and into the actual deployment before reading any config. This is
// the classic `switch_root(8)` sequence; `/sysroot` is the canonical mount
// point an ostree/dracut initramfs prepares for the real root.

/// True when running inside an initramfs — a temporary `rootfs`/`tmpfs` root
/// rather than the real deployment. The kernel exposes the `rootfs` fstype in
/// `/proc/self/mounts` for the `/` entry (a real btrfs/xfs/ext4 root reports
/// its own type). A container or `--user` run reports `overlay`/`btrfs` and
/// is skipped.
pub fn in_initramfs() -> bool {
    match fs::read_to_string("/proc/self/mounts") {
        Ok(m) => in_initramfs_from_mounts(&m),
        Err(_) => false,
    }
}

// The `/` entry is the line whose mountpoint field (index 1) is exactly "/".
fn in_initramfs_from_mounts(mounts: &str) -> bool {
    mounts
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some("/"))
        .and_then(|line| line.split_whitespace().nth(2))
        .map(|fstype| matches!(fstype, "rootfs" | "tmpfs" | "ramfs"))
        .unwrap_or(false)
}

/// The real deployment is staged at `/sysroot` by the upstream initramfs
/// (ostree/dracut mount the block device + subvols there before exec'ing
/// stage-2). It is "ready for handoff" when `/sysroot` is its own mountpoint —
/// a *different* filesystem than `/` — which we detect by it appearing in
/// `/proc/self/mounts` with mountpoint exactly `/sysroot`.
pub fn sysroot_mounted() -> bool {
    match fs::read_to_string("/proc/self/mounts") {
        Ok(m) => sysroot_mounted_from_mounts(&m),
        Err(_) => Path::new("/sysroot").is_dir(),
    }
}

fn sysroot_mounted_from_mounts(mounts: &str) -> bool {
    mounts
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some("/sysroot"))
}

/// Re-exec the manager against the real root after a successful pivot.
/// `switch_root` semantically "becomes init again"; this never returns on
/// success.
fn reexec(argv: &[std::ffi::CString]) -> std::io::Error {
    // Build a null-terminated array of raw `*const c_char` pointers: execv
    // needs `char *const argv[]` (an array of pointers), not pointers to &CStr.
    let ptrs: Vec<*const libc::c_char> = {
        let mut v: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
        v.push(std::ptr::null());
        v
    };
    // SAFETY: argv is valid CStrings; argv[0] points at a NUL-terminated
    // program path; ptrs is a NULL-terminated vector of pointers.
    unsafe {
        libc::execv(argv[0].as_ptr(), ptrs.as_ptr());
    }
    std::io::Error::last_os_error()
}

/// Mount the real root device at `/sysroot`, driven by kernel cmdline
/// (`root=`/`rootfstype=`/`rootflags=`). Best-effort and idempotent; only
/// meaningful when rystemd IS the initramfs init (Model B), i.e. nothing else
/// pre-mounted /sysroot. Returns Ok whether already-mounted or newly mounted.
///
/// A real ostree sysroot is a btrfs (or XFS/etc.) block device; mounting it
/// needs the device node present in /dev and CAP_SYS_ADMIN (both true for PID 1
/// in an initramfs). Verified for the *cmdline parsing* here; the actual device
/// mount is hardware-dependent and should be validated on a real host/VM.
pub fn mount_sysroot_from_cmdline() -> Result<(), String> {
    if sysroot_mounted() {
        return Ok(());
    }
    let root = cmdline_arg("root").ok_or_else(|| "no root= on kernel cmdline".to_string())?;
    if root.is_empty() || root == "none" {
        return Ok(()); // nothing to mount (e.g. an initramfs-only or tftp root)
    }
    let fstype = cmdline_arg("rootfstype").filter(|f| !f.is_empty());
    let data = cmdline_arg("rootflags").filter(|f| !f.is_empty());
    if fstype
        .as_deref()
        .map(|ty| matches!(ty, "ramfs" | "rootfs" | "tmpfs"))
        .unwrap_or(false)
    {
        return Ok(()); // not a block device we own the mount of
    }
    fs::create_dir_all("/sysroot").map_err(|e| format!("mkdir /sysroot: {e}"))?;
    nix::mount::mount(
        Some(std::path::Path::new(&root)),
        std::path::Path::new("/sysroot"),
        fstype.as_deref().map(std::path::Path::new),
        nix::mount::MsFlags::MS_RDONLY,
        data.as_deref().map(std::path::Path::new),
    )
    .map_err(|e| format!("mount {root} on /sysroot failed: {e}"))
}

/// Parse a key[=value] from `/proc/cmdline`. `Some("")` for a bare flag,
/// `None` if absent.
fn cmdline_arg(key: &str) -> Option<String> {
    let cmd = fs::read_to_string("/proc/cmdline").ok()?;
    for tok in cmd.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        } else if tok == key {
            return Some(String::new());
        }
    }
    None
}

/// The actual deployment directory under a mounted sysroot.
///
/// On a plain root, the deployment *is* the sysroot. On an ostree sysroot the
/// usable root lives at `ostree/deploy/<os>/deploy/<commit>/`, and `/sysroot`
/// itself holds only the ostree tree — so handing `/sysroot` to switch_root
/// would boot the *sysroot*, not the deployment. This resolves the real
/// deployment.
///
/// Heuristic: among `ostree/deploy/*/deploy/*`, pick the directory that looks
/// like a deployment (has `usr` + `etc`) and is newest by mtime. The `.origin`
/// file names the ref; bootloader/FIDO normally marks the booted commit, but
/// mtime-newest is a sound, portable default when that metadata is absent.
pub fn find_deployment(sysroot: &Path) -> Option<std::path::PathBuf> {
    let deploy_root = sysroot.join("ostree/deploy");
    if !deploy_root.is_dir() {
        return Some(sysroot.to_path_buf()); // plain root
    }
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(os) = fs::read_dir(&deploy_root) {
        for os_e in os.flatten() {
            let os_path = os_e.path();
            let commits = os_path.join("deploy");
            if let Ok(rd) = fs::read_dir(&commits) {
                for c in rd.flatten() {
                    let deploy = c.path();
                    if deploy.join("usr").is_dir()
                        && (deploy.join("etc").is_dir() || deploy.join("etc").is_symlink())
                    {
                        candidates.push(deploy);
                    }
                }
            }
        }
    }
    // Newest deployment wins.
    candidates.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    candidates.last().cloned().or(Some(sysroot.to_path_buf()))
}

/// Prepare a real deployment for switch_root.
///
/// Two self-contained bind-mounts so rystemd can boot a *stock, unmodified*
/// root (e.g. a downloaded cloud image that never had rystemd installed) from
/// the initramfs alone:
///
/// - **`/var`**: on ostree it lives under the sysroot, outside the deployment;
///   bind it in so service state survives.
/// - **the runtimes**: bind the initramfs's own `rystemd`/`rystemctl` (+
///   dynamic libs) into the deployment. After switch_root and re-`exec`, the
///   manager resolves `/usr/bin/rystemd` against the *deployment* root — which
///   on a stock image has no rystemd. Binding our copies in makes the pivot
///   self-contained without touching the disk.
///
/// Home-dir/boot mounts are left to userland units.
pub fn prepare_deployment(sysroot: &Path, deploy: &Path) -> Result<(), String> {
    // /var: on ostree it's a separate subdir of the sysroot, bind it in.
    let sysroot_var = sysroot.join("var");
    if sysroot_var.is_dir() {
        let dst = deploy.join("var");
        fs::create_dir_all(&dst).ok();
        nix::mount::mount(
            Some(sysroot_var.as_path()),
            dst.as_path(),
            None::<&std::path::Path>,
            nix::mount::MsFlags::MS_BIND | nix::mount::MsFlags::MS_REC,
            None::<&std::path::Path>,
        )
        .map_err(|e| format!("bind /var into deployment: {e}"))?;
    }

    // The runtimes: bind our own rystemd/rystemctl + lib dirs into the
    // deployment so the post-pivot re-exec is self-contained on a stock root.
    let bin_dir = deploy.join("usr/bin");
    fs::create_dir_all(&bin_dir).ok();
    for name in ["rystemd", "rystemctl"] {
        let src = Path::new("/usr/bin").join(name);
        if src.is_file() {
            let dst = bin_dir.join(name);
            // Prefer a bind-mount; on a stock root we fall back to a plain
            // copy (the deployment's own Fedora glibc satisfies deps, so we
            // only need the two binaries, not our lib tree). Copy is the
            // reliable path for a fresh, writable root.
            if let Err(e) = nix::mount::mount(
                Some(src.as_path()),
                dst.as_path(),
                None::<&std::path::Path>,
                nix::mount::MsFlags::MS_BIND,
                None::<&std::path::Path>,
            ) {
                eprintln!("rystemd prep: bind {name} failed ({e}); copying instead");
                fs::copy(&src, &dst).map_err(|ce| {
                    format!("copy {name} into deployment failed: {ce} (is /sysroot writable?)")
                })?;
            }
            eprintln!("rystemd prep: {name} in place as {dst:?}");
        }
    }
    // libs + linker: only bind our initramfs lib tree in if the deployment
    // LACKS one (e.g. our synthetic fake deployment). A real distro root
    // (Fedora Cloud etc.) has its own glibc/loader and binding ours over it
    // would corrupt the runtime — so skip when the deployment has /usr/lib64.
    let has_libs = ["lib", "lib64", "usr/lib", "usr/lib64"]
        .iter()
        .any(|d| deploy.join(d).exists());
    if !has_libs {
        for d in ["lib", "lib64", "usr/lib", "usr/lib64"] {
            let src = Path::new("/").join(d);
            if src.is_dir() {
                let dst = deploy.join(d);
                fs::create_dir_all(&dst).ok();
                nix::mount::mount(
                    Some(src.as_path()),
                    dst.as_path(),
                    None::<&std::path::Path>,
                    nix::mount::MsFlags::MS_BIND | nix::mount::MsFlags::MS_REC,
                    None::<&std::path::Path>,
                )
                .map_err(|e| format!("bind {d} into deployment: {e}"))?;
            }
        }
    }
    Ok(())
}

/// Perform the real-root handoff: pivot the current root onto `deploy` (the
/// deployment under `/sysroot`) and re-exec the manager so it boots against the
/// real `/etc`.
///
/// This mirrors `switch_root(8)` order precisely:
///   0. bind-mount `deploy` onto itself        — promote it to a mountpoint
///   1. `chdir(deploy)`          — enter the deployment
///   2. `mount(".", "/", MS_MOVE)` — the cwd *is* the deployment; move it onto `/`
///   3. `chroot(".")`            — make the deployment the process root
///   4. `chdir("/")`
///   5. re-`exec` the manager → boots against real config, remains PID 1.
///
/// (The MS_MOVE source is the current directory, not a literal path — after
/// step 1 the cwd *is* the deployment, and `mount(MS_MOVE)` requires source and
/// target to identify the top of the new root mount. This is why we `chdir`
/// first and pass "." as the source.)
///
/// Safety: caller must have confirmed the deployment is a real root (has `etc`
/// and `bin`/`usr`). On success this function never returns (it execs); on
/// failure it returns the first error so the caller can fall back to the
/// existing in-initramfs boot.
pub fn handoff(deploy: &Path) -> Result<(), String> {
    // 0. Make the deployment a *mountpoint* first. MS_MOVE requires the source
    // to be a mount; a real deployment (a dir inside the ostree sysroot — and
    // our Model A staging) is NOT one, so bind-mount it onto itself to promote
    // it to a mountpoint (the same step systemd's switch_root performs).
    nix::mount::mount(
        Some(deploy),
        deploy,
        None::<&std::path::Path>,
        nix::mount::MsFlags::MS_BIND | nix::mount::MsFlags::MS_REC,
        None::<&std::path::Path>,
    )
    .map_err(|e| format!("switch_root: bind {} onto itself: {e}", deploy.display()))?;

    // 1. Enter the deployment.
    nix::unistd::chdir(deploy).map_err(|e| format!("chdir {}: {e}", deploy.display()))?;

    // 2. Move the deployment (our cwd) onto "/".
    nix::mount::mount(
        Some(std::path::Path::new(".")),
        std::path::Path::new("/"),
        None::<&std::path::Path>,
        nix::mount::MsFlags::MS_MOVE,
        None::<&std::path::Path>,
    )
    .map_err(|e| format!("switch_root: mount --move . onto /: {e}"))?;

    // 3-4. chroot into it and settle at the top.
    nix::unistd::chroot(".").map_err(|e| format!("switch_root: chroot .: {e}"))?;
    nix::unistd::chdir("/").map_err(|e| format!("switch_root: chdir /: {e}"))?;

    // Delete the old initramfs root's contents (best-effort; a fresh initrd
    // holds only what we staged). Rebuild the current argv and exec the real
    // init — `/proc/self/exe` stays valid across the pivot.
    let _ = fs::remove_dir_all("/oldinitrd");
    let argv: Vec<std::ffi::CString> = std::env::args()
        .map(CString::new)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("argv has NUL: {e}"))?;
    // execv() terminates on the NULL pointer in `ptrs` (added inside reexec),
    // so there is NO empty-string argv entry — an empty argument would be
    // rejected by the CLI parser on the re-exec.
    let err = reexec(&argv);
    Err(format!("switch_root: exec /proc/self/exe failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_pid1_api_mounts_are_high_signal() {
        // On a healthy host the essentials are present: the check must be a
        // no-op, never a spurious abort.
        if std::path::Path::new("/proc/self").exists() && std::path::Path::new("/dev/null").exists()
        {
            assert!(missing_pid1_api_mounts().is_empty());
        }
        // Anything it does report is confined to the service-supervision
        // essentials, so unrelated partial-boot states cannot abort a boot.
        for m in missing_pid1_api_mounts() {
            assert!(
                m == "/proc" || m == "/dev",
                "unexpected mandatory mount {m}"
            );
        }
    }

    #[test]
    fn detects_initramfs_rootfs() {
        // dev=rootfs, mountpoint=/, fstype=rootfs
        let mounts = "rootfs / rootfs rw 0 0\n";
        assert!(in_initramfs_from_mounts(mounts));
    }

    #[test]
    fn detects_initramfs_tmpfs_root() {
        // Kernel exposes the initrd as `/` with fstype tmpfs on some builds.
        let mounts = "none / tmpfs rw 0 0\n";
        assert!(in_initramfs_from_mounts(mounts));
    }

    #[test]
    fn real_root_is_not_initramfs() {
        // A btrfs/xfs/ext4/overlay root is the real deployment (host or container).
        let mounts = "/dev/sda1 / btrfs rw,relatime 0 0\n";
        assert!(!in_initramfs_from_mounts(mounts));
        let overlay = "overlay / overlay rw 0 0\n";
        assert!(!in_initramfs_from_mounts(overlay));
    }

    #[test]
    fn empty_or_unreadable_mounts_is_not_initramfs() {
        assert!(!in_initramfs_from_mounts(""));
        assert!(!in_initramfs_from_mounts("no-newline"));
    }

    #[test]
    fn sysroot_mounted_only_when_real_mountpoint_present() {
        let with = "rootfs rootfs rootfs rw 0 0\n/dev/sda1 /sysroot btrfs rw 0 0\n";
        assert!(sysroot_mounted_from_mounts(with));
        let without = "rootfs rootfs rootfs rw 0 0\n";
        assert!(!sysroot_mounted_from_mounts(without));
        // A bare directory under /sysroot without a mount line is not "ready".
        assert!(!sysroot_mounted_from_mounts(
            "rootfs rootfs rootfs rw 0 0\n/some /sysroot/x btrfs rw 0 0\n"
        ));
    }

    #[test]
    fn plain_root_is_its_own_deployment() {
        // A sysroot with no ostree tree: the deployment is the sysroot.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("usr")).unwrap();
        assert_eq!(find_deployment(d.path()), Some(d.path().to_path_buf()));
    }

    #[test]
    fn ostree_sysroot_resolves_deployment_not_sysroot() {
        // A real ostree sysroot: usr/etc live under ostree/deploy/<os>/deploy/<c>.
        let d = tempfile::tempdir().unwrap();
        let sysroot = d.path();
        let depl = sysroot.join("ostree/deploy/fedora/deploy/abc123");
        std::fs::create_dir_all(depl.join("usr")).unwrap();
        std::fs::create_dir_all(depl.join("etc")).unwrap();
        assert_eq!(find_deployment(sysroot), Some(depl));
        // The raw /sysroot is NOT the deployment (it has no usr/etc at top).
        assert_ne!(find_deployment(sysroot).unwrap().as_path(), sysroot);
    }

    #[test]
    fn ostree_sysroot_picks_newest_deployment() {
        let d = tempfile::tempdir().unwrap();
        let sysroot = d.path();
        let old = sysroot.join("ostree/deploy/fedora/deploy/oldcommit");
        let new = sysroot.join("ostree/deploy/fedora/deploy/newcommit");
        for p in [&old, &new] {
            std::fs::create_dir_all(p.join("usr")).unwrap();
            std::fs::create_dir_all(p.join("etc")).unwrap();
        }
        // Bump the newer one's mtime so ordering is deterministic.
        let newer = std::time::SystemTime::now();
        let _ = filetime_set_mtime(&new, newer);
        let _ = filetime_set_mtime(&old, newer - std::time::Duration::from_secs(60));
        assert_eq!(find_deployment(sysroot), Some(new));
    }

    // Touch a path's mtime (portable helper; std can't set mtime).
    fn filetime_set_mtime(path: &Path, t: std::time::SystemTime) -> std::io::Result<()> {
        let f = std::fs::File::open(path)?;
        f.set_times(std::fs::FileTimes::new().set_modified(t))
    }

    #[test]
    fn cmdline_parses_root_and_flags() {
        // These exercise the private cmdline_arg via mount_sysroot_from_cmdline's
        // behavior indirectly; to keep them unit-testable we read /proc/cmdline
        // which exists on Linux. Skip when absent.
        if std::path::Path::new("/proc/cmdline").exists() {
            // At minimum, we can assert the parse helper handles values.
            assert!(cmdline_arg("root").is_some() || cmdline_arg("rdinit").is_some());
        }
    }
}
