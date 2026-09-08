//! Process spawning and supervision primitives.
//!
//! A service runs in its own **cgroup** (Linux cgroup v2) when available; the
//! spawned process adopts itself into the unit's cgroup before exec, so the
//! kernel tracks the whole tree and `cgroup.kill` reaches every descendant.
//! When cgroups aren't usable the fallback is a **process group**: the
//! spawned process becomes a *session* leader (and therefore a process-group
//! leader), so `kill(-pid, sig)` reaches the tree and the process can acquire
//! a controlling terminal (which `getty`/login programs require). See
//! [`crate::platform::cgroup`] for the cgroup side.
//!
//! `unsafe` is confined to the `pre_exec` closure, which is the one place
//! the language requires it (the API is `unsafe fn pre_exec`). Everything
//! inside it is async-signal-safe: setsid, chdir, setgroups/setgid/setuid,
//! umask, setpriority, setrlimit, and the cgroup self-adopt (open/write/
//! close). User/group *name* lookups (NSS) happen in the parent before
//! spawn and are passed in as uid/gid numbers.

use std::ffi::{CString, OsString};
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use nix::sys::resource::setrlimit;
use nix::sys::signal::{SigSet, SigmaskHow};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Gid, Pid, Uid};
use nix::unistd::{chdir, setgid, setgroups, setsid, setuid};

use crate::platform::signal::Signal;
use crate::unit::{Rlimit, RlimitResource, StdioTarget};

pub type ListenHandle = RawFd;

/// A spawned service process and any captured output pipes.
pub struct Spawned {
    pub pid: i32,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
}

pub struct SpawnOptions {
    pub argv: Vec<String>,
    /// Final environment (already includes the manager's + unit's).
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Supplementary groups for `User=` (resolved in the parent).
    pub groups: Vec<u32>,
    pub nice: Option<i32>,
    pub umask: Option<u32>,
    pub rlimits: Vec<Rlimit>,
    pub stdout_target: StdioTarget,
    pub stderr_target: StdioTarget,
    pub stdin_null: bool,
    /// NOTIFY_SOCKET value, if this is a Type=notify service.
    pub notify_socket: Option<PathBuf>,
    /// Listening socket fds to pass to the child as fd 3..3+n (socket
    /// activation), advertised via LISTEN_FDS/LISTEN_PID.
    pub listen_fds: Vec<RawFd>,
    /// cgroup v2 directory (Linux) the child self-adopts into before exec.
    /// `None` = no cgroup, process-group only.
    pub cgroup: Option<PathBuf>,
    /// Prebuilt sandbox ops (from the unit's `[Service]` sandbox config).
    /// Executed after cgroup adoption, before chdir/priv-drop.
    pub sandbox_ops: Option<Vec<crate::platform::sandbox::Op>>,
}

/// How a reaped child terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExit {
    /// Exited normally with this code.
    Exited(i32),
    /// Killed by this signal number.
    Signaled(i32),
}

/// Resolve a username to a numeric uid + gid + supplementary groups.
pub fn resolve_user(name: &str) -> Option<(u32, u32, Vec<u32>)> {
    let user = nix::unistd::User::from_name(name).ok()??;
    let cname = CString::new(name).ok()?;
    let groups = nix::unistd::getgrouplist(&cname, user.gid)
        .map(|gs| gs.into_iter().map(|g: Gid| g.as_raw()).collect())
        .unwrap_or_default();
    Some((user.uid.as_raw(), user.gid.as_raw(), groups))
}

pub fn resolve_group(name: &str) -> Option<u32> {
    Some(nix::unistd::Group::from_name(name).ok()??.gid.as_raw())
}

pub use crate::expand::{expand_env_argv, expand_env_token};

/// Format a non-negative integer as a NUL-terminated C string in `buf`
/// (async-signal-safe — no allocation). Returns a pointer to the digits.
fn itoa_cstr(buf: &mut [u8], mut v: i32) -> *const libc::c_char {
    let n = buf.len();
    buf[n - 1] = 0; // null terminator
    let mut i = n - 1;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    buf[i..].as_ptr().cast()
}

/// Length of a NUL-terminated byte string. Async-signal-safe (reads until the
/// terminator; no allocation).
fn cstr_len(p: *const u8) -> usize {
    let mut n = 0;
    // SAFETY: `p` points into a valid, NUL-terminated C string owned elsewhere
    // (the caller guarantees a live buffer with a terminator).
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    n
}

/// Overwrite the value of an existing `NAME=value` entry in the child's
/// `environ` in place. Async-signal-safe: only reads `environ` and writes the
/// caller's `value` bytes plus a terminator into the fixed-size `=value` slot
/// that the parent pre-seeded. Returns false (silently) if the key is absent
/// or the value would overflow the slot. Used to set `LISTEN_PID` between fork
/// and exec without the non-async-signal-safe `setenv(3)`.
fn overwrite_env_in_place(key: &[u8], value: &[u8]) -> bool {
    // SAFETY: the child's `environ` is exposed by the C runtime and in scope
    // across the fork/exec performed by std::process::Command. Only reads
    // existing entries and writes `value` plus a terminator into the fixed
    // `=value` slot the parent pre-seeded; no allocation.
    unsafe {
        // The child's environment, guaranteed live across the fork/exec.
        unsafe extern "C" {
            static mut environ: *mut *mut libc::c_char;
        }
        const SLOT_CAP: usize = 10; // matches the parent's "0000000000" pre-seed
        if value.len() > SLOT_CAP {
            return false;
        }
        let mut p = environ;
        while !(*p).is_null() {
            let entry = *p as *const u8;
            let mut j = 0;
            while j < key.len() && *entry.add(j) == key[j] {
                j += 1;
            }
            if j == key.len() {
                let slot = entry.add(key.len()) as *mut u8;
                for (k, &b) in value.iter().enumerate() {
                    *slot.add(k) = b;
                }
                *slot.add(value.len()) = 0;
                return true;
            }
            p = p.add(1);
        }
        false
    }
}

/// Spawn `argv` as a new process-group leader with the given environment and
/// process attributes. The returned pid is the group leader; kill with
/// [`kill_group`].
pub fn spawn(opts: &SpawnOptions) -> std::io::Result<Spawned> {
    let argv = opts.argv.clone();
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);

    let mut env: Vec<(OsString, OsString)> = opts
        .env
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
    if let Some(ns) = &opts.notify_socket {
        env.push((
            OsString::from("NOTIFY_SOCKET"),
            OsString::from(ns.as_os_str()),
        ));
    }
    cmd.envs(env);
    // Socket activation: the listener fds are dup2'd onto 3..3+n in pre_exec
    // (see below). Advertise them via LISTEN_FDS/LISTEN_PID (sd_listen_fds(3)).
    // LISTEN_FDS is set here in the parent (safe). LISTEN_PID is the child's
    // pid, only known between fork and exec; pre-seed a fixed-width slot that
    // pre_exec overwrites in place, because setenv() is not async-signal-safe.
    if !opts.listen_fds.is_empty() {
        cmd.env("LISTEN_FDS", opts.listen_fds.len().to_string());
        cmd.env("LISTEN_PID", "0000000000");
    }

    let (stdout, stderr) = setup_stdio(&opts.stdout_target, &opts.stderr_target);
    if let Some(s) = stdout {
        cmd.stdout(s);
    }
    if let Some(s) = stderr {
        cmd.stderr(s);
    }
    if opts.stdin_null {
        cmd.stdin(Stdio::null());
    }

    let uid = opts.uid;
    let gid = opts.gid;
    // Pre-collect the `Gid` array for `setgroups` so the closure body is a
    // single `&[]` borrow — no `Vec::collect` allocation in pre_exec, which
    // can deadlock on libc's malloc mutex from a multithreaded parent.
    let gids: Vec<Gid> = opts.groups.iter().map(|&g| Gid::from_raw(g)).collect();
    let umaskv = opts.umask;
    let nice = opts.nice;
    let rlimits = opts.rlimits.clone();
    let cwd = opts.cwd.clone();
    let listen_fds = opts.listen_fds.clone();

    // Path to the cgroup's `cgroup.procs`, pre-encoded for the async-signal-
    // safe open() in pre_exec (a Rust `fs::File::open` allocates and is not
    // legal in a pre_exec hook).
    let cgroup_procs: Option<CString> = opts
        .cgroup
        .as_ref()
        .and_then(|d| CString::new(d.join("cgroup.procs").as_os_str().as_bytes()).ok());
    let sandbox_ops = opts.sandbox_ops.clone();

    // The one justified `unsafe` in the spawn path: pre_exec is the only
    // hook between fork and exec. We call only async-signal-safe functions;
    // errors are propagated back through std's exec-status pipe, so a failed
    // attribute turns into an Err here and the child is reaped by std.
    unsafe {
        cmd.pre_exec(move || {
            // Reset the inherited signal mask. The manager blocks SIGTERM /
            // SIGINT / SIGCHLD / … for its signalfd, and a forked child
            // inherits that mask across fork+exec — which would make every
            // service immune to its (default SIGTERM) stop signal. A service
            // must start with a clean, empty signal mask. `sigprocmask` is
            // async-signal-safe, so it is legal in pre_exec.
            if let Err(e) =
                nix::sys::signal::sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
            {
                return Err(std::io::Error::from(e));
            }
            // Own session (the non-cgroup fallback; also matters for
            // KillMode=process semantics). `setsid` makes the child a session
            // leader AND process-group leader, so it can acquire a controlling
            // terminal (busybox getty calls setsid() and fails with EPERM if
            // the process is already a plain process-group leader).
            if let Err(e) = setsid() {
                return Err(std::io::Error::from(e));
            }
            // Socket activation: dup2 the listening fds to 3..3+n and advertise
            // them via LISTEN_FDS/LISTEN_PID (systemd's sd_listen_fds(3)
            // protocol). dup2 clears CLOEXEC on the target fd, so they survive
            // exec; the manager's own copies stay CLOEXEC and close on exec.
            for (i, fd) in listen_fds.iter().enumerate() {
                if libc::dup2(*fd, 3 + i as libc::c_int) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if !listen_fds.is_empty() {
                // LISTEN_FDS was set by the parent via Command::env. LISTEN_PID
                // is the child's pid (known only here); overwrite its pre-seeded
                // "0000000000" slot in place — async-signal-safe memory writes,
                // unlike the setenv() calls this replaces.
                let mut pid_buf = [0u8; 16];
                let pid_ptr = itoa_cstr(&mut pid_buf, libc::getpid());
                let pid_len = cstr_len(pid_ptr.cast());
                overwrite_env_in_place(
                    b"LISTEN_PID=",
                    std::slice::from_raw_parts(pid_ptr.cast(), pid_len),
                );
            }
            // Move this (still pre-exec) process into its cgroup by writing
            // our own pid to `cgroup.procs`. Doing it here — before exec —
            // means every process the service forks later is captured by the
            // kernel, closing the double-fork escape hatch a process group
            // can't. open/write/close are async-signal-safe and the path is
            // pre-encoded in the parent. Best-effort: failure just leaves us
            // in the process-group fallback.
            if let Some(cg) = &cgroup_procs {
                let fd = libc::open(cg.as_ptr(), libc::O_WRONLY);
                if fd >= 0 {
                    let pid = libc::getpid();
                    let mut buf = [0u8; 32];
                    let mut n = pid;
                    let mut i = buf.len();
                    loop {
                        i -= 1;
                        buf[i] = b'0' + (n % 10) as u8;
                        n /= 10;
                        if n == 0 {
                            break;
                        }
                    }
                    libc::write(fd, buf[i..].as_ptr().cast(), buf.len() - i);
                    libc::close(fd);
                }
            }
            // Sandbox (mount namespace + NoNewPrivileges) — after cgroup
            // self-adopt, before any privilege drop, so mount ops still have
            // CAP_SYS_ADMIN. A partial failure (`Err`) aborts the spawn.
            if let Some(ops) = &sandbox_ops
                && let Err(e) = crate::platform::sandbox::apply(ops)
            {
                return Err(std::io::Error::other(e));
            }
            if let Some(dir) = &cwd
                && let Err(e) = chdir(dir)
            {
                return Err(std::io::Error::from(e));
            }
            if !gids.is_empty()
                && let Err(e) = setgroups(&gids)
            {
                return Err(std::io::Error::from(e));
            }
            if let Some(g) = gid
                && let Err(e) = setgid(Gid::from_raw(g))
            {
                return Err(std::io::Error::from(e));
            }
            if let Some(u) = uid
                && let Err(e) = setuid(Uid::from_raw(u))
            {
                return Err(std::io::Error::from(e));
            }
            if let Some(m) = umaskv {
                nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(m));
            }
            if let Some(n) = nice {
                libc::setpriority(libc::PRIO_PROCESS, 0, n);
            }
            for rl in &rlimits {
                let resource = match rl.resource {
                    RlimitResource::NoFile => nix::sys::resource::Resource::RLIMIT_NOFILE,
                    RlimitResource::NProc => nix::sys::resource::Resource::RLIMIT_NPROC,
                    RlimitResource::Core => nix::sys::resource::Resource::RLIMIT_CORE,
                    RlimitResource::AddressSpace => nix::sys::resource::Resource::RLIMIT_AS,
                };
                if let Err(e) = setrlimit(resource, rl.soft, rl.hard) {
                    return Err(std::io::Error::from(e));
                }
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;
    // Take the pipes out before dropping the child handle so they stay open.
    let mut stdout: Option<OwnedFd> = child.stdout.take().map(Into::into);
    let mut stderr: Option<OwnedFd> = child.stderr.take().map(Into::into);
    drop(child);

    set_nonblocking_opt(&mut stdout)?;
    set_nonblocking_opt(&mut stderr)?;

    Ok(Spawned {
        pid,
        stdout,
        stderr,
    })
}

fn set_nonblocking_opt(fd: &mut Option<OwnedFd>) -> std::io::Result<()> {
    if let Some(fd) = fd {
        set_nonblocking(fd)?;
    }
    Ok(())
}

fn set_nonblocking(fd: &OwnedFd) -> std::io::Result<()> {
    let flags =
        nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL).map_err(std::io::Error::from)?;
    nix::fcntl::fcntl(
        fd,
        nix::fcntl::FcntlArg::F_SETFL(
            nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK,
        ),
    )
    .map_err(std::io::Error::from)?;
    Ok(())
}

fn setup_stdio(out: &StdioTarget, err: &StdioTarget) -> (Option<Stdio>, Option<Stdio>) {
    let to_stdio = |t: &StdioTarget| -> Option<Stdio> {
        match t {
            StdioTarget::Journal | StdioTarget::Inherit => Some(Stdio::piped()),
            StdioTarget::Discard => Some(Stdio::null()),
            StdioTarget::File(p) => open_file_stdio(p),
        }
    };
    (to_stdio(out), to_stdio(err))
}

fn open_file_stdio(path: &Path) -> Option<Stdio> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    Some(Stdio::from(f))
}

/// Signal the whole process group led by `group_pid`. A negative pid passed
/// to `kill(2)` addresses the group.
pub fn kill_group(group_pid: i32, sig: Signal) -> std::io::Result<()> {
    let signal = sig.to_nix().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsupported signal {sig}"),
        )
    })?;
    nix::sys::signal::kill(Pid::from_raw(-group_pid), signal).map_err(std::io::Error::from)
}

pub fn group_alive(group_pid: i32) -> bool {
    nix::sys::signal::kill(Pid::from_raw(-group_pid), None).is_ok()
}

/// Reap every exited direct child (non-blocking). Returns `(pid, exit)`
/// pairs. The manager correlates pids back to units; the raw `waitpid` loop
/// is kept here because it is the OS-specific primitive.
pub fn reap_children() -> Vec<(i32, ChildExit)> {
    let mut out = Vec::new();
    loop {
        let status = match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(nix::errno::Errno::ECHILD) => break,
            Ok(status) => status,
            Err(_) => break,
        };
        let Some(pid) = status.pid().map(|p| p.as_raw()) else {
            break;
        };
        let exit = match status {
            WaitStatus::Exited(_, c) => ChildExit::Exited(c),
            WaitStatus::Signaled(_, s, _) => ChildExit::Signaled(s as i32),
            _ => continue,
        };
        out.push((pid, exit));
    }
    out
}

/// Become a subreaper: adopted orphaned grandchildren get reparented to us so
/// we can reap them (matters for `Type=forking`/daemonizing services). No-op
/// when already PID 1 (which is inherently the reaper).
pub fn set_subreaper() {
    if nix::unistd::getpid() == Pid::from_raw(1) {
        return;
    }
    // SAFETY: prctl(PR_SET_CHILD_SUBREAPER, 1) is a single, always-safe
    // syscall with no pointer or memory-safety implications.
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// `itoa_cstr` formats the `LISTEN_FDS`/`LISTEN_PID` env values used for
    /// socket activation; it must be exact across the full `i32` range and
    /// NUL-terminate into the caller's fixed buffer.
    #[test]
    fn itoa_formats_integers_into_fixed_buffer() {
        let mut buf = [0u8; 16];
        for &(v, want) in &[
            (0, "0"),
            (7, "7"),
            (42, "42"),
            (123_456_789, "123456789"),
            (2_147_483_647, "2147483647"),
        ] {
            let p = itoa_cstr(&mut buf, v);
            let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
            assert_eq!(s, want, "itoa_cstr({v})");
        }
    }

    /// User/group resolution is the foundation of `User=`/`Group=` privilege
    /// dropping; `root` must resolve to uid/gid 0 on every Unix.
    #[test]
    fn resolve_root_user_and_group() {
        let (uid, gid, _supp) = resolve_user("root").expect("root must exist");
        assert_eq!(uid, 0);
        assert_eq!(gid, 0);
        assert_eq!(resolve_group("root"), Some(0));
    }
}
