//! The manager: unit table, job engine, process supervision, timers, and
//! the event loop. This is "PID 1 in a box" — spawnable as a container init
//! (`--system`) or a per-user manager (`--user`).

pub mod deps;
pub mod ops;
#[cfg(feature = "socket")]
pub mod socket;
pub mod state;
pub mod timer;
pub mod unit_type;

use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::log::mgr_log;
use crate::paths::Paths;
#[cfg(unix)]
use crate::platform::cgroup;
use crate::platform::process as spawn;
use crate::platform::signal::Signal;
use crate::platform::signals::SignalSource;
use crate::repo::Repo;
use crate::specifier::SpecifierContext;
use crate::unit::{
    DirectoryKind, KillMode, PathConfig, RestartPolicy, ServiceConfig, ServiceType, UnitFile,
    UnitKind,
};

use self::deps as D;
#[cfg(feature = "socket")]
use self::socket::{
    SocketId, SocketListener, bind_listen_datagram, bind_listen_netlink,
    bind_listen_sequential_packet, bind_listen_stream,
};
use self::state::ControlCommand as UnitControlCommand;
use self::state::{ActiveState, LoadState, PathState, SubState, TimerState, Unit, UnitResult};
use self::timer::{TimerKind, TimerWheel};
#[cfg(all(target_os = "linux", feature = "udev"))]
use self::unit_type::DeviceUnit;
#[cfg(target_os = "linux")]
use self::unit_type::MountUnit;
#[cfg(feature = "socket")]
use self::unit_type::SocketUnit;
use self::unit_type::{PathUnit, ServiceUnit, TargetUnit, TimerUnit, UnitType};

pub type Name = String;

// ---- configuration ----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ManagerCfg {
    pub user: bool,
    pub paths: Paths,
    pub hostname: String,
    pub machine_id: String,
    pub uid: u32,
    pub username: String,
    pub home: String,
    /// Base environment inherited by services.
    pub base_env: HashMap<String, String>,
    /// Directory for the persistent per-unit journal (disk store).
    pub journal_dir: PathBuf,
    /// Runtime gate for socket activation: when false, `.socket` units load
    /// but bind/listen nothing (and never trigger their service).
    pub socket_activation: bool,
}

impl ManagerCfg {
    pub fn for_mode(user: bool) -> Result<ManagerCfg, String> {
        let paths = if user {
            Paths::user()?
        } else {
            Paths::system()
        };
        #[cfg(unix)]
        let (uid, username, home, hostname, machine_id) = {
            let uid = nix::unistd::geteuid().as_raw();
            let user_entry = nix::unistd::User::from_uid(uid.into()).ok().flatten();
            let username = user_entry
                .as_ref()
                .map(|entry| entry.name.clone())
                .unwrap_or_else(|| "unknown".into());
            let home = user_entry
                .as_ref()
                .map(|entry| entry.dir.to_string_lossy().to_string())
                .or_else(|| std::env::var("HOME").ok())
                .unwrap_or_else(|| "/".into());
            let hostname = nix::unistd::gethostname()
                .ok()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| "localhost".into());
            let machine_id = std::fs::read_to_string("/etc/machine-id")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    std::env::var("RYSTEMD_MACHINE_ID").unwrap_or_else(|_| "unknown".into())
                });
            (uid, username, home, hostname, machine_id)
        };
        #[cfg(windows)]
        let (uid, username, home, hostname, machine_id) = {
            let username = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
            let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".into());
            let hostname = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into());
            let machine_id =
                std::env::var("RYSTEMD_MACHINE_ID").unwrap_or_else(|_| hostname.clone());
            (0, username, home, hostname, machine_id)
        };
        let base_env = std::env::vars().collect();
        let journal_dir = std::env::var_os("RYSTEMD_JOURNAL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if user {
                    std::env::var_os("XDG_STATE_HOME")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from(&home).join(".local").join("state"))
                        .join("rystemd")
                        .join("journal")
                } else {
                    PathBuf::from("/var/log/rystemd")
                }
            });
        Ok(ManagerCfg {
            user,
            paths,
            hostname,
            machine_id,
            uid,
            username,
            home,
            base_env,
            journal_dir,
            socket_activation: true,
        })
    }

    pub fn specifier(&self, unit_name: &str) -> SpecifierContext {
        SpecifierContext {
            unit_name: unit_name.to_string(),
            runtime_dir: self.paths.runtime_dir_spec().to_string_lossy().to_string(),
            user_name: self.username.clone(),
            uid: self.uid.to_string(),
            home: self.home.clone(),
            hostname: self.hostname.clone(),
            machine_id: self.machine_id.clone(),
        }
    }
}

// ---- jobs -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobMode {
    Replace,
    ReplaceIrreversibly,
}

#[derive(Debug, Clone)]
struct WaitEntry {
    unit: String,
    required: bool,
}

#[derive(Debug, Clone)]
struct Job {
    unit: Name,
    kind: JobKind,
    mode: JobMode,
    waiting: Vec<WaitEntry>,
    started: bool,
    failed: bool,
    failed_msg: Option<String>,
    /// For Stop jobs: unit to start once the stop completes.
    start_after_stop: Option<Name>,
    /// True while [`Manager::expand_start_job`] is building this job's
    /// `waiting` list. Guards against re-entrant `process_jobs` runs (e.g.
    /// from a synchronously-failing spawn) starting the job before its
    /// dependencies are known.
    expanding: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct JobStatus {
    pub id: u64,
    pub unit: String,
    pub state: String,
    pub ok: Option<bool>,
    pub error: Option<String>,
}

/// A compact native-IPC representation of a queued manager job.
#[derive(Debug, Clone, serde::Serialize)]
pub struct JobSummary {
    pub id: u64,
    pub unit: String,
    #[serde(rename = "type")]
    pub job_type: String,
    pub state: String,
}

// ---- manager ----------------------------------------------------------------

/// One accepted control connection being read non-blockingly across poll
/// iterations. The control protocol is one request per connection, so a single
/// bounded buffer suffices; the stream is dropped once the request is
/// dispatched and answered.
#[cfg(unix)]
struct PendingClient {
    stream: UnixStream,
    buffer: Vec<u8>,
    /// Remaining response bytes to send. `Some` puts the client in write mode
    /// (drained via `POLLOUT`); `None` leaves it in read mode.
    out: Option<Vec<u8>>,
}

pub struct Manager {
    pub cfg: ManagerCfg,
    /// Persistent per-unit journal (disk store). Appended wherever child
    /// output is captured (Unix `read_stdout` / Windows `drain_windows_output`).
    pub journal: crate::journal::Journal,
    /// Unit-file repository (DAO): the disk source of truth for unit files.
    /// LIST ([`Manager::discover_names`]) and READ ([`Manager::load_unit`])
    /// go through it, and the `repo` IPC query reports its root/backend so
    /// clients can open the same repository with `crate::repo::Repo`.
    pub repo: Repo,
    pub units: HashMap<Name, Unit>,
    jobs: HashMap<u64, Job>,
    completed_jobs: HashMap<u64, JobStatus>,
    unit_job: HashMap<Name, u64>,
    next_job: u64,
    log_level: String,
    pub wheel: TimerWheel,
    pid_unit: HashMap<i32, Name>,
    /// Unix child-output pipes are polled directly. Windows reader threads
    /// forward output through `platform::process::drain_output`.
    #[cfg(unix)]
    pub out_fds: HashMap<RawFd, Name>,
    #[cfg(unix)]
    pub owned_fds: HashMap<RawFd, OwnedFd>,
    #[cfg(unix)]
    control_clients: HashMap<RawFd, PendingClient>,
    #[cfg(feature = "socket")]
    pub socket_listeners: HashMap<SocketId, SocketListener>,
    #[cfg(feature = "socket")]
    pub socket_triggers: HashMap<SocketId, (Name, Name)>,
    #[cfg(unix)]
    listener: Option<UnixListener>,
    #[cfg(windows)]
    listener: Option<crate::platform::net::ControlListener>,
    #[cfg(unix)]
    notify: Option<UnixDatagram>,
    signalfd: Option<SignalSource>,
    pub shutting_down: bool,
    pub boot: SystemTime,
    pub boot_instant: Instant,
    pub as_pid1: bool,
    /// D-Bus bridge (Linux, opt-in `dbus` feature): control interface +
    /// name-ownership events.
    #[cfg(all(target_os = "linux", feature = "dbus"))]
    dbus: Option<crate::dbus::DbusHandle>,
    /// `Type=dbus` units waiting on their `BusName=` (bus name -> unit name).
    #[cfg(all(target_os = "linux", feature = "dbus"))]
    pending_bus_names: HashMap<String, String>,
    /// Live uevent monitor (hotplug add/remove). `None` when unavailable or
    /// before [`Manager::udev_init`] runs.
    #[cfg(all(target_os = "linux", feature = "udev"))]
    pub udev: Option<crate::platform::udev::UdevMonitor>,
    /// The device registry: every known device keyed by sysfs path. This is
    /// the source of truth for `.device` units and survives `load_all`
    /// reloads (which rebuild the transient `units` table from disk).
    #[cfg(all(target_os = "linux", feature = "udev"))]
    udev_devices: HashMap<String, crate::platform::udev::Device>,
}

impl Manager {
    pub fn new(cfg: ManagerCfg) -> Result<Manager, String> {
        crate::platform::process::set_subreaper();
        let repo =
            Repo::open_roots(cfg.paths.unit_path.clone()).map_err(|e| format!("repo: {e}"))?;
        let journal = crate::journal::Journal::new(cfg.journal_dir.clone(), 10 * 1024 * 1024, 5);
        Ok(Manager {
            cfg,
            repo,
            journal,
            units: HashMap::new(),
            jobs: HashMap::new(),
            completed_jobs: HashMap::new(),
            unit_job: HashMap::new(),
            next_job: 0,
            log_level: "info".into(),
            wheel: TimerWheel::default(),
            pid_unit: HashMap::new(),
            #[cfg(unix)]
            out_fds: HashMap::new(),
            #[cfg(unix)]
            owned_fds: HashMap::new(),
            #[cfg(unix)]
            control_clients: HashMap::new(),
            #[cfg(feature = "socket")]
            socket_listeners: HashMap::new(),
            #[cfg(feature = "socket")]
            socket_triggers: HashMap::new(),
            listener: None,
            #[cfg(unix)]
            notify: None,
            signalfd: None,
            shutting_down: false,
            boot: SystemTime::now(),
            boot_instant: Instant::now(),
            as_pid1: {
                #[cfg(unix)]
                {
                    nix::unistd::getpid() == nix::unistd::Pid::from_raw(1)
                }
                #[cfg(windows)]
                {
                    false
                }
            },
            #[cfg(all(target_os = "linux", feature = "dbus"))]
            dbus: None,
            #[cfg(all(target_os = "linux", feature = "dbus"))]
            pending_bus_names: HashMap::new(),
            #[cfg(all(target_os = "linux", feature = "udev"))]
            udev: None,
            #[cfg(all(target_os = "linux", feature = "udev"))]
            udev_devices: HashMap::new(),
        })
    }

    pub fn spec_for(&self, name: &str) -> SpecifierContext {
        self.cfg.specifier(name)
    }

    /// Runtime context used to evaluate `[Unit]` `Condition*`/`Assert*`
    /// directives for a starting unit.
    fn condition_context(&self) -> crate::unit::ConditionContext {
        #[cfg(unix)]
        let (gid, groupname) = {
            let gid = nix::unistd::User::from_uid(self.cfg.uid.into())
                .ok()
                .flatten()
                .map(|u| u.gid)
                .unwrap_or_else(|| nix::unistd::Gid::from_raw(0));
            let groupname = nix::unistd::Group::from_gid(gid)
                .ok()
                .flatten()
                .map(|g| g.name)
                .unwrap_or_else(|| "root".into());
            (gid.as_raw(), groupname)
        };
        #[cfg(not(unix))]
        let (gid, groupname) = (0u32, "root".to_string());

        crate::unit::ConditionContext {
            user_manager: self.cfg.user,
            username: self.cfg.username.clone(),
            uid: self.cfg.uid,
            gid,
            groupname,
            hostname: self.cfg.hostname.clone(),
        }
    }

    fn build_env(&self, u: &Unit) -> HashMap<String, String> {
        let mut env = self.cfg.base_env.clone();
        if let Some(sc) = u.service_cfg() {
            for (k, v) in &sc.environment {
                env.insert(k.clone(), v.clone());
            }
            for (path, ignore) in &sc.environment_files {
                match std::fs::read_to_string(path) {
                    Ok(text) => {
                        for line in text.lines() {
                            let line = line.trim();
                            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                                continue;
                            }
                            if let Some((k, v)) = line.split_once('=') {
                                env.insert(k.trim().to_string(), v.trim().to_string());
                            }
                        }
                    }
                    Err(_) if !*ignore => {
                        mgr_log(&format!("[{}] EnvironmentFile {} missing", u.name, path));
                    }
                    Err(_) => {}
                }
            }
            env.insert("UNIT_NAME".into(), u.name.clone());
        }
        env
    }

    /// Base directory for a `*Directory=` directive's root. System manager
    /// roots mirror systemd (`/run`, `/var/lib`, `/var/cache`, `/var/log`,
    /// `/etc`); the user manager follows XDG. `RYSTEMD_*` env hooks override
    /// each (matching the `paths.rs` convention) so tests can use scratch dirs.
    fn base_dir(&self, kind: DirectoryKind) -> PathBuf {
        let env_or = |key: &str, default: PathBuf| -> PathBuf {
            std::env::var_os(key).map(PathBuf::from).unwrap_or(default)
        };
        let home = PathBuf::from(&self.cfg.home);
        match kind {
            DirectoryKind::Runtime => {
                env_or("RYSTEMD_RUNTIME_DIR", self.cfg.paths.runtime_dir.clone())
            }
            DirectoryKind::State => env_or("RYSTEMD_STATE_DIR", {
                if self.cfg.user {
                    env_or("XDG_STATE_HOME", home.join(".local").join("state"))
                } else {
                    PathBuf::from("/var/lib")
                }
            }),
            DirectoryKind::Cache => env_or("RYSTEMD_CACHE_DIR", {
                if self.cfg.user {
                    env_or("XDG_CACHE_HOME", home.join(".cache"))
                } else {
                    PathBuf::from("/var/cache")
                }
            }),
            DirectoryKind::Logs => env_or("RYSTEMD_LOG_DIR", {
                if self.cfg.user {
                    env_or("XDG_STATE_HOME", home.join(".local").join("state")).join("log")
                } else {
                    PathBuf::from("/var/log")
                }
            }),
            DirectoryKind::Configuration => env_or("RYSTEMD_CONFIG_ROOT", {
                if self.cfg.user {
                    env_or("XDG_CONFIG_HOME", home.join(".config"))
                } else {
                    PathBuf::from("/etc")
                }
            }),
        }
    }

    /// Create and own the unit's `*Directory=` directories (`<root>/<name>`)
    /// before the process turns up: create (recursively if requested), chmod
    /// to the explicit mode or `0755`, then chown to `User=`/`Group=` if set.
    fn apply_directories(&self, sc: &ServiceConfig) -> Result<(), String> {
        for d in &sc.directories {
            let path = self.base_dir(d.kind).join(&d.name);
            if d.recursive {
                std::fs::create_dir_all(&path)
                    .map_err(|e| format!("create {}: {e}", path.display()))?;
            } else if !path.is_dir() {
                std::fs::create_dir(&path)
                    .map_err(|e| format!("create {}: {e}", path.display()))?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = d.mode.unwrap_or(0o755) & 0o7777;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| format!("chmod {}: {e}", path.display()))?;
                if let Some(user) = &sc.user
                    && let Some((uid, primary_gid, _)) = spawn::resolve_user(user)
                {
                    let gid = sc
                        .group
                        .as_deref()
                        .and_then(spawn::resolve_group)
                        .unwrap_or(primary_gid);
                    nix::unistd::chown(
                        std::path::Path::new(&path),
                        Some(nix::unistd::Uid::from_raw(uid)),
                        Some(nix::unistd::Gid::from_raw(gid)),
                    )
                    .map_err(|e| format!("chown {}: {e}", path.display()))?;
                }
            }
        }
        Ok(())
    }

    /// Remove runtime directories on stop; remove state directories only when
    /// empty (matching systemd). Cache/log/config directories persist.
    fn cleanup_directories(&self, dirs: &[crate::unit::DirectorySpec]) {
        for d in dirs {
            let path = self.base_dir(d.kind).join(&d.name);
            match d.kind {
                DirectoryKind::Runtime => {
                    let _ = std::fs::remove_dir_all(&path);
                }
                DirectoryKind::State => {
                    // remove_dir only works on an empty directory.
                    let _ = std::fs::remove_dir(&path);
                }
                DirectoryKind::Cache | DirectoryKind::Logs | DirectoryKind::Configuration => {}
            }
        }
    }

    // ---- unit loading -------------------------------------------------------

    pub fn discover_names(&self) -> Vec<String> {
        let mut names: HashSet<String> = HashSet::new();
        // Unit files come from the repository DAO (all unit-path directories,
        // precedence-merged). The manager recognizes only the suffixes its
        // build supports, preserving the existing feature gates.
        if let Ok(units) = self.repo.list() {
            for uf in units {
                let f = &uf.name;
                if f.ends_with(".service")
                    || f.ends_with(".timer")
                    || f.ends_with(".target")
                    || f.ends_with(".path")
                    || (cfg!(feature = "socket") && f.ends_with(".socket"))
                    || (cfg!(target_os = "linux") && f.ends_with(".mount"))
                {
                    names.insert(uf.name);
                }
            }
        }
        // Wants/requires dirs imply units.
        for dir in &self.cfg.paths.unit_path {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in rd.flatten() {
                let fname = e.file_name().to_string_lossy().to_string();
                if let Some(base) = fname.strip_suffix(".wants") {
                    names.insert(format!("{base}.target"));
                    if let Ok(rd2) = std::fs::read_dir(e.path()) {
                        for d in rd2.flatten() {
                            if let Some(n) = d.file_name().to_str() {
                                names.insert(n.to_string());
                            }
                        }
                    }
                } else if let Some(base) = fname.strip_suffix(".requires") {
                    names.insert(format!("{base}.target"));
                }
            }
        }
        names.insert("basic.target".into());
        #[cfg(feature = "boot")]
        {
            names.insert("sysinit.target".into());
            names.insert("graphical.target".into());
            names.insert("getty.target".into());
        }
        names.insert("multi-user.target".into());
        names.insert("default.target".into());
        if let Ok(md) = std::fs::read_link(self.cfg.paths.default_target())
            && let Some(n) = md.file_name().and_then(|f| f.to_str())
        {
            names.insert(n.to_string());
        }
        let mut v: Vec<String> = names.into_iter().collect();
        v.sort();
        v
    }

    pub fn load_all(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        let names = self.discover_names();
        let mut next: HashMap<Name, Unit> = HashMap::new();
        for name in names {
            match self.load_unit(&name) {
                Ok(Some(mut unit)) => {
                    // Emit a compat warning for recognized-but-unimplemented
                    // sandbox directives (one per unit per load).
                    #[cfg(target_os = "linux")]
                    if let Some(svc) = unit.service_cfg() {
                        for (k, v) in svc.sandbox.compat_warnings() {
                            self.mgr(&name, &format!("{k}={v} is not yet supported; ignoring"));
                        }
                    }
                    // Preserve runtime state for still-active units.
                    if let Some(old) = self.units.get(&name)
                        && old.active != ActiveState::Inactive
                    {
                        unit.main_pid = old.main_pid;
                        unit.group_pid = old.group_pid;
                        unit.cgroup = old.cgroup.clone();
                        unit.control_pid = old.control_pid;
                        unit.control_command = old.control_command;
                        unit.active = old.active;
                        unit.sub = old.sub;
                        unit.log = old.log.clone();
                    }
                    next.insert(name, unit);
                }
                Ok(None) => {
                    // A dependency reference (e.g. a dangling `.wants` dir
                    // symlink) to a unit with no backing file. systemd silently
                    // ignores these — the dependency is simply not activated —
                    // so we skip the name rather than recording a load error.
                }
                Err(e) => {
                    let mut u = Unit::new(&name, unit_kind_of(&name));
                    u.load = LoadState::Error;
                    u.load_error = Some(e.clone());
                    let msg = format!("{name}: {e}");
                    next.insert(name, u);
                    errors.push(msg);
                }
            }
        }
        self.units = next;
        // `.device` units are runtime-generated (never parsed from disk), so a
        // reload would drop them. Re-register from the device registry.
        #[cfg(all(target_os = "linux", feature = "udev"))]
        {
            let devices: Vec<crate::platform::udev::Device> =
                self.udev_devices.values().cloned().collect();
            for dev in devices {
                self.udev_register(&dev);
            }
        }
        self.rearm_all_timers();
        errors
    }

    /// Load one unit from disk.
    ///
    /// Returns `Ok(None)` when the unit has no backing file and is not a
    /// builtin/synthesizable target. Such names come from dependency
    /// references (e.g. a dangling `.wants` dir symlink) and are silently
    /// ignored by systemd rather than treated as a load error.
    fn load_unit(&self, name: &str) -> Result<Option<Unit>, String> {
        let kind = unit_kind_of(name);
        if kind == UnitKind::Target && self.cfg.paths.find_unit(name).is_none() && !is_builtin(name)
        {
            return Ok(Some(builtin_target(name)));
        }

        let mut raw = crate::unit::parse::RawUnitFile { sections: vec![] };
        let mut path: Option<PathBuf> = None;
        let spec = self.cfg.specifier(name);

        // Load main file if it exists; synthesized builtins have none. The
        // file content is read through the repository DAO.
        let has_main = self.cfg.paths.find_unit(name).is_some();
        if let Some(main) = self.cfg.paths.find_unit(name) {
            let parsed = self
                .read_unit_file(&main)
                .map_err(|e| format!("parse error: {e}"))?;
            raw.sections.extend(parsed.sections);
            path = Some(main);
        } else if !is_builtin(name) && !kind_unit_needs_file(kind) {
            // No backing file and not a builtin/synthesizable unit: a
            // dependency reference to a unit that does not exist on this
            // host. systemd treats this as a no-op (the dependency is simply
            // not activated) rather than a load error.
            return Ok(None);
        }

        for dropin in self.cfg.paths.dropins(name) {
            match self.read_unit_file(&dropin) {
                Ok(d) => raw.sections.extend(d.sections),
                Err(e) => return Err(format!("drop-in error: {e}")),
            }
        }

        let mut file = crate::unit::build(&raw, &spec)?;
        // Wants/requires dirs contribute implicit dependencies.
        file.unit
            .wants
            .extend(self.cfg.paths.dir_deps(name, "wants"));
        file.unit
            .requires
            .extend(self.cfg.paths.dir_deps(name, "requires"));

        if kind == UnitKind::Target && !has_main {
            // Synthesized default.lower: default.target wants multi-user.target.
            if name == "default.target" && file.unit.unit_defaults_empty() {
                file.unit.wants.push("multi-user.target".into());
                file.unit.after.push("multi-user.target".into());
            }
            // Builtin empty targets get a description.
            if name == "basic.target" {
                file.unit.description = "Basic System".into();
            }
            if name == "multi-user.target" {
                file.unit.description = "Multi-User System".into();
            }
        }

        let mut unit = Unit::new(name, file.kind());
        unit.load = LoadState::Loaded;
        unit.path = path;
        unit.file = Some(file);
        Ok(Some(unit))
    }

    /// Read and parse a unit file (or drop-in) by its resolved path, reading
    /// the raw bytes through the repository DAO. The daemon owns systemd path
    /// semantics (search precedence and `getty@tty1` -> `getty@.service`
    /// template instantiation) via [`Paths::find_unit`], so it resolves the
    /// path and then reads the content through the repository here.
    fn read_unit_file(
        &self,
        path: &std::path::Path,
    ) -> Result<crate::unit::parse::RawUnitFile, String> {
        let definition = self
            .repo
            .read_path(path)
            .map_err(|e| format!("can't read {}: {e}", path.display()))?;
        Ok(crate::unit::parse::from_repository_document(
            definition.document,
        ))
    }

    // ---- IPC plumbing -------------------------------------------------------

    pub fn control_socket_path(&self) -> PathBuf {
        let paths = self.cfg.paths.clone();
        paths.control_socket()
    }

    pub fn bind_ipc(&mut self) -> Result<(), String> {
        let path = self.control_socket_path();
        self.listener = Some(crate::platform::net::bind_control(&path, self.cfg.user)?);
        Ok(())
    }

    pub fn bind_notify(&mut self) -> Result<(), String> {
        #[cfg(unix)]
        {
            let path = self.cfg.paths.notify_socket();
            self.notify = Some(crate::platform::net::bind_notify(&path)?);
        }
        Ok(())
    }

    #[cfg(unix)]
    fn drain_control_client(&mut self, fd: RawFd) {
        use std::io::Read;
        // This client may already be draining a response (`POLLOUT` also woke
        // us). Flush it instead of treating it as a new request.
        if self
            .control_clients
            .get(&fd)
            .map(|c| c.out.is_some())
            .unwrap_or(false)
        {
            self.flush_control_response(fd);
            return;
        }
        // Bound a single client's buffered request so a slow or abusive writer
        // cannot grow manager memory without limit.
        const MAX_REQUEST: usize = 16 * 1024;

        let mut tmp = [0u8; 1024];
        loop {
            let n = match self
                .control_clients
                .get_mut(&fd)
                .map(|c| c.stream.read(&mut tmp))
            {
                Some(Ok(0)) => {
                    // Peer closed without a complete request.
                    self.control_clients.remove(&fd);
                    return;
                }
                Some(Ok(n)) => n,
                Some(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Some(Err(_)) => {
                    self.control_clients.remove(&fd);
                    return;
                }
                None => return,
            };
            if let Some(c) = self.control_clients.get_mut(&fd) {
                c.buffer.extend_from_slice(&tmp[..n]);
            }
        }

        let Some(newline) = self
            .control_clients
            .get(&fd)
            .and_then(|c| c.buffer.iter().position(|&b| b == b'\n'))
        else {
            // No complete request yet. Drop an overlong partial line rather
            // than buffer it forever.
            if self
                .control_clients
                .get(&fd)
                .map(|c| c.buffer.len() >= MAX_REQUEST)
                .unwrap_or(false)
            {
                self.control_clients.remove(&fd);
            }
            return;
        };

        let Some(client) = self.control_clients.remove(&fd) else {
            return;
        };
        let line = String::from_utf8_lossy(&client.buffer[..newline]).into_owned();
        let resp = crate::ipc::dispatch(self, &line);
        let mut out = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into());
        out.push('\n');

        // Bound the response we keep resident per client: a slow or stalled
        // peer can otherwise hold an arbitrarily large JSON payload (think
        // `list-units` against thousands of units) in manager memory until
        // it finishes reading. 1 MiB is well above any realistic single
        // response; the cap turns pathological peers into dropped clients.
        const MAX_PENDING_RESPONSE: usize = 1024 * 1024;
        let out = if out.len() > MAX_PENDING_RESPONSE {
            // Over-cap: tell the peer with a small error payload instead of
            // keeping the giant one, and drop the client.
            let truncated = serde_json::json!({
                "ok": false,
                "error": format!(
                    "response truncated: {} bytes exceeded cap {}",
                    out.len(),
                    MAX_PENDING_RESPONSE
                ),
            });
            let mut t = serde_json::to_string(&truncated).unwrap_or_else(|_| "{}".into());
            t.push('\n');
            t.into_bytes()
        } else {
            out.into_bytes()
        };

        // Re-register the client in write mode and deliver the response via
        // `POLLOUT`, so a slow or non-reading peer never blocks the loop.
        self.control_clients.insert(
            fd,
            PendingClient {
                stream: client.stream,
                buffer: Vec::new(),
                out: Some(out),
            },
        );
        self.flush_control_response(fd);
    }

    #[cfg(unix)]
    fn flush_control_response(&mut self, fd: RawFd) {
        use std::io::Write;
        let mut remove = false;
        while let Some(pending) = self.control_clients.get_mut(&fd) {
            let out = match pending.out.as_mut() {
                Some(o) => o,
                None => return,
            };
            match pending.stream.write(out) {
                Ok(0) => {
                    remove = true;
                    break;
                }
                Ok(n) => {
                    out.drain(..n);
                    if out.is_empty() {
                        remove = true;
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    remove = true;
                    break;
                }
            }
        }
        if remove {
            self.control_clients.remove(&fd);
        }
    }

    // ---- public control entry points ----------------------------------------

    pub fn start(&mut self, name: &str) -> Result<(), String> {
        self.start_with_mode(name, JobMode::Replace).map(|_| ())
    }

    fn ensure_unit_loaded(&mut self, name: &str) -> Result<(), String> {
        if self.units.contains_key(name) {
            return Ok(());
        }
        let Some(unit) = self.load_unit(name)? else {
            return Err(format!("Unit {name} not found."));
        };
        self.units.insert(name.to_string(), unit);
        Ok(())
    }

    pub fn start_with_mode(&mut self, name: &str, mode: JobMode) -> Result<Option<u64>, String> {
        self.ensure_unit_loaded(name)?;
        // Once shutdown has begun, refuse new starts: a socket-activation
        // trigger (or any other edge) that fires mid-shutdown would otherwise
        // restart units faster than `shutdown()` stops them and prevent the
        // manager from ever reaching `idle()`.
        if self.shutting_down {
            return Ok(None);
        }
        if !self.units.contains_key(name) {
            return Err(format!("Unit {name} not found."));
        }
        if self.unit_active(name) {
            return Ok(None);
        }
        if let Some(jid) = self.unit_job.get(name).copied()
            && let Some(job) = self.jobs.get(&jid)
            && job.kind == JobKind::Start
        {
            return Ok(Some(jid));
        }
        if let Some(jid) = self.unit_job.get(name).copied()
            && let Some(job) = self.jobs.get(&jid)
            && matches!(job.kind, JobKind::Stop | JobKind::Restart)
        {
            if job.mode == JobMode::ReplaceIrreversibly {
                return Err(format!("Job for {name} is irreversible."));
            }
            self.cancel_job(jid, "Job canceled by a replacement start.");
            self.units.get_mut(name).unwrap().set_active(
                ActiveState::Active,
                SubState::Exited,
                UnitResult::Success,
            );
            return Ok(None);
        }
        let id = self.enqueue_start_job_mode(name, mode);
        self.process_jobs();
        Ok(Some(id))
    }

    /// Start a unit without expanding its dependency closure. This is the
    /// useful subset of systemd's `--job-mode=ignore-dependencies` for the
    /// native control socket.
    pub fn start_ignore_dependencies(&mut self, name: &str) -> Result<(), String> {
        if !self.units.contains_key(name) {
            return Err(format!("Unit {name} not found."));
        }
        if self.unit_active(name) {
            return Ok(());
        }
        if let Some(jid) = self.unit_job.remove(name) {
            self.jobs.remove(&jid);
            for job in self.jobs.values_mut() {
                job.waiting.retain(|wait| wait.unit != name);
            }
        }
        let id = self.new_job(JobKind::Start, name, vec![], JobMode::Replace);
        self.maybe_start_job(id);
        self.process_jobs();
        Ok(())
    }

    pub fn stop(&mut self, name: &str) -> Result<(), String> {
        self.stop_with_mode(name, JobMode::Replace).map(|_| ())
    }

    pub fn stop_with_mode(&mut self, name: &str, mode: JobMode) -> Result<Option<u64>, String> {
        if !self.units.contains_key(name) {
            return Err(format!("Unit {name} not found."));
        }
        if !self.unit_operational(name) {
            return Ok(None);
        }
        if let Some(jid) = self.unit_job.get(name).copied() {
            let kind = self.jobs.get(&jid).map(|j| j.kind);
            if kind == Some(JobKind::Stop) || kind == Some(JobKind::Restart) {
                if mode == JobMode::ReplaceIrreversibly {
                    self.jobs.get_mut(&jid).unwrap().mode = mode;
                }
                return Ok(Some(jid));
            }
            if kind == Some(JobKind::Start) {
                // Cancel the pending start and stop instead.
                self.cancel_job(jid, "Job canceled by a replacement stop.");
            }
        }
        let id = self.enqueue_stop_job_after_mode(name, None, mode);
        self.process_jobs();
        Ok(Some(id))
    }

    pub fn restart(&mut self, name: &str) -> Result<(), String> {
        if !self.units.contains_key(name) {
            return Err(format!("Unit {name} not found."));
        }
        #[cfg(feature = "socket")]
        let activated_service = (self.units[name].kind == UnitKind::Socket)
            .then(|| self.units[name].activated_service());
        if self.unit_operational(name) {
            if let Some(jid) = self.unit_job.get(name).copied() {
                let kind = self.jobs.get(&jid).map(|j| j.kind);
                if kind == Some(JobKind::Restart) {
                    return Ok(());
                }
                if kind == Some(JobKind::Start) {
                    self.unit_job.remove(name);
                    self.jobs.remove(&jid);
                }
            }
            self.enqueue_stop_job_after(name, Some(name.to_string()));
        } else {
            return self.start(name);
        }
        self.process_jobs();
        #[cfg(feature = "socket")]
        if let Some(service) = activated_service
            && self.unit_operational(&service)
        {
            self.restart(&service)?;
        }
        Ok(())
    }

    pub fn reload(&mut self, name: &str) -> Result<(), String> {
        if !self.units.contains_key(name) {
            return Err(format!("Unit {name} not found."));
        }
        let has_reload = self.units[name]
            .service_cfg()
            .map(|s| !s.exec_reload.is_empty())
            .unwrap_or(false);
        if !has_reload {
            return Err(format!("Unit {name} has no ExecReload."));
        }
        self.spawn_control(name, UnitControlCommand::Reload, 0);
        Ok(())
    }

    pub fn kill(&mut self, name: &str, sig: Signal) -> Result<(), String> {
        if !self.unit_has_processes(name) {
            return Err(format!("Unit {name} has no processes."));
        }
        self.kill_tree(name, sig);
        Ok(())
    }

    /// Does this unit currently have a live process tree to signal? A main
    /// pid, a process-group leader, or a *non-empty* cgroup all count; a
    /// lingering empty cgroup (oneshot that already exited) does not.
    pub(crate) fn unit_has_processes(&self, name: &str) -> bool {
        let u = &self.units[name];
        u.main_pid.is_some() || u.group_pid.is_some() || {
            #[cfg(unix)]
            {
                u.cgroup
                    .as_ref()
                    .map(|dir| !cgroup::is_empty(dir))
                    .unwrap_or(false)
            }
            #[cfg(windows)]
            {
                false
            }
        }
    }

    /// Create (or reuse) the unit's cgroup and apply its resource limits.
    /// Returns `None` when cgroup v2 is unavailable; callers fall back to
    /// process groups.
    fn ensure_cgroup(&mut self, name: &str) -> Option<PathBuf> {
        #[cfg(unix)]
        {
            if let Some(dir) = self.units[name].cgroup.clone() {
                return Some(dir);
            }
            let root = cgroup::root()?;
            let dir = cgroup::create(&root, name).ok()?;
            if let Some(service) = self.units[name].service_cfg() {
                cgroup::apply_limits(&dir, &service.cgroup_limits);
            }
            self.units.get_mut(name).unwrap().cgroup = Some(dir.clone());
            Some(dir)
        }
        #[cfg(windows)]
        {
            let _ = name;
            None
        }
    }

    /// Signal the whole process tree of a unit: the cgroup when present,
    /// else the process group.
    pub(crate) fn kill_tree(&self, name: &str, sig: Signal) {
        #[cfg(unix)]
        if let Some(dir) = self.units.get(name).and_then(|unit| unit.cgroup.clone()) {
            cgroup::kill(&dir, sig);
            return;
        }
        if let Some(group) = self.units.get(name).and_then(|unit| unit.group_pid) {
            spawn::kill_group(group, sig).ok();
        }
    }

    /// SIGKILL the whole process tree (cgroup.kill when available).
    fn kill_tree_kill(&self, name: &str) {
        #[cfg(unix)]
        if let Some(dir) = self.units.get(name).and_then(|unit| unit.cgroup.clone()) {
            cgroup::kill_all(&dir);
            return;
        }
        if let Some(group) = self.units.get(name).and_then(|unit| unit.group_pid) {
            spawn::kill_group(group, Signal::SIGKILL).ok();
        }
    }

    pub fn shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        // Drop pending service timers so the manager can reach `idle()` and
        // exit. A re-arming monotonic/calendar timer would otherwise keep
        // `has_service_timers()` true forever and block poweroff. Stop
        // timeouts are re-armed as units stop below and are excluded from
        // `has_service_timers()` anyway.
        self.wheel = TimerWheel::default();
        let names: Vec<String> = self
            .units
            .iter()
            .filter(|(_, u)| u.active != ActiveState::Inactive)
            .map(|(n, _)| n.clone())
            .collect();
        for n in names {
            self.stop(&n).ok();
        }
        self.process_jobs();
    }

    pub fn idle(&mut self) -> bool {
        self.jobs.is_empty()
            && !self.wheel.has_service_timers()
            // Windows output readers run on worker threads. Keep the manager
            // alive until they have reached EOF, so shutdown drains tail data.
            && {
                #[cfg(windows)]
                {
                    !spawn::output_pending()
                }
                #[cfg(not(windows))]
                {
                    true
                }
            }
    }

    /// Return the jobs currently queued in the manager.
    pub fn list_jobs(&self) -> Vec<JobSummary> {
        let mut ids: Vec<u64> = self.jobs.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| {
                let job = self.jobs.get(&id)?;
                Some(JobSummary {
                    id,
                    unit: job.unit.clone(),
                    job_type: job_kind_str(job.kind).to_string(),
                    state: if job.waiting.is_empty() {
                        "running".into()
                    } else {
                        "waiting".into()
                    },
                })
            })
            .collect()
    }

    pub fn job_status(&self, ids: &[u64]) -> Vec<JobStatus> {
        ids.iter()
            .filter_map(|id| {
                if let Some(job) = self.jobs.get(id) {
                    return Some(JobStatus {
                        id: *id,
                        unit: job.unit.clone(),
                        state: "pending".into(),
                        ok: None,
                        error: None,
                    });
                }
                self.completed_jobs.get(id).cloned()
            })
            .collect()
    }

    /// Query the selected manager log level. Dynamic filtering is not yet
    /// implemented, but the control state is real and validated.
    pub fn log_level(&self) -> &str {
        &self.log_level
    }

    pub fn set_log_level(&mut self, level: &str) -> Result<(), String> {
        let level = level.to_ascii_lowercase();
        match level.as_str() {
            "emerg" | "alert" | "crit" | "err" | "warning" | "notice" | "info" | "debug" => {
                self.log_level = level;
                Ok(())
            }
            _ => Err(format!("unknown log level `{level}`")),
        }
    }

    // ---- job engine ---------------------------------------------------------

    fn new_job(
        &mut self,
        kind: JobKind,
        unit: &str,
        waiting: Vec<WaitEntry>,
        mode: JobMode,
    ) -> u64 {
        self.next_job += 1;
        let id = self.next_job;
        self.jobs.insert(
            id,
            Job {
                unit: unit.to_string(),
                kind,
                mode,
                waiting,
                started: false,
                failed: false,
                failed_msg: None,
                start_after_stop: None,
                expanding: false,
            },
        );
        self.unit_job.insert(unit.to_string(), id);
        id
    }

    fn enqueue_start_job(&mut self, name: &str) -> u64 {
        self.enqueue_start_job_mode(name, JobMode::Replace)
    }

    fn enqueue_start_job_mode(&mut self, name: &str, mode: JobMode) -> u64 {
        let id = self.new_job(JobKind::Start, name, vec![], mode);
        self.expand_start_job(id);
        id
    }

    fn expand_start_job(&mut self, id: u64) {
        let name = self.jobs[&id].unit.clone();

        // A dependency can name a unit that isn't loaded (missing unit file).
        // Fail the job cleanly — like systemd's "Unit X not found" — instead
        // of panicking on `self.units[&name]` below.
        if !self.units.contains_key(&name) {
            let job = self.jobs.get_mut(&id).unwrap();
            job.failed = true;
            job.failed_msg = Some(format!("Unit {name} not found."));
            self.maybe_start_job(id);
            return;
        }

        // [Unit] Condition*/Assert* gate startup (systemd skip-vs-fail
        // semantics), resolved here before any dependency expansion.
        //
        // A failing *condition* skips activation: the start job is treated as
        // satisfied so it never blocks Requires=/Wants=/After= dependents, and
        // the unit stays inactive (not failed). A failing *assert* fails the
        // unit exactly like a hard start error.
        let conditions = self.units[&name]
            .file
            .as_ref()
            .map(|f| f.unit.conditions.clone())
            .unwrap_or_default();
        if !conditions.is_empty() {
            let ctx = self.condition_context();
            if let Some(bad) = conditions.iter().find(|c| !c.evaluate(&ctx)) {
                if bad.is_assert {
                    self.units.get_mut(&name).unwrap().result = UnitResult::Assert;
                    self.fail_unit(
                        &name,
                        format!("Assert {} failed, refusing to start.", bad.kind.name()),
                    );
                } else {
                    self.mgr(
                        &name,
                        &format!("Condition {} failed, skipping {name}.", bad.kind.name()),
                    );
                    self.finish_job(id);
                }
                return;
            }
        }

        // Mark this job as mid-expansion so a re-entrant `process_jobs` (e.g.
        // a dependency that fails to spawn synchronously) cannot start it
        // before its `waiting` list is finalised below.
        self.jobs.get_mut(&id).unwrap().expanding = true;

        let (needs, weak, requisite) = D::start_closure(&self.units, &name);

        let mut waiting: Vec<WaitEntry> = Vec::new();

        // Requisite must already be active.
        for r in requisite {
            if self.unit_active(&r) {
                continue;
            }
            if let Some(jid) = self.unit_job.get(&r)
                && self.jobs[jid].kind == JobKind::Start
            {
                waiting.push(WaitEntry {
                    unit: r,
                    required: true,
                });
                continue;
            }
            self.jobs.get_mut(&id).unwrap().failed = true;
            self.jobs.get_mut(&id).unwrap().failed_msg =
                Some(format!("Required unit {r} is not active (requisite)."));
            self.maybe_start_job(id);
            return;
        }

        // Conflicts: stop active/starting conflicting units first.
        for c in D::closure_conflicts(&self.units, &name) {
            if self.unit_operational(&c)
                && self
                    .unit_job
                    .get(&c)
                    .map(|j| self.jobs[j].kind == JobKind::Stop)
                    != Some(true)
            {
                self.enqueue_stop_job(&c);
            }
        }

        // Activation dependencies: Requires= (fatal if missing) and Wants=
        // (silently ignored if missing) pull units into the transaction.
        // `After=` is deliberately *not* here — it only orders, never activates
        // (systemd semantics).
        let needs_set: HashSet<String> = needs.iter().cloned().collect();
        // Keep the dependency closure's order. In particular, a target's
        // `Wants=` list may contain a unit that another wanted unit orders
        // after; putting the list in a HashSet can start the latter before
        // the former has a job in the transaction, making `After=` invisible.
        let mut open = Vec::new();
        for d in needs.iter().chain(&weak) {
            if !open.contains(d) {
                open.push(d.clone());
            }
        }
        for d in open {
            if self.unit_active(&d) {
                continue;
            }
            let required = needs_set.contains(&d);
            if !self.units.contains_key(&d) {
                // The dependency names a unit with no backing file. A missing
                // Requires= fails the transaction; a missing Wants= is silent.
                if required {
                    self.jobs.get_mut(&id).unwrap().failed = true;
                    self.jobs.get_mut(&id).unwrap().failed_msg =
                        Some(format!("Dependency failed: {d} (unit not found)."));
                    self.maybe_start_job(id);
                    return;
                }
                continue;
            }
            if let Some(jid) = self.unit_job.get(&d)
                && self.jobs[jid].kind == JobKind::Start
            {
                waiting.push(WaitEntry { unit: d, required });
                continue;
            }
            self.enqueue_start_job(&d);
            waiting.push(WaitEntry { unit: d, required });
        }

        // Ordering only: `After=` never activates a unit. It merely makes this
        // unit wait for an After= target that is *already* part of the
        // transaction (has a pending start job). A missing or unrelated
        // After= target is silently ignored.
        let after_names: Vec<String> = self.units[&name]
            .file
            .as_ref()
            .map(|f| f.unit.after.clone())
            .unwrap_or_default();
        for a in &after_names {
            if self.unit_active(a) {
                continue;
            }
            if let Some(jid) = self.unit_job.get(a)
                && self.jobs[jid].kind == JobKind::Start
                && !waiting.iter().any(|w| w.unit == *a)
            {
                waiting.push(WaitEntry {
                    unit: a.clone(),
                    required: false,
                });
            }
        }

        // `Before=` is the inverse spelling of `After=`. If a pending unit
        // declares `Before=name`, make this job wait for that pending start
        // job as well. Without the reverse lookup, a target can pull in two
        // units and let the later one run before the target's `After=` work
        // has completed.
        let before_names: Vec<String> = self
            .units
            .iter()
            .filter(|(other, unit)| {
                *other != &name
                    && unit
                        .file
                        .as_ref()
                        .is_some_and(|f| f.unit.before.iter().any(|b| b == &name))
            })
            .map(|(other, _)| other.clone())
            .collect();
        for before in before_names {
            if self.unit_active(&before) || waiting.iter().any(|w| w.unit == before) {
                continue;
            }
            if let Some(jid) = self.unit_job.get(&before)
                && self.jobs[jid].kind == JobKind::Start
            {
                waiting.push(WaitEntry {
                    unit: before,
                    required: false,
                });
            }
        }

        // Some dependencies resolve synchronously during the expansion above:
        // a `.mount` fails on mount(2) EPERM, a `.target` activates the instant
        // it starts, etc. Their job completes (and runs `on_job_completed`)
        // while this job's `waiting` list is still uncommitted, so a wait entry
        // pushed for them would never be removed and would block this job
        // forever. Drop any entry whose job is no longer pending.
        //
        // But first: a *required* dependency that finished synchronously in a
        // non-active state has failed (e.g. a `Requires=` mount that hit EPERM).
        // `on_job_completed` could not propagate that failure — `waiting` was
        // not yet committed when it ran — so fail this job here to mirror its
        // required-dependency handling instead of silently treating the dead
        // dependency as satisfied.
        let sync_failed = waiting
            .iter()
            .find(|w| {
                w.required && !self.unit_job.contains_key(&w.unit) && !self.unit_active(&w.unit)
            })
            .map(|w| w.unit.clone());
        waiting.retain(|w| self.unit_job.contains_key(&w.unit));

        if let Some(j) = self.jobs.get_mut(&id) {
            if let Some(dep) = sync_failed {
                j.failed = true;
                j.failed_msg = Some(format!("Dependency failed: {dep}"));
            }
            j.waiting = waiting;
            j.expanding = false;
        }
        self.maybe_start_job(id);
    }

    fn enqueue_stop_job(&mut self, name: &str) -> u64 {
        self.enqueue_stop_job_after(name, None)
    }

    fn enqueue_stop_job_after(&mut self, name: &str, start_after_stop: Option<Name>) -> u64 {
        self.enqueue_stop_job_after_mode(name, start_after_stop, JobMode::Replace)
    }

    fn enqueue_stop_job_after_mode(
        &mut self,
        name: &str,
        start_after_stop: Option<Name>,
        mode: JobMode,
    ) -> u64 {
        let id = self.new_job(JobKind::Stop, name, vec![], mode);
        if let Some(next) = start_after_stop
            && let Some(job) = self.jobs.get_mut(&id)
        {
            job.start_after_stop = Some(next);
        }
        let dependents = D::stop_propagate(&self.units, name);
        for d in dependents {
            if self.unit_operational(&d) && !self.unit_job.contains_key(&d) {
                self.enqueue_stop_job(&d);
            }
        }
        self.maybe_stop_job(id);
        id
    }

    fn unit_active(&self, name: &str) -> bool {
        self.units
            .get(name)
            .map(|u| u.active == ActiveState::Active)
            .unwrap_or(false)
    }

    fn unit_operational(&self, name: &str) -> bool {
        self.units
            .get(name)
            .map(|u| u.active == ActiveState::Active || u.active == ActiveState::Activating)
            .unwrap_or(false)
    }

    fn maybe_start_job(&mut self, id: u64) {
        let unit = self.jobs[&id].unit.clone();
        if self.jobs[&id].failed {
            self.finish_job_failed(id);
            return;
        }
        if !self.jobs[&id].waiting.is_empty() || self.jobs[&id].started {
            return;
        }
        self.jobs.get_mut(&id).unwrap().started = true;
        if self.check_start_limit(&unit) {
            self.units.get_mut(&unit).unwrap().result = UnitResult::StartLimitHit;
            self.jobs.get_mut(&id).unwrap().failed = true;
            self.jobs.get_mut(&id).unwrap().failed_msg =
                Some("Start request repeated too quickly (start-limit-hit).".into());
            self.finish_job_failed(id);
            return;
        }
        self.do_start(&unit);
    }

    fn maybe_stop_job(&mut self, id: u64) {
        let unit = self.jobs[&id].unit.clone();
        if !self.unit_operational(&unit) {
            self.finish_job(id);
            return;
        }
        if self.jobs[&id].started {
            return;
        }
        self.unit_job.insert(unit.clone(), id);
        self.jobs.get_mut(&id).unwrap().started = true;
        self.do_stop(&unit);
    }

    pub fn tick(&mut self, now: Instant) {
        for entry in self.wheel.pop_due(now) {
            self.fire_timer(&entry.unit, entry.kind, now);
        }
        self.poll_paths();
        self.reap();
        self.process_jobs();
    }

    fn process_jobs(&mut self) {
        loop {
            let ids: Vec<u64> = self.jobs.keys().copied().collect();
            let mut changed = false;
            for id in ids {
                if !self.jobs.contains_key(&id) {
                    continue;
                }
                changed |= self.try_advance_job(id);
            }
            if !changed {
                break;
            }
        }
    }

    fn try_advance_job(&mut self, id: u64) -> bool {
        if !self.jobs.contains_key(&id) {
            return false;
        }
        match self.jobs[&id].kind {
            JobKind::Start => {
                // A job whose dependency list is still being built must not be
                // started by a re-entrant `process_jobs` run.
                if self.jobs[&id].expanding {
                    return false;
                }
                if self.jobs[&id].failed {
                    self.finish_job_failed(id);
                    true
                } else if self.jobs[&id].waiting.is_empty() && !self.jobs[&id].started {
                    self.maybe_start_job(id);
                    true
                } else {
                    false
                }
            }
            JobKind::Stop => {
                if !self.jobs[&id].started {
                    self.maybe_stop_job(id);
                    true
                } else {
                    false
                }
            }
            JobKind::Restart => false,
        }
    }

    fn on_job_completed(&mut self, unit: &str, ok: bool) {
        let ids: Vec<u64> = self.jobs.keys().copied().collect();
        for id in ids {
            if !self.jobs.contains_key(&id) {
                continue;
            }
            let mut remove_required = false;
            if let Some(j) = self.jobs.get(&id)
                && let Some(pos) = j.waiting.iter().position(|w| w.unit == unit)
                && !ok
                && j.waiting[pos].required
            {
                remove_required = true;
            }
            if remove_required {
                self.jobs.get_mut(&id).unwrap().failed = true;
                self.jobs.get_mut(&id).unwrap().failed_msg =
                    Some(format!("Dependency failed: {unit}"));
                continue;
            }
            if let Some(j) = self.jobs.get_mut(&id) {
                let before = j.waiting.len();
                j.waiting.retain(|w| w.unit != unit);
                if j.waiting.len() != before {
                    self.try_advance_job(id);
                }
            }
        }
    }

    fn record_job_result(&mut self, id: u64, ok: bool, error: Option<String>) {
        let Some(job) = self.jobs.get(&id) else {
            return;
        };
        self.completed_jobs.insert(
            id,
            JobStatus {
                id,
                unit: job.unit.clone(),
                state: "done".into(),
                ok: Some(ok),
                error,
            },
        );
    }

    fn cancel_job(&mut self, id: u64, message: &str) {
        let Some(job) = self.jobs.get(&id).cloned() else {
            return;
        };
        self.record_job_result(id, false, Some(message.into()));
        self.jobs.remove(&id);
        if self.unit_job.get(&job.unit) == Some(&id) {
            self.unit_job.remove(&job.unit);
        }
        self.on_job_completed(&job.unit, false);
    }

    fn finish_job(&mut self, id: u64) {
        let Some(job) = self.jobs.get(&id).cloned() else {
            return;
        };
        let unit = job.unit.clone();
        if job.kind == JobKind::Stop
            && let Some(next) = job.start_after_stop.clone()
        {
            self.record_job_result(id, true, None);
            self.jobs.remove(&id);
            if self.unit_job.get(&unit) == Some(&id) {
                self.unit_job.remove(&unit);
            }
            self.on_job_completed(&unit, true);
            self.enqueue_start_job(&next);
            return;
        }
        self.record_job_result(id, true, None);
        self.jobs.remove(&id);
        if self.unit_job.get(&unit) == Some(&id) {
            self.unit_job.remove(&unit);
        }
        self.on_job_completed(&unit, true);
    }

    fn finish_job_failed(&mut self, id: u64) {
        let Some(job) = self.jobs.get(&id).cloned() else {
            return;
        };
        let unit = job.unit.clone();
        if let Some(msg) = &job.failed_msg {
            self.mgr(&unit, msg);
        }
        self.record_job_result(id, false, job.failed_msg.clone());
        self.jobs.remove(&id);
        if self.unit_job.get(&unit) == Some(&id) {
            self.unit_job.remove(&unit);
        }
        self.on_job_completed(&unit, false);
    }

    fn check_start_limit(&mut self, name: &str) -> bool {
        let now = Instant::now();
        let cutoff = now - Duration::from_secs(10);
        let u = self.units.get_mut(name).unwrap();
        u.start_window.retain(|t| *t >= cutoff);
        if u.start_window.len() >= 5 {
            return true;
        }
        u.start_window.push(now);
        false
    }

    // ---- start / stop -------------------------------------------------------

    /// Resolve the per-type behavior for a unit (the internal VTable dispatch).
    fn unit_type(&self, name: &str) -> &'static dyn UnitType {
        match self.units.get(name).map(|u| u.kind) {
            Some(UnitKind::Timer) => &TimerUnit,
            Some(UnitKind::Target) => &TargetUnit,
            Some(UnitKind::Path) => &PathUnit,
            #[cfg(feature = "socket")]
            Some(UnitKind::Socket) => &SocketUnit,
            #[cfg(all(target_os = "linux", feature = "udev"))]
            Some(UnitKind::Device) => &DeviceUnit,
            #[cfg(target_os = "linux")]
            Some(UnitKind::Mount) => &MountUnit,
            _ => &ServiceUnit,
        }
    }

    fn do_start(&mut self, name: &str) {
        let ut = self.unit_type(name);
        ut.start(self, name);
    }

    // ---- udev device tracking (Linux + `udev` feature) ----------------------

    /// Discover kernel devices and start monitoring uevents. Idempotent: runs
    /// once, at startup, before any unit is started so that
    /// `After=sys-…device` / `Requires=sys-…device` ordering resolves against
    /// the freshly enumerated table.
    ///
    /// Ordering matters here: subscribe to the uevent socket **before**
    /// enumerating, then drain any events that raced the walk. That closes the
    /// classic subscribe/enumerate race without missing a hotplug event.
    #[cfg(all(target_os = "linux", feature = "udev"))]
    pub fn udev_init(&mut self) {
        if self.udev.is_some() {
            return;
        }
        self.udev = match crate::platform::udev::UdevMonitor::new() {
            Ok(m) => Some(m),
            Err(e) => {
                mgr_log(&format!(
                    "udev: monitor unavailable ({e}); hotplug disabled"
                ));
                None
            }
        };
        for dev in crate::platform::udev::enumerate_devices() {
            self.udev_register(&dev);
        }
        // Drain any uevents that arrived while we enumerated.
        self.udev_process();
        let device_units = self
            .units
            .values()
            .filter(|u| u.kind == UnitKind::Device)
            .count();
        mgr_log(&format!(
            "udev: {} devices → {device_units} .device units",
            self.udev_devices.len()
        ));
    }

    /// Register (or refresh) the `.device` unit(s) for a discovered device.
    /// A device is active the instant it exists, so the unit is inserted
    /// already `active`; no start job is involved.
    #[cfg(all(target_os = "linux", feature = "udev"))]
    fn udev_register(&mut self, dev: &crate::platform::udev::Device) {
        self.udev_devices.insert(dev.devpath.clone(), dev.clone());
        for name in dev.unit_names() {
            if self.units.contains_key(&name) {
                continue;
            }
            let mut u = Unit::new(&name, UnitKind::Device);
            u.load = LoadState::Loaded;
            u.file = Some(self.udev_unit_file(dev));
            u.set_active(ActiveState::Active, SubState::Dead, UnitResult::Success);
            self.units.insert(name, u);
        }
    }

    /// Remove the `.device` unit(s) for a device that disappeared (hotplug
    /// remove or a synthetic `change` that dropped the node).
    #[cfg(all(target_os = "linux", feature = "udev"))]
    fn udev_remove(&mut self, dev: &crate::platform::udev::Device) {
        self.udev_devices.remove(&dev.devpath);
        for name in dev.unit_names() {
            self.units.remove(&name);
        }
    }

    /// Drain pending uevents and apply them to the unit table.
    #[cfg(all(target_os = "linux", feature = "udev"))]
    fn udev_process(&mut self) {
        let events = match self.udev.as_mut() {
            Some(m) => m.read_events(),
            None => return,
        };
        for (action, dev) in events {
            use crate::platform::udev::UEventAction;
            match action {
                UEventAction::Add | UEventAction::Change | UEventAction::Move => {
                    self.udev_register(&dev);
                }
                UEventAction::Remove => self.udev_remove(&dev),
                UEventAction::Other => {}
            }
        }
    }

    /// The synthesized unit file backing a `.device` unit (description only —
    /// no config is ever parsed from disk).
    #[cfg(all(target_os = "linux", feature = "udev"))]
    fn udev_unit_file(&self, dev: &crate::platform::udev::Device) -> UnitFile {
        let description = if dev.devname.is_empty() {
            format!("{} {}", dev.subsystem, dev.sysname())
        } else {
            format!("{} {}", dev.subsystem, dev.devname)
        };
        UnitFile {
            path: None,
            unit: crate::unit::UnitConfig {
                description,
                ..Default::default()
            },
            service: None,
            timer: None,
            path_unit: None,
            #[cfg(feature = "socket")]
            socket: None,
            #[cfg(target_os = "linux")]
            mount: None,
            install: Default::default(),
        }
    }

    fn run_main_start(&mut self, name: &str, idx: usize) {
        // Oneshot & main process types both go through spawn_control(Start).
        self.spawn_control(name, UnitControlCommand::Start, idx);
    }

    /// Run one Exec command for a unit, updating control/main bookkeeping.
    pub(crate) fn spawn_control(&mut self, name: &str, cmd: UnitControlCommand, idx: usize) {
        let sc = match self.units[name].service_cfg() {
            Some(s) => s.clone(),
            None => {
                self.stage_done(name, cmd);
                return;
            }
        };
        #[cfg(windows)]
        {
            let unsupported = match sc.service_type {
                ServiceType::Forking => Some("Type=forking is not supported on Windows"),
                ServiceType::Notify => Some("Type=notify is not supported on Windows"),
                ServiceType::Dbus => Some("Type=dbus is not supported on Windows"),
                _ if sc.user.is_some() || sc.group.is_some() => {
                    Some("User=/Group= are not supported on Windows")
                }
                _ if sc.kill_mode == KillMode::Process => {
                    Some("KillMode=process is not supported on Windows; use the Job Object default")
                }
                _ if sc.cgroup_limits.memory_high.is_some() => {
                    Some("MemoryHigh= is not supported by Win32 Job Objects")
                }
                _ if sc.cgroup_limits.cpu_weight.is_some() => {
                    Some("CPUWeight= is not supported by the Windows manager MVP")
                }
                _ if sc.cgroup_limits.cpu_quota.is_some() => {
                    Some("CPUQuota= is not supported by the Windows manager MVP")
                }
                _ if sc.cgroup_limits.io_weight.is_some()
                    || !sc.cgroup_limits.io_device_weights.is_empty() =>
                {
                    Some("IOWeight=/IODeviceWeight= are not supported by the Windows manager MVP")
                }
                _ => None,
            };
            if let Some(reason) = unsupported {
                self.units.get_mut(name).unwrap().result = UnitResult::Protocol;
                self.fail_unit(name, reason.to_string());
                return;
            }
        }
        let list_ref: &Vec<crate::unit::ExecCommand> = match cmd {
            UnitControlCommand::StartPre => self.exec_slice(&sc, cmd),
            UnitControlCommand::Start => self.exec_slice(&sc, cmd),
            UnitControlCommand::StartPost => self.exec_slice(&sc, cmd),
            UnitControlCommand::Stop => self.exec_slice(&sc, cmd),
            UnitControlCommand::Reload => self.exec_slice(&sc, cmd),
            UnitControlCommand::Kill => self.exec_slice(&sc, UnitControlCommand::Start),
        };
        if list_ref.is_empty() {
            self.stage_done(name, cmd);
            return;
        }
        let Some(exec) = list_ref.get(idx).cloned() else {
            self.stage_done(name, cmd);
            return;
        };

        // Env expansion at exec time.
        let env = self.build_env(self.units.get(name).unwrap());
        let env_refs: Vec<(String, String)> =
            env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let argv = spawn::expand_env_argv(&exec.argv, &env);

        // Resolve user/group before the (async-signal-safe) pre_exec.
        let resolved_user = match sc.user.as_deref() {
            Some(user) => match spawn::resolve_user(user) {
                Some(identity) => Some(identity),
                None => {
                    self.units.get_mut(name).unwrap().result = UnitResult::User;
                    self.fail_unit(name, format!("User={user} could not be resolved"));
                    return;
                }
            },
            None => None,
        };
        let uid = resolved_user.as_ref().map(|identity| identity.0);
        let gid = match sc.group.as_deref() {
            Some(group) => match spawn::resolve_group(group) {
                Some(gid) => Some(gid),
                None => {
                    self.units.get_mut(name).unwrap().result = UnitResult::Group;
                    self.fail_unit(name, format!("Group={group} could not be resolved"));
                    return;
                }
            },
            None => None,
        };
        let groups = resolved_user.map(|identity| identity.2).unwrap_or_default();

        // Create/own the unit's `*Directory=` directories before the process
        // turns up (on the Start command, which is the process spawn).
        if cmd == UnitControlCommand::Start
            && let Err(e) = self.apply_directories(&sc)
        {
            self.units.get_mut(name).unwrap().result = UnitResult::Resources;
            self.fail_unit(name, e);
            return;
        }

        let cgroup = self.ensure_cgroup(name);

        let opts = spawn::SpawnOptions {
            argv,
            env: env_refs,
            cwd: sc.working_directory.as_ref().map(|(p, _)| PathBuf::from(p)),
            uid,
            gid,
            groups,
            nice: sc.nice,
            umask: sc.umask,
            rlimits: sc.rlimits.clone(),
            stdout_target: sc.std_output,
            stderr_target: sc.std_error,
            stdin_null: !sc.std_input,
            notify_socket: if sc.service_type == ServiceType::Notify {
                Some(self.cfg.paths.notify_socket())
            } else {
                None
            },
            listen_fds: {
                #[cfg(feature = "socket")]
                {
                    self.socket_fds_for(name)
                }
                #[cfg(not(feature = "socket"))]
                {
                    Vec::new()
                }
            },
            cgroup,
            #[cfg(target_os = "linux")]
            sandbox_ops: crate::platform::sandbox::plan(&sc.sandbox),
            #[cfg(windows)]
            limits: sc.cgroup_limits,
            #[cfg(windows)]
            unit_name: name.to_string(),
        };

        match spawn::spawn(&opts) {
            Ok(sp) => {
                #[cfg(unix)]
                {
                    if let Some(fd) = sp.stdout {
                        let raw = fd.as_raw_fd();
                        self.out_fds.insert(raw, name.to_string());
                        self.owned_fds.insert(raw, fd);
                    }
                    if let Some(fd) = sp.stderr {
                        let raw = fd.as_raw_fd();
                        self.out_fds.insert(raw, name.to_string());
                        self.owned_fds.insert(raw, fd);
                    }
                }
                let pid = sp.pid;
                self.pid_unit.insert(pid, name.to_string());
                let u = self.units.get_mut(name).unwrap();
                u.control_pid = Some(pid);
                u.control_command = Some(cmd);
                u.group_pid = Some(pid);
                u.control_start = Some(Instant::now());

                // Long-running / notify / dbus types: the Start command is the
                // main process. Simple/exec/idle are considered active right
                // away; notify waits for READY=1, dbus waits for BusName=.
                if cmd == UnitControlCommand::Start
                    && matches!(
                        sc.service_type,
                        ServiceType::Simple
                            | ServiceType::Exec
                            | ServiceType::Idle
                            | ServiceType::Notify
                            | ServiceType::Dbus
                    )
                {
                    u.control_pid = None;
                    u.control_command = None;
                    u.main_pid = Some(pid);
                    if matches!(
                        sc.service_type,
                        ServiceType::Simple | ServiceType::Exec | ServiceType::Idle
                    ) {
                        u.set_active(ActiveState::Active, SubState::Running, UnitResult::Success);
                        self.complete_start_job(name);
                        return;
                    }
                    // Notify: wait for READY=1; Dbus: wait for BusName=.
                    if sc.service_type == ServiceType::Dbus {
                        self.begin_bus_name_wait(name, sc.bus_name.as_deref());
                    }
                    self.arm_start_timeout(name);
                    return;
                }

                // Oneshot / forking: wait for exec completion or pidfile.
                if cmd == UnitControlCommand::Start {
                    if sc.service_type == ServiceType::Forking {
                        self.arm_start_timeout(name);
                    } else {
                        self.units.get_mut(name).unwrap().sub = SubState::Start;
                        self.arm_start_timeout(name);
                    }
                } else {
                    self.arm_start_timeout(name);
                }
            }
            Err(e) => {
                self.mgr(name, &format!("failed to spawn: {e}"));
                if exec.ignore_failure && cmd != UnitControlCommand::Stop {
                    self.stage_done(name, cmd);
                } else {
                    self.units.get_mut(name).unwrap().result = UnitResult::Exec;
                    self.fail_unit(name, format!("Failed to execute: {e}"));
                }
            }
        }
    }

    fn exec_slice<'a>(
        &self,
        sc: &'a ServiceConfig,
        cmd: UnitControlCommand,
    ) -> &'a Vec<crate::unit::ExecCommand> {
        match cmd {
            UnitControlCommand::StartPre => &sc.exec_start_pre,
            UnitControlCommand::Start => &sc.exec_start,
            UnitControlCommand::StartPost => &sc.exec_start_post,
            UnitControlCommand::Stop => &sc.exec_stop,
            UnitControlCommand::Reload => &sc.exec_reload,
            UnitControlCommand::Kill => &sc.exec_start,
        }
    }

    pub(crate) fn complete_start_job(&mut self, name: &str) {
        if let Some(jid) = self.unit_job.get(name).copied() {
            let kind = self.jobs[&jid].kind;
            if kind == JobKind::Start {
                self.finish_job(jid);
            }
        }
        self.timer_dep_check(name);
    }

    /// Advance after a control-command stage is exhausted.
    fn stage_done(&mut self, name: &str, cmd: UnitControlCommand) {
        match cmd {
            UnitControlCommand::StartPre => self.run_main_start(name, 0),
            UnitControlCommand::Start => self.oneshot_done(name),
            UnitControlCommand::Stop => self.finalize_stop(name),
            UnitControlCommand::Reload => self.mgr(name, "reloaded"),
            UnitControlCommand::StartPost => {}
            UnitControlCommand::Kill => {}
        }
    }

    fn oneshot_done(&mut self, name: &str) {
        let remain = self.units[name]
            .service_cfg()
            .map(|s| s.remain_after_exit)
            .unwrap_or(false);
        let state = if remain {
            ActiveState::Active
        } else {
            ActiveState::Inactive
        };
        let sub = if remain {
            SubState::Exited
        } else {
            SubState::Dead
        };
        self.units
            .get_mut(name)
            .unwrap()
            .set_active(state, sub, UnitResult::Success);
        self.complete_start_job(name);
    }

    fn handle_control_exit(&mut self, name: &str, code: i32, signal: Option<i32>) {
        // Capture state before mutating.
        let ccmd = self.units[name].control_command;
        let cidx = self.units[name].cmd_index;
        let stopping = self.units[name].active == ActiveState::Deactivating;
        let service_type = self.units[name]
            .service_cfg()
            .map(|s| s.service_type)
            .unwrap_or(ServiceType::Simple);
        let sc = self.units[name].service_cfg().cloned();
        self.units.get_mut(name).unwrap().control_pid = None;
        self.units.get_mut(name).unwrap().group_pid = None;
        self.units.get_mut(name).unwrap().control_command = None;

        if ccmd == Some(UnitControlCommand::Stop) {
            let current_is_stop = self
                .unit_job
                .get(name)
                .and_then(|id| self.jobs.get(id))
                .map(|job| job.kind == JobKind::Stop)
                .unwrap_or(false);
            if !current_is_stop {
                return;
            }
        }

        // A stop is in progress: the control command's death (however it was
        // signalled — e.g. shutdown SIGTERMing an in-flight oneshot ExecStart)
        // completes the stop. Route to `finalize_stop` instead of the
        // start-failure path, which would otherwise leave the Stop job pending
        // forever and hang shutdown.
        if stopping {
            self.finalize_stop(name);
            return;
        }

        let exit_ok = sc
            .as_ref()
            .map(|s| match signal {
                Some(sig) => s.effective_exit_success().matches(None, Some(sig)),
                None => s.effective_exit_success().matches(Some(code), None),
            })
            .unwrap_or(code == 0 && signal.is_none());
        let ignore_failure = sc
            .as_ref()
            .map(|s| {
                matches!(ccmd, Some(UnitControlCommand::Start))
                    && s.exec_start
                        .get(cidx)
                        .map(|c| c.ignore_failure)
                        .unwrap_or(false)
            })
            .unwrap_or(false);

        match ccmd {
            Some(UnitControlCommand::StartPre) => {
                if exit_ok || ignore_failure {
                    self.run_main_start(name, 0);
                } else {
                    self.units.get_mut(name).unwrap().result = UnitResult::ExitCode;
                    self.fail_unit(
                        name,
                        format!("ExecStartPre failed: {}", self.describe_exit(code, signal)),
                    );
                }
            }
            Some(UnitControlCommand::Start) => match service_type {
                ServiceType::Oneshot => {
                    if exit_ok || ignore_failure {
                        let next = cidx + 1;
                        let has_next = sc
                            .as_ref()
                            .map(|s| s.exec_start.len() > next)
                            .unwrap_or(false);
                        if has_next {
                            self.units.get_mut(name).unwrap().cmd_index = next;
                            self.spawn_control(name, UnitControlCommand::Start, next);
                        } else {
                            self.units.get_mut(name).unwrap().cmd_index = 0;
                            self.run_start_post_or_finish(name);
                        }
                    } else {
                        self.units.get_mut(name).unwrap().result = UnitResult::ExitCode;
                        self.fail_unit(
                            name,
                            format!("ExecStart failed: {}", self.describe_exit(code, signal)),
                        );
                    }
                }
                ServiceType::Forking => {
                    self.handle_forking_start_done(name);
                }
                _ => {
                    // Simple/exec/notify main process is not a control command.
                    self.main_exit(name, code, signal);
                }
            },
            Some(UnitControlCommand::StartPost) => {
                self.units.get_mut(name).unwrap().set_active(
                    if sc.as_ref().map(|s| s.remain_after_exit).unwrap_or(false) {
                        ActiveState::Active
                    } else {
                        ActiveState::Inactive
                    },
                    if sc.as_ref().map(|s| s.remain_after_exit).unwrap_or(false) {
                        SubState::Exited
                    } else {
                        SubState::Dead
                    },
                    UnitResult::Success,
                );
                self.complete_start_job(name);
            }
            Some(UnitControlCommand::Stop) => {
                self.finalize_stop(name);
            }
            Some(UnitControlCommand::Reload) => {
                self.mgr(name, "reloaded");
            }
            Some(UnitControlCommand::Kill) => {}
            None => {
                self.main_exit(name, code, signal);
            }
        }
    }

    fn run_start_post_or_finish(&mut self, name: &str) {
        let has_post = self.units[name]
            .service_cfg()
            .map(|s| !s.exec_start_post.is_empty())
            .unwrap_or(false);
        if has_post {
            self.spawn_control(name, UnitControlCommand::StartPost, 0);
        } else {
            self.oneshot_done(name);
        }
    }

    fn handle_forking_start_done(&mut self, name: &str) {
        // Try to read PIDFile; if it appears, the daemon is up.
        let pid = self.read_forked_pidfile(name);
        if let Some(pid) = pid {
            self.units.get_mut(name).unwrap().forked_main_pid = Some(pid);
            self.units.get_mut(name).unwrap().main_pid = Some(pid);
            self.pid_unit.insert(pid, name.to_string());
            self.units.get_mut(name).unwrap().set_active(
                ActiveState::Active,
                SubState::Running,
                UnitResult::Success,
            );
            self.complete_start_job(name);
        } else {
            // No pidfile; treat as started (best-effort for cgroup-less mode).
            self.units.get_mut(name).unwrap().set_active(
                ActiveState::Active,
                SubState::Running,
                UnitResult::Success,
            );
            self.complete_start_job(name);
        }
    }

    fn read_forked_pidfile(&self, name: &str) -> Option<i32> {
        let sc = self.units[name].service_cfg().cloned()?;
        let pf = sc.pid_file?;
        let text = std::fs::read_to_string(pf).ok()?;
        text.trim().parse::<i32>().ok().filter(|p| *p > 0)
    }

    fn main_exit(&mut self, name: &str, code: i32, signal: Option<i32>) {
        let group_pid = self.units[name].group_pid;
        let u = self.units.get_mut(name).unwrap();
        u.main_pid = None;
        u.group_pid = None;
        u.last_exit_code = if signal.is_none() { Some(code) } else { None };
        u.last_exit_signal = signal;
        let state = u.active;
        let u = self.units.get_mut(name).unwrap();
        let exit_ok = u
            .service_cfg()
            .map(|s| match signal {
                Some(sig) => s.effective_exit_success().matches(None, Some(sig)),
                None => s.effective_exit_success().matches(Some(code), None),
            })
            .unwrap_or(code == 0 && signal.is_none());

        // "Don't self-daemonize" enforcement: if the main process of a still-
        // running foreground service exits but its process group still has live
        // members, the service double-forked (or forked workers and died).
        // With KillMode=control-group — the cgroups stand-in — SIGKILL the
        // survivors so nothing escapes tracking; always log loudly.
        if state == ActiveState::Active
            && let Some(pgid) = group_pid
        {
            self.sweep_orphaned_group(name, pgid);
        }

        match state {
            ActiveState::Deactivating => self.on_stop_main_exit(name),
            ActiveState::Activating => {
                // A Type=dbus main process died before acquiring its BusName=;
                // drop the pending watch so the name can't revive the unit.
                #[cfg(all(target_os = "linux", feature = "dbus"))]
                self.release_bus_name_watch(name);
                if exit_ok {
                    self.units.get_mut(name).unwrap().set_active(
                        ActiveState::Inactive,
                        SubState::Dead,
                        UnitResult::Success,
                    );
                } else {
                    self.units.get_mut(name).unwrap().result = if signal.is_some() {
                        UnitResult::Signal
                    } else {
                        UnitResult::ExitCode
                    };
                    self.fail_unit(name, self.describe_exit(code, signal));
                }
            }
            _ => self.handle_active_exit(name, code, signal),
        }
    }

    fn describe_exit(&self, code: i32, signal: Option<i32>) -> String {
        match signal {
            Some(sig) => format!("killed by signal {}", signal_name(sig)),
            None => format!("exited with status {code}"),
        }
    }

    /// Probe whether a process group still has live members (null signal).
    fn group_alive(group: i32) -> bool {
        spawn::group_alive(group)
    }

    /// Detect and clean up a self-daemonizing service: after a foreground main
    /// process exits while its unit is still running, any members still in its
    /// process group are orphans (a double-forked daemon, or forked workers
    /// left behind). Log a loud warning and, under the default
    /// `KillMode=control-group`, SIGKILL them so they can't escape tracking.
    /// `Type=forking` is exempt — its pidfile daemon legitimately detaches.
    fn sweep_orphaned_group(&mut self, name: &str, pgid: i32) {
        let (kill_mode, is_forking) = match self.units[name].service_cfg() {
            Some(s) => (s.kill_mode, s.service_type == ServiceType::Forking),
            None => return,
        };
        if is_forking || !Self::group_alive(pgid) {
            return;
        }
        self.mgr(
            name,
            "WARNING: main process exited but its process group still has live members — service appears to have self-daemonized",
        );
        if kill_mode == KillMode::ControlGroup && spawn::kill_group(pgid, Signal::SIGKILL).is_ok() {
            self.mgr(
                name,
                "killed orphaned process group (KillMode=control-group)",
            );
        }
    }

    fn handle_active_exit(&mut self, name: &str, code: i32, signal: Option<i32>) {
        let u = self.units.get_mut(name).unwrap();
        let sc = u.service_cfg().cloned();
        let exit_ok = sc
            .as_ref()
            .map(|s| match signal {
                Some(sig) => s.effective_exit_success().matches(None, Some(sig)),
                None => s.effective_exit_success().matches(Some(code), None),
            })
            .unwrap_or(code == 0 && signal.is_none());
        let policy = sc.as_ref().map(|s| s.restart).unwrap_or(RestartPolicy::No);
        let restart_sec = sc
            .as_ref()
            .map(|s| s.restart_sec.as_duration().unwrap_or(Duration::ZERO))
            .unwrap_or(Duration::ZERO);
        let remain_after = sc.as_ref().map(|s| s.remain_after_exit).unwrap_or(false);

        let should_restart = match policy {
            RestartPolicy::No => false,
            RestartPolicy::Always => true,
            RestartPolicy::OnSuccess => exit_ok,
            RestartPolicy::OnFailure => !exit_ok,
            RestartPolicy::OnAbnormal => signal.is_some(),
            RestartPolicy::OnAbort => signal.is_some(),
            RestartPolicy::OnWatchdog => false,
        };

        if should_restart {
            if self.check_start_limit(name) {
                self.units.get_mut(name).unwrap().result = UnitResult::StartLimitHit;
                self.units.get_mut(name).unwrap().set_active(
                    ActiveState::Failed,
                    SubState::Failed,
                    UnitResult::StartLimitHit,
                );
                self.poke_failed(name);
                return;
            }
            self.units.get_mut(name).unwrap().set_active(
                ActiveState::Activating,
                SubState::AutoRestart,
                UnitResult::Success,
            );
            self.units.get_mut(name).unwrap().n_restarts += 1;
            self.wheel
                .schedule(Instant::now() + restart_sec, TimerKind::RestartDelay, name);
        } else if exit_ok {
            let state = if remain_after {
                ActiveState::Active
            } else {
                ActiveState::Inactive
            };
            let sub = if remain_after {
                SubState::Exited
            } else {
                SubState::Dead
            };
            self.units
                .get_mut(name)
                .unwrap()
                .set_active(state, sub, UnitResult::Success);
            self.complete_start_job(name);
            self.timer_dep_check(name);
        } else {
            self.units.get_mut(name).unwrap().result = if signal.is_some() {
                UnitResult::Signal
            } else {
                UnitResult::ExitCode
            };
            let res = self.units[name].result;
            self.units.get_mut(name).unwrap().set_active(
                ActiveState::Failed,
                SubState::Failed,
                res,
            );
            self.poke_failed(name);
            self.fire_on_failure(name);
        }
    }

    fn poke_failed(&mut self, name: &str) {
        let ids: Vec<u64> = self.jobs.keys().copied().collect();
        for id in ids {
            if !self.jobs.contains_key(&id) {
                continue;
            }
            let mut required = false;
            if let Some(j) = self.jobs.get(&id)
                && let Some(w) = j.waiting.iter().find(|w| w.unit == name)
                && w.required
            {
                required = true;
            }
            if required {
                self.jobs.get_mut(&id).unwrap().failed = true;
                self.jobs.get_mut(&id).unwrap().failed_msg =
                    Some(format!("Dependency failed: {name}"));
                self.finish_job_failed(id);
            } else if let Some(j) = self.jobs.get_mut(&id) {
                let before = j.waiting.len();
                j.waiting.retain(|w| w.unit != name);
                if j.waiting.len() != before {
                    self.try_advance_job(id);
                }
            }
        }
    }

    fn fire_on_failure(&mut self, name: &str) {
        let onfail: Vec<String> = self.units[name]
            .file
            .as_ref()
            .map(|f| f.unit.on_failure.clone())
            .unwrap_or_default();
        for t in onfail {
            self.start(&t).ok();
        }
        self.process_jobs();
    }

    fn fail_unit(&mut self, name: &str, msg: String) {
        self.mgr(name, &format!("failed: {msg}"));
        if let Some(unit) = self.units.get_mut(name) {
            unit.log.push_chunk(&format!("rystemd: failed: {msg}\n"));
        }
        let res = self.units[name].result;
        self.units
            .get_mut(name)
            .unwrap()
            .set_active(ActiveState::Failed, SubState::Failed, res);
        if let Some(jid) = self.unit_job.get(name).copied() {
            let job = self.jobs[&jid].clone();
            if job.kind == JobKind::Start {
                self.jobs.get_mut(&jid).unwrap().failed_msg = Some(msg);
                self.finish_job_failed(jid);
            } else {
                self.poke_failed(name);
            }
        } else {
            self.poke_failed(name);
        }
        self.fire_on_failure(name);
    }

    fn do_stop(&mut self, name: &str) {
        let u = self.units.get_mut(name).unwrap();
        u.set_active(
            ActiveState::Deactivating,
            SubState::Stop,
            UnitResult::Success,
        );
        u.stop_started = Some(Instant::now());
        let ut = self.unit_type(name);
        ut.stop(self, name);
    }

    /// `.socket` start: bind each `ListenStream=` and register the fds with the
    /// event loop so a connection activates the matching service.
    #[cfg(feature = "socket")]
    pub(crate) fn start_socket(&mut self, name: &str) {
        let scfg = match self.units[name].socket_cfg().cloned() {
            Some(c) => c,
            None => {
                self.units.get_mut(name).unwrap().set_active(
                    ActiveState::Active,
                    SubState::Dead,
                    UnitResult::Success,
                );
                self.complete_start_job(name);
                return;
            }
        };
        if !self.cfg.socket_activation {
            self.mgr(
                name,
                "socket activation disabled at runtime; binding nothing",
            );
            self.units.get_mut(name).unwrap().set_active(
                ActiveState::Active,
                SubState::Dead,
                UnitResult::Success,
            );
            self.complete_start_job(name);
            return;
        }
        let service = self.units[name].activated_service();
        for spec in &scfg.listen_stream {
            match bind_listen_stream(spec) {
                Ok(listener) => {
                    let fd = listener.id();
                    self.socket_listeners.insert(fd, listener);
                    self.socket_triggers
                        .insert(fd, (name.to_string(), service.clone()));
                }
                Err(e) => {
                    self.units.get_mut(name).unwrap().result = UnitResult::Resources;
                    self.fail_unit(name, format!("Failed to bind socket {spec}: {e}"));
                    return;
                }
            }
        }
        for spec in &scfg.listen_datagram {
            match bind_listen_datagram(spec) {
                Ok(listener) => {
                    let fd = listener.id();
                    self.socket_listeners.insert(fd, listener);
                    self.socket_triggers
                        .insert(fd, (name.to_string(), service.clone()));
                }
                Err(e) => {
                    self.units.get_mut(name).unwrap().result = UnitResult::Resources;
                    self.fail_unit(name, format!("Failed to bind socket {spec}: {e}"));
                    return;
                }
            }
        }
        for spec in &scfg.listen_netlink {
            match bind_listen_netlink(spec) {
                Ok(listener) => {
                    let fd = listener.id();
                    self.socket_listeners.insert(fd, listener);
                    self.socket_triggers
                        .insert(fd, (name.to_string(), service.clone()));
                }
                Err(e) => {
                    self.units.get_mut(name).unwrap().result = UnitResult::Resources;
                    self.fail_unit(name, format!("Failed to bind socket {spec}: {e}"));
                    return;
                }
            }
        }
        for spec in &scfg.listen_sequential_packet {
            match bind_listen_sequential_packet(spec) {
                Ok(listener) => {
                    let fd = listener.id();
                    self.socket_listeners.insert(fd, listener);
                    self.socket_triggers
                        .insert(fd, (name.to_string(), service.clone()));
                }
                Err(e) => {
                    self.units.get_mut(name).unwrap().result = UnitResult::Resources;
                    self.fail_unit(name, format!("Failed to bind socket {spec}: {e}"));
                    return;
                }
            }
        }
        if scfg.accept {
            self.mgr(name, "Accept=yes not yet supported; treating as Accept=no");
        }
        self.units.get_mut(name).unwrap().set_active(
            ActiveState::Active,
            SubState::Running,
            UnitResult::Success,
        );
        self.complete_start_job(name);
    }

    /// `.socket` stop: close the bound listeners and drop their triggers.
    #[cfg(feature = "socket")]
    pub(crate) fn stop_socket(&mut self, name: &str) {
        let fds: Vec<SocketId> = self
            .socket_triggers
            .iter()
            .filter(|(_, (unit, _))| unit == name)
            .map(|(fd, _)| *fd)
            .collect();
        for fd in fds {
            self.socket_listeners.remove(&fd);
            self.socket_triggers.remove(&fd);
        }
        self.finalize_stop(name);
    }

    /// `.mount` start: perform `mount(2)` and go `active(mounted)` on success,
    /// or `failed` on error. Mounting is synchronous, so there is no
    /// intermediate `activating` phase to supervise.
    #[cfg(target_os = "linux")]
    pub(crate) fn start_mount(&mut self, name: &str) {
        let cfg = match self.units[name].mount_cfg().cloned() {
            Some(c) => c,
            None => {
                self.fail_unit(name, "missing [Mount] section".into());
                return;
            }
        };
        let target = match cfg.where_.as_deref() {
            Some(w) if !w.is_empty() => w.to_string(),
            _ => {
                self.fail_unit(name, "no Where= mount point".into());
                return;
            }
        };
        let fstype = match cfg.fs_type.as_deref() {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                self.fail_unit(name, "no Type= filesystem type".into());
                return;
            }
        };
        let (flags, data) = crate::platform::mount::split_options(cfg.options.as_deref());
        match crate::platform::mount::mount(
            cfg.what.as_deref(),
            std::path::Path::new(&target),
            &fstype,
            flags,
            data.as_deref(),
        ) {
            Ok(()) => {
                self.units.get_mut(name).unwrap().set_active(
                    ActiveState::Active,
                    SubState::Mounted,
                    UnitResult::Success,
                );
                self.complete_start_job(name);
            }
            Err(e) => {
                self.units.get_mut(name).unwrap().result = UnitResult::Resources;
                self.fail_unit(name, format!("mount {target} failed: {e}"));
            }
        }
    }

    /// `.mount` stop: perform `umount2(2)` and finalize, or `failed` on error.
    #[cfg(target_os = "linux")]
    pub(crate) fn stop_mount(&mut self, name: &str) {
        let cfg = self.units[name].mount_cfg().cloned();
        let target = match cfg.and_then(|c| c.where_) {
            Some(w) if !w.is_empty() => w,
            _ => {
                // Nothing to unmount: finalize immediately.
                self.finalize_stop(name);
                return;
            }
        };
        match crate::platform::mount::unmount(std::path::Path::new(&target), false) {
            Ok(()) => self.finalize_stop(name),
            Err(e) => {
                self.units.get_mut(name).unwrap().result = UnitResult::Resources;
                self.fail_unit(name, format!("unmount {target} failed: {e}"));
            }
        }
    }

    /// Listening fds to pass to a service being socket-activated.
    #[cfg(feature = "socket")]
    fn socket_fds_for(&self, service: &str) -> Vec<spawn::ListenHandle> {
        let mut fds: Vec<spawn::ListenHandle> = self
            .socket_triggers
            .iter()
            .filter(|(_, (_, svc))| svc == service)
            .map(|(fd, _)| *fd)
            .collect();
        fds.sort_unstable();
        fds
    }

    fn arm_start_timeout(&mut self, name: &str) {
        let lim = self.units[name].service_cfg().map(|s| s.timeout_start_sec);
        if let Some(ts) = lim
            && let Some(d) = ts.as_duration()
        {
            // Drop any prior StartTimeout for this unit. Without this, a stop →
            // re-start cycle leaks the previous start's deadline into the wheel;
            // `fire_service_timer` would no-op it (the state guard catches the
            // wrong unit state), but the heap grows without bound and the
            // manager pays an O(log n) cost per fire for nothing.
            self.wheel.cancel_by_kind(name, TimerKind::StartTimeout);
            self.wheel
                .schedule(Instant::now() + d, TimerKind::StartTimeout, name);
        }
    }

    pub(crate) fn arm_stop_timeout(&mut self, name: &str) {
        let lim = self.units[name].service_cfg().map(|s| s.timeout_stop_sec);
        if let Some(ts) = lim
            && let Some(d) = ts.as_duration()
        {
            // Same idempotence as `arm_start_timeout` — keep one live deadline
            // per (unit, kind) so a re-arming stop does not pile entries up.
            self.wheel.cancel_by_kind(name, TimerKind::StopTimeout);
            self.wheel
                .schedule(Instant::now() + d, TimerKind::StopTimeout, name);
        }
    }

    fn on_stop_main_exit(&mut self, name: &str) {
        self.kill_tree_kill(name);
        self.finalize_stop(name);
    }

    pub(crate) fn finalize_stop(&mut self, name: &str) {
        // Snap the *Directory= directives before the mutable unit borrow, so
        // the cleanup can remove runtime/empty-state dirs on stop.
        let dirs = self.units[name]
            .service_cfg()
            .map(|s| s.directories.clone())
            .unwrap_or_default();
        // Drain stdout fds for this unit on Unix. Windows reader threads
        // terminate when the Job Object closes.
        #[cfg(unix)]
        {
            let fds: Vec<RawFd> = self
                .out_fds
                .iter()
                .filter(|(_, unit)| *unit == name)
                .map(|(fd, _)| *fd)
                .collect();
            for fd in fds {
                self.out_fds.remove(&fd);
                self.owned_fds.remove(&fd);
            }
        }
        // A Type=dbus unit that is stopped before acquiring its BusName= must
        // drop its pending name watch.
        #[cfg(all(target_os = "linux", feature = "dbus"))]
        self.release_bus_name_watch(name);
        let u = self.units.get_mut(name).unwrap();
        u.main_pid = None;
        u.group_pid = None;
        #[cfg(unix)]
        if let Some(dir) = u.cgroup.take() {
            cgroup::release(&dir);
        }
        #[cfg(windows)]
        {
            u.cgroup = None;
        }
        u.control_pid = None;
        u.control_command = None;
        u.cmd_index = 0;
        u.forked_main_pid = None;
        u.stop_started = None;
        u.set_active(ActiveState::Inactive, SubState::Dead, UnitResult::Success);

        if let Some(jid) = self.unit_job.get(name).copied() {
            let kind = self.jobs[&jid].kind;
            if kind == JobKind::Stop || kind == JobKind::Restart {
                self.finish_job(jid);
            }
        }
        // Remove the runtime (and empty state) directories; cache/log/config
        // directories persist by design.
        self.cleanup_directories(&dirs);
        self.timer_dep_check(name);
    }

    fn mgr(&self, unit: &str, msg: &str) {
        mgr_log(&format!("[{unit}] {msg}"));
    }

    // ---- reaping ------------------------------------------------------------

    fn reap(&mut self) {
        for (pid, exit) in spawn::reap_children() {
            let name = self.pid_unit.remove(&pid);
            let (code, signal) = match exit {
                crate::platform::process::ChildExit::Exited(c) => (c, None),
                crate::platform::process::ChildExit::Signaled(s) => (0, Some(s)),
            };
            if let Some(n) = name {
                self.handle_process_exit(&n, pid, code, signal);
            }
        }
    }

    fn handle_process_exit(&mut self, name: &str, pid: i32, code: i32, signal: Option<i32>) {
        if self
            .units
            .get(name)
            .map(|u| u.kind == UnitKind::Target)
            .unwrap_or(false)
        {
            return;
        }
        let is_control = self
            .units
            .get(name)
            .map(|u| u.control_pid == Some(pid))
            .unwrap_or(false);
        if is_control {
            self.handle_control_exit(name, code, signal);
        } else {
            self.main_exit(name, code, signal);
        }
    }

    // ---- timers -------------------------------------------------------------

    fn rearm_all_timers(&mut self) {
        self.wheel = TimerWheel::default();
        let timers: Vec<String> = self
            .units
            .iter()
            .filter(|(_, u)| u.kind == UnitKind::Timer)
            .map(|(n, _)| n.clone())
            .collect();
        for t in timers {
            self.rearm_timer(&t);
        }
    }

    fn rearm_timer(&mut self, name: &str) {
        // Idempotent: drop any prior deadlines for this timer so repeated
        // re-arms (load_all + timer_dep_check on every unit state change)
        // don't accumulate duplicate entries that all fire at once.
        self.wheel.cancel_by_unit(name);
        // Once we're shutting down, stop arming timers: a re-arming schedule
        // would keep `has_service_timers()` true and block the manager from
        // ever reaching `idle()` and exiting (see `shutdown`).
        if self.shutting_down {
            return;
        }
        let tc = match self.units[name].timer_cfg() {
            Some(t) => t.clone(),
            None => return,
        };
        let mut st = self
            .units
            .get_mut(name)
            .unwrap()
            .timer
            .take()
            .unwrap_or_else(|| {
                TimerState::new(tc.on_calendar.iter().map(|c| c.to_string()).collect())
            });

        let now_civil = chrono::Local::now().naive_local();
        let mut next_calendar: Option<(u64, usize)> = None;
        for (i, spec) in tc.on_calendar.iter().enumerate() {
            if let Some(dt) = spec.next_elapse(now_civil) {
                let epoch = dt.and_utc().timestamp().max(0) as u64;
                if next_calendar.map(|(e, _)| epoch < e).unwrap_or(true) {
                    next_calendar = Some((epoch, i));
                }
            }
        }

        let mut next_mono: Option<Instant> = None;
        for ts in tc.on_boot_sec.iter().chain(tc.on_startup_sec.iter()) {
            if let Some(d) = ts.as_duration() {
                next_mono = min_of(next_mono, self.boot_instant + d);
            }
        }
        let target = self.units[name].activated_unit();
        if let Some(tu) = self.units.get(&target) {
            if tu.active == ActiveState::Active {
                for ts in tc.on_active_sec.iter() {
                    if let Some(d) = ts.as_duration() {
                        next_mono = min_of(next_mono, Instant::now() + d);
                    }
                }
            } else if tu.active_enter.is_some() {
                // `OnUnitInactiveSec=` only arms once the target has actually
                // been activated and then deactivated — never for a unit that
                // has not been started this boot (spurious fire at load).
                for ts in tc.on_inactive_sec.iter() {
                    if let Some(d) = ts.as_duration() {
                        next_mono = min_of(next_mono, Instant::now() + d);
                    }
                }
            }
        }

        // Record the next fire time for `list-timers`' NEXT column. Without
        // this, `next_display` stays None and list-timers always shows "-".
        let mut next_display: Option<SystemTime> = None;
        if let Some((epoch, _)) = next_calendar {
            next_display = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(epoch));
        }

        if let Some((epoch, idx)) = next_calendar {
            let now_epoch = chrono::Local::now().timestamp().max(0) as u64;
            let delta = epoch.saturating_sub(now_epoch).max(1);
            self.wheel.schedule(
                Instant::now() + Duration::from_secs(delta),
                TimerKind::CalendarElapse(idx),
                name,
            );
        }
        if let Some(when) = next_mono
            && when >= Instant::now()
        {
            let sys_when = SystemTime::now() + when.duration_since(Instant::now());
            next_display = Some(match next_display {
                Some(cur) if cur <= sys_when => cur,
                _ => sys_when,
            });
            self.wheel.schedule(when, TimerKind::MonotonicElapse, name);
        }

        st.next_display = next_display;
        self.units.get_mut(name).unwrap().timer = Some(st);
    }

    fn timer_dep_check(&mut self, _name: &str) {
        let timers: Vec<String> = self
            .units
            .iter()
            .filter(|(_, u)| u.kind == UnitKind::Timer)
            .map(|(n, _)| n.clone())
            .collect();
        for t in timers {
            self.rearm_timer(&t);
        }
    }

    fn fire_timer(&mut self, unit: &str, kind: TimerKind, _now: Instant) {
        if self.units.get(unit).map(|u| u.kind) != Some(UnitKind::Timer) {
            self.fire_service_timer(unit, kind);
            return;
        }
        let target = self.units.get(unit).unwrap().activated_unit();
        let now_epoch = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(t) = self.units.get_mut(unit)
            && let Some(ts) = t.timer.as_mut()
        {
            ts.last_trigger = Some(SystemTime::now());
            match kind {
                TimerKind::CalendarElapse(idx) => {
                    ts.last_trigger_calendar = Some((now_epoch, idx));
                }
                TimerKind::MonotonicElapse => {
                    ts.last_trigger_monotonic = Some(Instant::now());
                }
                _ => {}
            }
        }
        self.mgr(unit, &format!("triggered {target}"));
        self.start(&target).ok();
        self.rearm_timer(unit);
        self.process_jobs();
    }

    fn fire_service_timer(&mut self, unit: &str, kind: TimerKind) {
        match kind {
            TimerKind::RestartDelay => {
                let restart_pending = self
                    .units
                    .get(unit)
                    .map(|u| u.sub == SubState::AutoRestart)
                    .unwrap_or(false);
                if restart_pending {
                    self.do_start(unit);
                    self.process_jobs();
                }
            }
            TimerKind::StartTimeout => {
                let activating = self
                    .units
                    .get(unit)
                    .map(|u| u.active == ActiveState::Activating)
                    .unwrap_or(false);
                if activating {
                    self.units.get_mut(unit).unwrap().result = UnitResult::Timeout;
                    self.kill_tree_kill(unit);
                    self.fail_unit(unit, "start operation timed out".to_string());
                }
            }
            TimerKind::StopTimeout => {
                let deactivating = self
                    .units
                    .get(unit)
                    .map(|u| u.active == ActiveState::Deactivating)
                    .unwrap_or(false);
                if deactivating {
                    self.kill_tree_kill(unit);
                    self.finalize_stop(unit);
                }
            }
            _ => {}
        }
    }

    // ---- paths (`.path` activation) ----------------------------------------

    /// Poll every armed `.path` unit's watch conditions and start its `Unit=`
    /// target on a fresh trigger. A unit only fires while its target is not
    /// running/starting (mirroring how socket activation keeps a listener out
    /// of the poll set while its service runs), so a satisfied path restarts
    /// the target each time the target drops back to inactive.
    fn poll_paths(&mut self) {
        let candidates: Vec<(String, String, PathConfig)> = self
            .units
            .iter()
            .filter(|(_, u)| u.kind == UnitKind::Path && u.active == ActiveState::Active)
            .filter_map(|(name, u)| {
                let pc = u.path_cfg()?.clone();
                Some((name.clone(), u.activated_path_target(), pc))
            })
            .collect();

        // Resolve which targets are quiescent (not active/activating) so re-arm
        // and re-fire decisions don't fight the mutable borrow of `units`.
        let target_quiescent: HashSet<String> = candidates
            .iter()
            .filter(|(_, t, _)| {
                self.units
                    .get(t)
                    .map(|tu| !matches!(tu.active, ActiveState::Active | ActiveState::Activating))
                    .unwrap_or(true)
            })
            .map(|(_, t, _)| t.clone())
            .collect();

        let mut fired: Vec<(String, String)> = Vec::new();
        for (name, target, pc) in candidates {
            let mut st = self
                .units
                .get_mut(&name)
                .unwrap()
                .path_state
                .take()
                .unwrap_or_default();
            if target_quiescent.contains(&target) {
                if st.triggered {
                    // Target dropped out of `active` → re-arm, may fire again.
                    st.triggered = false;
                }
                if !st.triggered && eval_path_triggers(&pc, &mut st) {
                    st.triggered = true;
                    fired.push((name.clone(), target));
                }
            }
            self.units.get_mut(&name).unwrap().path_state = Some(st);
        }

        for (path_unit, target) in fired {
            self.mgr(&path_unit, &format!("triggered {target}"));
            let _ = self.start(&target);
            self.process_jobs();
        }
    }

    // ---- signal handling ----------------------------

    /// Block the manager's signals and install the signalfd used by the event
    /// loop to read them. Idempotent; called once at daemon startup.
    pub fn setup_signals(&mut self) {
        self.signalfd = SignalSource::new();
    }

    fn handle_signals(&mut self, sig: Signal) {
        match sig {
            Signal::SIGCHLD => self.reap(),
            Signal::SIGTERM | Signal::SIGINT | Signal::SIGQUIT => {
                mgr_log("received shutdown signal");
                self.shutdown();
            }
            Signal::SIGHUP => {
                let errs = self.load_all();
                for e in errs {
                    mgr_log(&e);
                }
            }
            _ => {}
        }
    }

    // ---- event loop -----------------------------------------------------------

    #[cfg(unix)]
    pub fn run(&mut self) {
        self.process_jobs();
        loop {
            self.tick(Instant::now());
            // Drain D-Bus commands/events queued by the dedicated D-Bus
            // thread(s). The poll timeout below is capped at ~1s, so this
            // runs at least that often even when no fd is ready.
            #[cfg(all(target_os = "linux", feature = "dbus"))]
            self.drain_dbus();
            if self.shutting_down && self.idle() {
                break;
            }

            // Cap the poll wait so the loop re-runs `tick()` (reaping, timers)
            // within a bounded latency even when no fd is ready. This matters
            // when SIGCHLD is not delivered to the signalfd (e.g. a manager
            // embedded in a multi-threaded process): `reap()` is `waitpid`
            // based, so a periodic wake guarantees zombies are collected.
            const MAX_POLL_MS: u16 = 1_000;
            let timeout = {
                let deadline = self.wheel.next_deadline();
                match deadline {
                    Some(d) => {
                        let ms = d
                            .saturating_duration_since(Instant::now())
                            .as_millis()
                            .min(u16::MAX as u128) as u16;
                        nix::poll::PollTimeout::from(ms.min(MAX_POLL_MS))
                    }
                    None => nix::poll::PollTimeout::from(MAX_POLL_MS),
                }
            };
            let has_sig = self.signalfd.is_some();
            let has_listener = self.listener.is_some();
            let has_notify = self.notify.is_some();
            #[cfg(all(target_os = "linux", feature = "udev"))]
            let has_udev = self.udev.is_some();
            let out_ids: Vec<RawFd> = self.out_fds.keys().copied().collect();
            // Pair each control fd with whether it has a pending response.
            // POLLOUT is only registered for those — UNIX stream sockets are
            // always POLLOUT-ready while the send buffer has room, so
            // registering it for read-mode clients causes `poll()` to return
            // immediately and burns a full core as long as a peer sits
            // connected-but-quiet (busy-spin defeating the 1s MAX_POLL_MS).
            let control_ids: Vec<(RawFd, bool)> = self
                .control_clients
                .iter()
                .map(|(fd, c)| (*fd, c.out.is_some()))
                .collect();
            // Socket activation: poll a listener only while its target service
            // is Inactive, so a connection triggers the service once (and a
            // running/failed service keeps the fd out of the poll set).
            #[cfg(feature = "socket")]
            let socket_ids: Vec<RawFd> = self
                .socket_triggers
                .iter()
                .filter(|(_, (_, service))| {
                    self.units
                        .get(service)
                        .map(|u| u.active == ActiveState::Inactive)
                        .unwrap_or(true)
                })
                .map(|(fd, _)| *fd)
                .collect();

            let mut pfds: Vec<nix::poll::PollFd> = Vec::new();
            if let Some(sfd) = &self.signalfd {
                pfds.push(nix::poll::PollFd::new(
                    sfd.as_fd(),
                    nix::poll::PollFlags::POLLIN,
                ));
            }
            if let Some(l) = &self.listener {
                pfds.push(nix::poll::PollFd::new(
                    l.as_fd(),
                    nix::poll::PollFlags::POLLIN,
                ));
            }
            if let Some(n) = &self.notify {
                pfds.push(nix::poll::PollFd::new(
                    n.as_fd(),
                    nix::poll::PollFlags::POLLIN,
                ));
            }
            #[cfg(feature = "socket")]
            for &fd in &socket_ids {
                pfds.push(nix::poll::PollFd::new(
                    borrowed_fd(fd),
                    nix::poll::PollFlags::POLLIN,
                ));
            }
            for &fd in &out_ids {
                pfds.push(nix::poll::PollFd::new(
                    borrowed_fd(fd),
                    nix::poll::PollFlags::POLLIN,
                ));
            }
            for &(fd, wants_write) in &control_ids {
                let flags = if wants_write {
                    nix::poll::PollFlags::POLLIN | nix::poll::PollFlags::POLLOUT
                } else {
                    nix::poll::PollFlags::POLLIN
                };
                pfds.push(nix::poll::PollFd::new(borrowed_fd(fd), flags));
            }
            #[cfg(all(target_os = "linux", feature = "udev"))]
            if let Some(m) = &self.udev {
                pfds.push(nix::poll::PollFd::new(
                    m.as_fd(),
                    nix::poll::PollFlags::POLLIN,
                ));
            }

            if nix::poll::poll(&mut pfds, timeout).unwrap_or(0) == 0 {
                continue;
            }

            // Extract readiness before touching `self` mutably.
            let mut idx = 0usize;
            let sig_ready = if has_sig {
                let r = pfds[idx]
                    .revents()
                    .unwrap_or(nix::poll::PollFlags::empty())
                    .contains(nix::poll::PollFlags::POLLIN);
                idx += 1;
                r
            } else {
                false
            };
            let listener_ready = if has_listener {
                let r = pfds[idx]
                    .revents()
                    .unwrap_or(nix::poll::PollFlags::empty())
                    .contains(nix::poll::PollFlags::POLLIN);
                idx += 1;
                r
            } else {
                false
            };
            let notify_ready = if has_notify {
                let r = pfds[idx]
                    .revents()
                    .unwrap_or(nix::poll::PollFlags::empty())
                    .contains(nix::poll::PollFlags::POLLIN);
                idx += 1;
                r
            } else {
                false
            };
            #[cfg(feature = "socket")]
            let socket_ready: Vec<(RawFd, bool)> = socket_ids
                .iter()
                .map(|fd| {
                    let r = pfds[idx]
                        .revents()
                        .unwrap_or(nix::poll::PollFlags::empty())
                        .contains(nix::poll::PollFlags::POLLIN);
                    idx += 1;
                    (*fd, r)
                })
                .collect();
            let out_ready: Vec<(RawFd, bool)> = out_ids
                .iter()
                .map(|fd| {
                    let r = pfds[idx]
                        .revents()
                        .unwrap_or(nix::poll::PollFlags::empty())
                        .contains(nix::poll::PollFlags::POLLIN);
                    idx += 1;
                    (*fd, r)
                })
                .collect();
            let control_ready: Vec<(RawFd, bool, bool)> = control_ids
                .iter()
                .map(|&(fd, _wants_write)| {
                    let rev = pfds[idx].revents().unwrap_or(nix::poll::PollFlags::empty());
                    idx += 1;
                    (
                        fd,
                        rev.contains(nix::poll::PollFlags::POLLIN),
                        rev.contains(nix::poll::PollFlags::POLLOUT),
                    )
                })
                .collect();
            #[cfg(all(target_os = "linux", feature = "udev"))]
            let udev_ready = if has_udev {
                pfds[idx]
                    .revents()
                    .unwrap_or(nix::poll::PollFlags::empty())
                    .contains(nix::poll::PollFlags::POLLIN)
            } else {
                false
            };

            if sig_ready {
                self.read_signalfd();
            }
            if listener_ready {
                self.accept_connections();
            }
            if notify_ready {
                self.read_notify();
            }
            #[cfg(feature = "socket")]
            for (fd, ready) in &socket_ready {
                if *ready {
                    // Idempotent: start() no-ops if the service is already
                    // active or has a pending start job.
                    if let Some((_, service)) = self.socket_triggers.get(fd).cloned() {
                        let _ = self.start(&service);
                    }
                }
            }
            for (fd, ready) in out_ready {
                if ready {
                    self.read_stdout(fd);
                }
            }
            for (fd, read_ready, write_ready) in control_ready {
                if read_ready {
                    self.drain_control_client(fd);
                }
                if write_ready {
                    self.flush_control_response(fd);
                }
            }
            #[cfg(all(target_os = "linux", feature = "udev"))]
            if udev_ready {
                self.udev_process();
            }
        }
    }

    #[cfg(windows)]
    pub fn run(&mut self) {
        self.process_jobs();
        loop {
            self.tick(Instant::now());
            self.reap();
            self.read_signalfd();
            self.drain_windows_output();
            self.drain_windows_ipc();

            #[cfg(feature = "socket")]
            {
                let ready: Vec<SocketId> = self
                    .socket_triggers
                    .iter()
                    .filter(|(_, (_, service))| {
                        self.units
                            .get(service)
                            .map(|unit| {
                                !matches!(
                                    unit.active,
                                    ActiveState::Active
                                        | ActiveState::Activating
                                        | ActiveState::Deactivating
                                )
                            })
                            .unwrap_or(true)
                    })
                    .filter_map(|(id, _)| {
                        self.socket_listeners
                            .get(id)
                            .is_some_and(SocketListener::take_trigger)
                            .then_some(*id)
                    })
                    .collect();
                for id in ready {
                    if let Some((_, service)) = self.socket_triggers.get(&id).cloned() {
                        let _ = self.start(&service);
                    }
                }
            }

            if self.shutting_down && self.idle() {
                break;
            }
            let sleep = self
                .wheel
                .next_deadline()
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_millis(50))
                .min(Duration::from_millis(50));
            std::thread::sleep(sleep.max(Duration::from_millis(1)));
        }
    }

    #[cfg(windows)]
    fn drain_windows_ipc(&mut self) {
        let requests = self
            .listener
            .as_ref()
            .map(|listener| listener.drain())
            .unwrap_or_default();
        for request in requests {
            let response = crate::ipc::dispatch(self, &request.line);
            let mut line = serde_json::to_string(&response).unwrap_or_else(|_| "{}".into());
            line.push('\n');
            request.respond(line);
        }
    }

    #[cfg(windows)]
    fn drain_windows_output(&mut self) {
        for (name, bytes) in spawn::drain_output() {
            if let Some(unit) = self.units.get_mut(&name) {
                unit.log.push_chunk(&String::from_utf8_lossy(&bytes));
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            self.journal
                .append(&name, crate::journal::timestamp_secs(), &text);
        }
    }

    fn read_signalfd(&mut self) {
        let Some(sfd) = &self.signalfd else { return };
        let signals = sfd.read();
        for s in signals {
            self.handle_signals(s);
        }
    }

    #[cfg(unix)]
    fn accept_connections(&mut self) {
        let Some(listener) = &self.listener else {
            return;
        };
        // Bound concurrent clients so slow or stalled peers cannot exhaust
        // manager file descriptors or memory. Excess accepts are dropped.
        //
        // Re-check the cap inside the loop: with a kernel backlog of queued
        // connections the accept loop can otherwise keep growing the map
        // past `MAX_CONCURRENT_CLIENTS` for every iteration before EWOULDBLOCK.
        const MAX_CONCURRENT_CLIENTS: usize = 1024;
        while let Ok((stream, _)) = listener.accept() {
            if self.control_clients.len() >= MAX_CONCURRENT_CLIENTS
                || stream.set_nonblocking(true).is_err()
            {
                continue;
            }
            // Only the manager's own UID may issue control requests. This is
            // the authoritative gate; the owner-only socket mode is defense
            // in depth. A system manager (root) accepts only root; a user
            // manager accepts only its owner. A peer whose UID cannot be
            // resolved (kernel without `SO_PEERCRED`, transient error at
            // accept time) is rejected — fail closed, not open.
            match crate::platform::net::peer_uid(&stream) {
                Some(uid) if uid == self.cfg.uid => {
                    let fd = stream.as_raw_fd();
                    self.control_clients.insert(
                        fd,
                        PendingClient {
                            stream,
                            buffer: Vec::new(),
                            out: None,
                        },
                    );
                }
                _ => continue,
            }
        }
    }

    #[cfg(unix)]
    fn read_notify(&mut self) {
        let datagrams: Vec<Vec<u8>> = {
            let Some(sock) = &self.notify else {
                return;
            };
            let mut out = Vec::new();
            let mut buf = [0u8; 2048];
            while let Ok((n, _)) = sock.recv_from(&mut buf) {
                if n == 0 {
                    break;
                }
                out.push(buf[..n].to_vec());
            }
            out
        };
        for d in datagrams {
            self.handle_notify_datagram(&d);
        }
    }

    #[cfg(unix)]
    fn handle_notify_datagram(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let mut ready = false;
        let mut mainpid: Option<i32> = None;
        for kv in text.split('\n') {
            let Some(eq) = kv.find('=') else { continue };
            match (&kv[..eq], &kv[eq + 1..]) {
                ("READY", "1") => ready = true,
                ("MAINPID", v) => mainpid = v.trim().parse().ok(),
                _ => {}
            }
        }
        let candidates: Vec<String> = self
            .units
            .iter()
            .filter(|(_, u)| {
                u.service_cfg()
                    .map(|s| s.service_type == ServiceType::Notify)
                    .unwrap_or(false)
                    && (u.active == ActiveState::Activating || u.active == ActiveState::Active)
            })
            .map(|(n, _)| n.clone())
            .collect();
        let target = if let Some(mp) = mainpid {
            candidates
                .iter()
                .find(|c| self.pid_unit.get(&mp).map(|n| n == *c).unwrap_or(false))
                .cloned()
                .or_else(|| candidates.first().cloned())
        } else {
            candidates.first().cloned()
        };
        let Some(unit) = target else { return };
        if ready && self.units[&unit].active == ActiveState::Activating {
            self.units.get_mut(&unit).unwrap().set_active(
                ActiveState::Active,
                SubState::Running,
                UnitResult::Success,
            );
            self.complete_start_job(&unit);
        }
    }

    #[cfg(unix)]
    fn read_stdout(&mut self, fd: RawFd) {
        let name = match self.out_fds.get(&fd) {
            Some(n) => n.clone(),
            None => return,
        };
        let mut buf = [0u8; 4096];
        loop {
            let n = match nix::unistd::read(borrowed_fd(fd), &mut buf) {
                Ok(n) => n,
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(_) => {
                    self.out_fds.remove(&fd);
                    self.owned_fds.remove(&fd);
                    break;
                }
            };
            if n == 0 {
                self.out_fds.remove(&fd);
                self.owned_fds.remove(&fd);
                break;
            }
            if let Some(u) = self.units.get_mut(&name) {
                u.log.push_chunk(&String::from_utf8_lossy(&buf[..n]));
            }
            // Durable journal: same captured bytes go to the disk store.
            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
            self.journal
                .append(&name, crate::journal::timestamp_secs(), &text);
        }
    }
}

impl Manager {
    /// Called when a `Type=dbus` main process has been spawned: the unit stays
    /// `activating` until `BusName=` is acquired (Linux, `dbus` feature) or,
    /// absent D-Bus support, until `TimeoutStartSec`.
    fn begin_bus_name_wait(&mut self, name: &str, bus_name: Option<&str>) {
        let Some(bn) = bus_name else {
            // Type=dbus requires BusName=; without it we can never go active.
            self.mgr(name, "Type=dbus requires BusName=; failing");
            self.units.get_mut(name).unwrap().result = UnitResult::Protocol;
            self.fail_unit(name, "Type=dbus without BusName=".to_string());
            return;
        };
        self.units.get_mut(name).unwrap().sub = SubState::WaitingForBus;
        #[cfg(all(target_os = "linux", feature = "dbus"))]
        self.watch_bus_name(name, bn);
        #[cfg(not(all(target_os = "linux", feature = "dbus")))]
        self.mgr(
            name,
            &format!(
                "BusName={bn} ignored: D-Bus support is disabled (build without the `dbus` feature)"
            ),
        );
    }
}

#[cfg(all(target_os = "linux", feature = "dbus"))]
impl Manager {
    /// Bring up the D-Bus bridge (control interface + name-ownership
    /// monitoring) on a dedicated thread. Safe to call even when no bus is
    /// reachable: the thread logs a warning and exits, and the manager keeps
    /// running without D-Bus.
    pub fn start_dbus(&mut self) -> Result<(), String> {
        if self.dbus.is_some() {
            return Ok(());
        }
        self.dbus = Some(crate::dbus::spawn(self.cfg.user, self.cfg.uid));
        Ok(())
    }

    /// Drain queued D-Bus control requests and name-ownership events. Called
    /// from the event loop; never blocks.
    fn drain_dbus(&mut self) {
        // Collect the queued work first, so the immutable borrow of
        // `self.dbus` ends before the mutable processing below.
        let (requests, events): (Vec<crate::dbus::DbRequest>, Vec<crate::dbus::DbEvent>) = {
            let Some(handle) = &self.dbus else {
                return;
            };
            let mut requests = Vec::new();
            while let Ok(r) = handle.commands.try_recv() {
                requests.push(r);
            }
            let mut events = Vec::new();
            while let Ok(ev) = handle.events.try_recv() {
                events.push(ev);
            }
            // Push a fresh full-unit snapshot for the systemd1-compatible
            // surface, built exactly like the ListUnits branch below. The
            // dbus thread coalesces bursts, and send errors (dbus thread gone)
            // are ignored.
            let mut names: Vec<String> = self.units.keys().cloned().collect();
            names.sort();
            let mut snapshot: Vec<crate::dbus::UnitEntry> = Vec::with_capacity(names.len());
            for n in names {
                if let Some(e) = self.dbus_entry(&n) {
                    snapshot.push(e);
                }
            }
            let _ = handle
                .unit_tx
                .send(crate::dbus::DbUnitEvent::Snapshot(snapshot));
            (requests, events)
        };

        // Control requests from method handlers (ListUnits/GetUnit/…).
        for req in requests {
            let reply = self.handle_dbus_op(&req.op);
            let _ = req.reply.send(reply);
        }
        // Name-ownership events from the monitor thread.
        for ev in events {
            self.handle_dbus_event(ev);
        }
        // Starting/stopping units from D-Bus enqueues jobs; keep them moving.
        self.process_jobs();
    }

    fn handle_dbus_op(&mut self, op: &crate::dbus::DbOp) -> crate::dbus::DbReply {
        use crate::dbus::{DbOp, DbReply};
        match op {
            DbOp::ListUnits => {
                let mut rows = Vec::new();
                let mut names: Vec<String> = self.units.keys().cloned().collect();
                names.sort();
                for n in names {
                    if let Some(e) = self.dbus_entry(&n) {
                        rows.push(e);
                    }
                }
                DbReply::UnitList(rows)
            }
            DbOp::GetUnit(name) => {
                DbReply::Unit(self.dbus_entry(&crate::names::normalize_unit(name)))
            }
            DbOp::StartUnit(name) => {
                let n = crate::names::normalize_unit(name);
                match self.start(&n) {
                    Ok(()) => DbReply::UnitStarted,
                    Err(e) => DbReply::Error(e),
                }
            }
            DbOp::StopUnit(name) => {
                let n = crate::names::normalize_unit(name);
                match self.stop(&n) {
                    Ok(()) => DbReply::UnitStopped,
                    Err(e) => DbReply::Error(e),
                }
            }
            DbOp::ListUnits1 => {
                let mut rows = Vec::new();
                let mut names: Vec<String> = self.units.keys().cloned().collect();
                names.sort();
                for n in names {
                    if let Some(e) = self.dbus_entry(&n) {
                        rows.push(self.unit_info1(&n, e));
                    }
                }
                DbReply::UnitList1(rows)
            }
            DbOp::GetUnit1(name) => {
                let n = crate::names::normalize_unit(name);
                DbReply::Unit1(self.dbus_entry(&n).map(|e| self.unit_info1(&n, e)))
            }
            DbOp::LoadUnit(name) => {
                let n = crate::names::normalize_unit(name);
                if self.units.contains_key(&n) {
                    DbReply::UnitPath(crate::dbus::unit_dbus_path(&n))
                } else {
                    match self.load_unit(&n) {
                        Ok(Some(unit)) => {
                            // Fresh load inserts the unit into the table.
                            self.units.insert(n.clone(), unit);
                            DbReply::UnitPath(crate::dbus::unit_dbus_path(&n))
                        }
                        Ok(None) => DbReply::Error(format!("No such unit '{n}'")),
                        Err(e) => DbReply::Error(e),
                    }
                }
            }
            DbOp::ListJobs => {
                let mut jobs = Vec::new();
                let mut ids: Vec<u64> = self.jobs.keys().cloned().collect();
                ids.sort();
                for id in ids {
                    if let Some(job) = self.jobs.get(&id) {
                        jobs.push(crate::dbus::JobInfo {
                            id: id as u32,
                            unit: job.unit.clone(),
                            job_type: job_kind_str(job.kind).to_string(),
                            state: if job.waiting.is_empty() {
                                "running".to_string()
                            } else {
                                "waiting".to_string()
                            },
                            unit_path: crate::dbus::unit_dbus_path(&job.unit),
                            job_path: format!("/org/freedesktop/systemd1/job/{id}"),
                        });
                    }
                }
                DbReply::JobList(jobs)
            }
            DbOp::GetUnitProcesses(name) => {
                let n = crate::names::normalize_unit(name);
                DbReply::ProcessList(self.unit_processes(&n))
            }
        }
    }

    fn handle_dbus_event(&mut self, ev: crate::dbus::DbEvent) {
        match ev {
            crate::dbus::DbEvent::NameAcquired(bus_name) => {
                let Some(unit) = self.pending_bus_names.remove(&bus_name) else {
                    return;
                };
                // Stop watching now that the unit is active.
                if let Some(h) = &self.dbus {
                    let _ = h
                        .watch_tx
                        .send(crate::dbus::DbWatch::Remove(bus_name.clone()));
                }
                let still_activating = self
                    .units
                    .get(&unit)
                    .map(|u| u.active == ActiveState::Activating)
                    .unwrap_or(false);
                if still_activating {
                    self.units.get_mut(&unit).unwrap().set_active(
                        ActiveState::Active,
                        SubState::Running,
                        UnitResult::Success,
                    );
                    self.complete_start_job(&unit);
                    self.mgr(&unit, &format!("D-Bus name {bus_name} acquired"));
                }
            }
            crate::dbus::DbEvent::NameLost(_) => {}
        }
    }

    fn dbus_entry(&self, name: &str) -> Option<crate::dbus::UnitEntry> {
        let u = self.units.get(name)?;
        Some(crate::dbus::UnitEntry {
            name: u.name.clone(),
            load: crate::manager::ops::load_str(u.load).to_string(),
            active: crate::manager::ops::active_str(u.active).to_string(),
            sub: u.sub.as_str().to_string(),
            description: u
                .file
                .as_ref()
                .map(|f| f.unit.description.clone())
                .unwrap_or_default(),
        })
    }

    /// Expand a native [`crate::dbus::UnitEntry`] into the systemd1 `UnitInfo`
    /// struct fields, resolving the unit's object path and the job currently
    /// targeting it (if any).
    fn unit_info1(&self, name: &str, e: crate::dbus::UnitEntry) -> crate::dbus::UnitInfo1 {
        let (job_id, job_type, job_object_path) = match self
            .jobs
            .iter()
            .find(|(_, j)| j.unit == name)
            .map(|(&id, j)| (id, job_kind_str(j.kind)))
        {
            Some((id, kind)) => (
                id as u32,
                kind.to_string(),
                format!("/org/freedesktop/systemd1/job/{id}"),
            ),
            None => (0, String::new(), "/".to_string()),
        };
        crate::dbus::UnitInfo1 {
            name: e.name,
            description: e.description,
            load: e.load,
            active: e.active,
            sub: e.sub,
            following: String::new(),
            object_path: crate::dbus::unit_dbus_path(name),
            job_id,
            job_type,
            job_object_path,
        }
    }

    /// The processes of a unit as systemd1 `GetUnitProcesses` entries. Reads
    /// the unit's cgroup `cgroup.procs` when it has a cgroup; otherwise falls
    /// back to the main pid. Returns an empty list when the unit is not
    /// running.
    fn unit_processes(&self, name: &str) -> Vec<crate::dbus::ProcessInfo> {
        let Some(u) = self.units.get(name) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(dir) = &u.cgroup {
            let cgroup = dir.display().to_string();
            if let Ok(text) = std::fs::read_to_string(dir.join("cgroup.procs")) {
                for line in text.lines() {
                    let Ok(pid) = line.trim().parse::<u32>() else {
                        continue;
                    };
                    out.push(crate::dbus::ProcessInfo {
                        cgroup: cgroup.clone(),
                        pid,
                        cmdline: read_cmdline(pid),
                    });
                }
            }
        } else if let Some(pid) = u.main_pid {
            out.push(crate::dbus::ProcessInfo {
                cgroup: String::new(),
                pid: pid as u32,
                cmdline: read_cmdline(pid as u32),
            });
        }
        out
    }

    /// Register `unit` as waiting on `bus_name` and ask the monitor thread to
    /// watch for it.
    fn watch_bus_name(&mut self, unit: &str, bus_name: &str) {
        self.pending_bus_names
            .insert(bus_name.to_string(), unit.to_string());
        if let Some(h) = &self.dbus {
            let _ = h
                .watch_tx
                .send(crate::dbus::DbWatch::Add(bus_name.to_string()));
        }
    }

    /// Drop any pending `BusName=` watches registered for `unit`.
    fn release_bus_name_watch(&mut self, unit: &str) {
        let names: Vec<String> = self
            .pending_bus_names
            .iter()
            .filter(|(_, u)| *u == unit)
            .map(|(n, _)| n.clone())
            .collect();
        for n in names {
            self.pending_bus_names.remove(&n);
            if let Some(h) = &self.dbus {
                let _ = h.watch_tx.send(crate::dbus::DbWatch::Remove(n));
            }
        }
    }
}

impl Default for ManagerCfg {
    fn default() -> Self {
        ManagerCfg::for_mode(false).expect("default cfg")
    }
}

// ---- helpers ----------------------------------------------------------------

fn unit_kind_of(name: &str) -> UnitKind {
    UnitKind::from_unit_name(name).unwrap_or(UnitKind::Service)
}

/// The systemd wire string for a [`JobKind`] (`start`/`stop`/`restart`).
fn job_kind_str(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Start => "start",
        JobKind::Stop => "stop",
        JobKind::Restart => "restart",
    }
}

/// Read a pid's `/proc/<pid>/cmdline` (NUL-separated args) as a single
/// space-joined string. Empty string when the process is gone or unreadable.
#[cfg(all(target_os = "linux", feature = "dbus"))]
fn read_cmdline(pid: u32) -> String {
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return String::new();
    };
    let spaced: Vec<u8> = bytes
        .iter()
        .map(|&b| if b == 0 { b' ' } else { b })
        .collect();
    String::from_utf8_lossy(&spaced).trim().to_string()
}

fn is_builtin(name: &str) -> bool {
    // Always-resolvable aggregation targets plus the boot-time ones (the
    // latter are only relevant when rystemd runs as PID 1).
    name == "basic.target"
        || name == "multi-user.target"
        || name == "default.target"
        || (cfg!(feature = "boot")
            && (name == "sysinit.target" // empty aggregation target (real systemd: pulls in its .wants)
                || name == "graphical.target"
                || name == "getty.target"))
}

/// Evaluate a `.path` unit's watch conditions against the live filesystem,
/// updating `PathChanged=` mtime bookkeeping. True when any condition fires.
fn eval_path_triggers(pc: &PathConfig, st: &mut PathState) -> bool {
    if pc.path_exists.iter().any(|p| std::fs::metadata(p).is_ok()) {
        return true;
    }
    if pc.directory_not_empty.iter().any(|p| {
        std::fs::read_dir(p)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false)
    }) {
        return true;
    }
    if pc.path_exists_glob.iter().any(|pat| matches_glob_any(pat)) {
        return true;
    }
    for p in &pc.path_changed {
        if path_changed_triggered(p, st) {
            return true;
        }
    }
    false
}

/// `PathChanged=` semantics: fire when the path exists and its mtime differs
/// from the mtime recorded the first time the unit observed it. Unlike
/// `PathExists=`/`DirectoryNotEmpty=`, a path that already exists at arm time
/// does *not* trigger immediately (systemd.path(5)). The baseline is
/// re-recorded on every change, so a stop → re-arm cycle only re-fires on a
/// *new* distinct mtime, never on the same one.
fn path_changed_triggered(path: &str, st: &mut PathState) -> bool {
    let now = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let baseline = st.armed_mtimes.get(path).copied().flatten();
    match (st.armed_mtimes.contains_key(path), baseline, now) {
        // First observation: record the baseline, do not fire.
        (false, _, m) => {
            st.armed_mtimes.insert(path.to_string(), m);
            false
        }
        // Path appeared after being absent at arm time → a change.
        (true, None, Some(m)) => {
            st.armed_mtimes.insert(path.to_string(), Some(m));
            true
        }
        // mtime differs from the recorded baseline → fire and re-baseline.
        (true, Some(prev), Some(m)) if prev != m => {
            st.armed_mtimes.insert(path.to_string(), Some(m));
            true
        }
        // Path vanished → record absence, no fire.
        (true, Some(_), None) => {
            st.armed_mtimes.insert(path.to_string(), None);
            false
        }
        _ => false,
    }
}

/// Match a `PathExistsGlob=` spec: a minimal glob (`*` = any chars, `?` =
/// exactly one char within a single path component) evaluated by listing the
/// spec's parent directory and matching entries against the final component.
/// No `/`, `[...]`, or `{...}` support. A spec with no wildcards is treated as
/// plain `PathExists`.
fn matches_glob_any(pattern: &str) -> bool {
    let (parent, glob) = match pattern.rfind('/') {
        Some(i) => (&pattern[..i], &pattern[i + 1..]),
        None => ("", pattern),
    };
    if !glob.contains('*') && !glob.contains('?') {
        return std::path::Path::new(pattern).exists();
    }
    let parent = if parent.is_empty() { "." } else { parent };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return false;
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .any(|name| glob_match(glob, &name))
}

/// Backtracking `*`/`?` matcher for a single path component.
fn glob_match(pat: &str, name: &str) -> bool {
    fn rec(pat: &[u8], name: &[u8]) -> bool {
        match pat.first() {
            None => name.is_empty(),
            Some(b'*') => {
                // Collapse consecutive stars, then try every split point.
                let mut rest_start = 1;
                while rest_start < pat.len() && pat[rest_start] == b'*' {
                    rest_start += 1;
                }
                let rest = &pat[rest_start..];
                (0..=name.len()).any(|k| rec(rest, &name[k..]))
            }
            Some(b'?') => !name.is_empty() && rec(&pat[1..], &name[1..]),
            Some(&c) => name.first() == Some(&c) && rec(&pat[1..], &name[1..]),
        }
    }
    rec(pat.as_bytes(), name.as_bytes())
}

fn kind_unit_needs_file(kind: UnitKind) -> bool {
    kind == UnitKind::Target
}

fn builtin_target(name: &str) -> Unit {
    let mut u = Unit::new(name, UnitKind::Target);
    u.load = LoadState::Loaded;
    u.file = Some(UnitFile {
        path: None,
        unit: crate::unit::UnitConfig {
            description: match name {
                "basic.target" => "Basic System".into(),
                #[cfg(feature = "boot")]
                "sysinit.target" => "System Initialization".into(),
                "multi-user.target" => "Multi-User System".into(),
                #[cfg(feature = "boot")]
                "graphical.target" => "Graphical Interface".into(),
                #[cfg(feature = "boot")]
                "getty.target" => "Login Prompts".into(),
                "default.target" => "Default".into(),
                _ => String::new(),
            },
            ..Default::default()
        },
        service: None,
        timer: None,
        path_unit: None,
        #[cfg(feature = "socket")]
        socket: None,
        #[cfg(target_os = "linux")]
        mount: None,
        install: Default::default(),
    });
    u
}

fn min_of(a: Option<Instant>, b: Instant) -> Option<Instant> {
    Some(match a {
        Some(x) => x.min(b),
        None => b,
    })
}

fn signal_name(sig: i32) -> String {
    crate::unit::sig_from_name(&format!("{sig}"))
        .map(|s| format!("{s}"))
        .unwrap_or_else(|| format!("{sig}"))
}

/// Borrow a raw fd for the duration of one poll/read call.
///
/// # Safety
/// The fd belongs to an owned handle held by the manager (a child stdout
/// pipe tracked in `out_fds`, or a bound socket). It stays open for the
/// whole event-loop iteration; we never poll an fd after removing it from
/// `out_fds`.
#[cfg(unix)]
fn borrowed_fd(fd: RawFd) -> BorrowedFd<'static> {
    // SAFETY: the fd is valid and stays open through the poll/read; see above.
    unsafe { BorrowedFd::borrow_raw(fd) }
}

// ---- trait helper for unit config checks -------------------------------------

/// Extension used by the loader to detect a blank synthesized target.
pub trait UnitConfigCheck {
    fn unit_defaults_empty(&self) -> bool;
}
impl UnitConfigCheck for crate::unit::UnitConfig {
    fn unit_defaults_empty(&self) -> bool {
        self.after.is_empty() && self.wants.is_empty() && self.requires.is_empty()
    }
}

// ---- udev device-tracking tests ---------------------------------------------

#[cfg(all(test, target_os = "linux", feature = "udev"))]
mod udev_tests {
    use super::*;
    use crate::platform::udev::Device;

    fn fake(devpath: &str, subsystem: &str, devname: &str) -> Device {
        Device {
            devpath: devpath.to_string(),
            subsystem: subsystem.to_string(),
            devname: devname.to_string(),
            devtype: String::new(),
        }
    }

    /// Registering a device inserts *both* names (sysfs-path primary + subsystem
    /// alias) as active `.device` units; removing it deletes both. This is the
    /// hotplug create/remove path, exercised without a live uevent.
    #[test]
    fn register_and_remove_track_device_units() {
        let mut mgr = Manager::new(ManagerCfg::for_mode(false).unwrap()).unwrap();
        let dev = fake("devices/virtual/test/fake0", "test", "fake0");

        mgr.udev_register(&dev);
        let primary = "sys-devices-virtual-test-fake0.device";
        let alias = "sys-test-fake0.device";
        assert_eq!(mgr.units.get(primary).unwrap().active, ActiveState::Active);
        assert_eq!(mgr.units.get(alias).unwrap().active, ActiveState::Active);
        assert_eq!(mgr.units.get(primary).unwrap().kind, UnitKind::Device);
        assert_eq!(mgr.units.get(primary).unwrap().load, LoadState::Loaded);
        assert_eq!(mgr.udev_devices.len(), 1);

        mgr.udev_remove(&dev);
        assert!(!mgr.units.contains_key(primary));
        assert!(!mgr.units.contains_key(alias));
        assert!(mgr.udev_devices.is_empty());
    }
}

// ---- load/start dependency-leniency tests -----------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a user-mode config whose search path is a scratch dir.
    fn scratch_cfg(dir: &tempfile::TempDir) -> ManagerCfg {
        let units = dir.path().join("units");
        std::fs::create_dir_all(&units).unwrap();
        let paths = Paths {
            user: true,
            unit_path: vec![units.clone()],
            config_dir: units,
            runtime_dir: dir.path().to_path_buf(),
        };
        ManagerCfg {
            user: true,
            paths,
            hostname: "testhost".into(),
            machine_id: "testid".into(),
            uid: 1000,
            username: "testuser".into(),
            home: "/".into(),
            base_env: HashMap::new(),
            journal_dir: dir.path().join("journal"),
            socket_activation: true,
        }
    }

    /// A dangling `.wants`-dir reference (e.g. `podman.socket` on a host
    /// without podman) must not be a load error — systemd silently ignores a
    /// dependency on a unit that has no backing file.
    #[cfg(unix)]
    #[test]
    fn missing_wants_reference_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let units = dir.path().join("units");
        std::fs::create_dir_all(units.join("sockets.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            "/nonexistent/podman.socket",
            units.join("sockets.target.wants/podman.socket"),
        )
        .unwrap();

        let mut mgr = Manager::new(scratch_cfg(&dir)).unwrap();
        let errs = mgr.load_all();
        assert!(
            errs.is_empty(),
            "missing dependency must not be a load error: {errs:?}"
        );
        assert!(!mgr.units.contains_key("podman.socket"));
    }

    /// `After=` and `Wants=` on units that do not exist must not activate (or
    /// block) anything: the unit starts cleanly and the missing targets are
    /// never pulled into the unit table.
    #[test]
    fn missing_after_and_wants_deps_are_silent_and_do_not_block_start() {
        let dir = tempfile::tempdir().unwrap();
        let units = dir.path().join("units");
        std::fs::create_dir_all(&units).unwrap();
        std::fs::write(
            units.join("a.target"),
            "[Unit]\nDescription=test\nAfter=graphical.target\nWants=missing.service\n",
        )
        .unwrap();

        let mut mgr = Manager::new(scratch_cfg(&dir)).unwrap();
        mgr.load_all();
        mgr.start("a.target").unwrap();

        assert_eq!(mgr.units["a.target"].active, ActiveState::Active);
        #[cfg(not(feature = "boot"))]
        // Non-boot: graphical.target is not a known target, so an After= to it
        // stays dangling (silent). With the boot feature it IS a preseeded
        // builtin — present but never started (After only orders, never
        // activates) — so we assert it stayed inactive instead in that build.
        assert!(!mgr.units.contains_key("graphical.target"));
        #[cfg(feature = "boot")]
        assert_eq!(mgr.units["graphical.target"].active, ActiveState::Inactive);
        assert!(!mgr.units.contains_key("missing.service"));
    }

    #[cfg(feature = "socket")]
    #[test]
    fn list_unit_files_includes_socket_units() {
        let dir = tempfile::tempdir().unwrap();
        let units = dir.path().join("units");
        std::fs::create_dir_all(&units).unwrap();
        std::fs::write(
            units.join("api.socket"),
            "[Socket]\nListenStream=127.0.0.1:0\n",
        )
        .unwrap();
        let mut mgr = Manager::new(scratch_cfg(&dir)).unwrap();
        mgr.load_all();
        assert!(
            mgr.list_unit_file_info()
                .iter()
                .any(|entry| entry.file == "api.socket")
        );
    }

    /// A `Requires=` dependency that fails *synchronously* (its job completes
    /// inside the parent's `expand_start_job`, e.g. a `.mount` whose `mount(2)`
    /// returns EPERM/ENOENT) must fail the parent — not hang, and not silently
    /// succeed. Regression test for the `waiting.retain` sync-failure handling.
    #[cfg(target_os = "linux")]
    #[test]
    fn required_dependency_that_fails_synchronously_fails_parent() {
        let dir = tempfile::tempdir().unwrap();
        let units = dir.path().join("units");
        std::fs::create_dir_all(&units).unwrap();
        std::fs::write(
            units.join("a.target"),
            "[Unit]\nDescription=parent\nRequires=b.mount\n",
        )
        .unwrap();
        // `Where=` points at a directory we never create, so mount(2) fails
        // (ENOENT) deterministically — regardless of whether we run as root.
        let mnt = dir.path().join("mnt");
        std::fs::write(
            units.join("b.mount"),
            format!("[Mount]\nWhat=tmpfs\nWhere={}\nType=tmpfs\n", mnt.display()),
        )
        .unwrap();

        let mut mgr = Manager::new(scratch_cfg(&dir)).unwrap();
        mgr.load_all();
        mgr.start("a.target").unwrap();

        assert_eq!(mgr.units["b.mount"].active, ActiveState::Failed);
        assert_eq!(mgr.units["a.target"].active, ActiveState::Inactive);
        assert!(
            !mgr.unit_job.contains_key("a.target"),
            "parent job must resolve (fail), not hang on a dead dependency"
        );
    }

    /// `PathChanged=` must not fire for a path that already exists at arm time,
    /// and must re-baseline after a change so a stop → re-arm cycle does not
    /// re-fire on the same mtime (systemd.path(5)). Regression test.
    #[test]
    fn path_changed_only_fires_on_distinct_mtimes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("watch.conf");
        std::fs::write(&file, "v0").unwrap();
        let path = file.to_string_lossy();
        let mut st = PathState::default();

        // Existing at arm time: record baseline, do NOT fire.
        assert!(!path_changed_triggered(&path, &mut st));
        // Same mtime on a second poll: still no fire.
        assert!(!path_changed_triggered(&path, &mut st));

        // A real change fires once and re-baselines...
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, "v1").unwrap();
        assert!(path_changed_triggered(&path, &mut st));
        // ...so the same mtime does not re-fire.
        assert!(!path_changed_triggered(&path, &mut st));

        // Disappearance records absence and does not fire...
        std::fs::remove_file(&file).unwrap();
        assert!(!path_changed_triggered(&path, &mut st));
        // ...and a reappearance is a fresh change.
        std::fs::write(&file, "v2").unwrap();
        assert!(path_changed_triggered(&path, &mut st));
    }
}
