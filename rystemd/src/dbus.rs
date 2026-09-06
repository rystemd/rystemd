//! D-Bus integration (Linux only): `Type=dbus`/`BusName=` service activation
//! and a small manager control interface.
//!
//! # Threading model
//!
//! The manager's poll loop is single-threaded and synchronous, so **no**
//! D-Bus work happens there. All of it lives on dedicated threads, bridged to
//! the manager over `std::sync::mpsc` channels that the loop drains each
//! iteration (bounded to ≤ ~1s by the poll-timeout cap):
//!
//! - A *monitor thread* owns a blocking [`zbus::blocking::Connection`] (system
//!   bus in system mode, session bus in user mode). It serves the manager
//!   interface, owns the well-known name, and polls `org.freedesktop.DBus`
//!   `NameHasOwner` for the `BusName=` of every activating `Type=dbus` unit,
//!   forwarding acquisitions to the manager.
//! - The interface's method handlers run on zbus's internal executor threads;
//!   each forwards a request to the manager over a channel and blocks for the
//!   reply, so the manager stays the single writer of unit state.
//!
//! zbus is pulled in with `default-features = false` and just
//! `["blocking-api", "async-io"]`, keeping the dependency tree pure-Rust with
//! no C compiler and no tokio.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::fdo::RequestNameFlags;
use zbus::interface;
use zbus::zvariant::ObjectPath;

use crate::log::mgr_log;

/// Well-known bus name rystemd owns on the (system or session) bus.
const BUS_NAME: &str = "org.rystemd.Manager1";
/// Object path the manager interface is exposed at.
const OBJECT_PATH: &str = "/org/rystemd/Manager1";
/// Well-known bus name of the systemd1-compatible surface (only owned when it
/// is free, i.e. no real systemd is present on the bus).
const SYSTEMD1_BUS_NAME: &str = "org.freedesktop.systemd1";
/// Object path the systemd1-compatible manager interface is exposed at.
const SYSTEMD1_OBJECT_PATH: &str = "/org/freedesktop/systemd1";

/// One unit's listing row, as returned over the bus.
#[derive(Debug, Clone)]
pub struct UnitEntry {
    pub name: String,
    pub load: String,
    pub active: String,
    pub sub: String,
    pub description: String,
}

/// A unit's `(name, load, active, sub, description)` tuple on the wire.
pub type UnitRow = (String, String, String, String, String);

/// One row of the systemd1-compatible `ListUnits` reply: the `UnitInfo`
/// struct array members in wire order, as plain fields the manager fills in.
#[derive(Debug, Clone)]
pub struct UnitInfo1 {
    pub name: String,
    pub description: String,
    pub load: String,
    pub active: String,
    pub sub: String,
    pub following: String,
    pub object_path: String,
    pub job_id: u32,
    pub job_type: String,
    pub job_object_path: String,
}

/// One row of the systemd1-compatible `ListJobs` reply: the `JobInfo`
/// struct array members in wire order.
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: u32,
    pub unit: String,
    pub job_type: String,
    pub state: String,
    pub unit_path: String,
    pub job_path: String,
}

/// One entry of the systemd1-compatible `GetUnitProcesses` reply.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub cgroup: String,
    pub pid: u32,
    pub cmdline: String,
}

/// Build the object path under which a unit's `org.freedesktop.systemd1.Unit`
/// interface is served, escaping `name` per systemd's `bus_label_escape`:
/// ASCII alphanumerics are emitted unchanged, and every other byte as `_`
/// followed by its two lowercase hex digits. So `foo.service` maps to
/// `/org/freedesktop/systemd1/unit/foo_2eservice`.
pub fn unit_dbus_path(name: &str) -> String {
    let mut out = String::with_capacity(30 + name.len());
    out.push_str("/org/freedesktop/systemd1/unit/");
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(b as char);
        } else {
            out.push('_');
            out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{DbOp, DbReply, DbRequest, bridge_call, uid_allowed, unit_dbus_path};
    use std::sync::mpsc;

    #[test]
    fn test_unit_dbus_path_escape() {
        assert_eq!(
            unit_dbus_path("foo.service"),
            "/org/freedesktop/systemd1/unit/foo_2eservice"
        );
        assert_eq!(
            unit_dbus_path("systemd-udevd.service"),
            "/org/freedesktop/systemd1/unit/systemd_2dudevd_2eservice"
        );
        assert_eq!(
            unit_dbus_path("getty@tty1.service"),
            "/org/freedesktop/systemd1/unit/getty_40tty1_2eservice"
        );
        assert_eq!(
            unit_dbus_path("session-1.scope"),
            "/org/freedesktop/systemd1/unit/session_2d1_2escope"
        );
    }

    #[test]
    fn mutating_calls_are_limited_to_the_manager_owner_or_root() {
        // The manager's own UID can drive it.
        assert!(uid_allowed(1000, 1000));
        // Root can drive any manager.
        assert!(uid_allowed(0, 1000));
        assert!(uid_allowed(0, 0));
        // Any other uid is refused.
        assert!(!uid_allowed(1001, 1000));
        assert!(!uid_allowed(1000, 1001));
        assert!(!uid_allowed(999, 1000));
    }

    /// `bridge_call` forwards an op over the manager channel and returns the
    /// manager's reply — the mechanism every D-Bus method handler routes
    /// through. Testable without a live bus because it is a pure channel hop.
    #[test]
    fn bridge_call_returns_manager_reply() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<DbRequest>();
        let server = std::thread::spawn(move || {
            let req = cmd_rx.recv().unwrap();
            assert!(matches!(req.op, DbOp::ListUnits));
            req.reply.send(DbReply::UnitList(vec![])).unwrap();
        });
        let got = bridge_call(&cmd_tx, DbOp::ListUnits).unwrap();
        assert!(matches!(got, DbReply::UnitList(v) if v.is_empty()));
        server.join().unwrap();
    }

    /// A wedged/shut-down manager (dropped receiver) must surface as a clean
    /// error, not a panic or hang.
    #[test]
    fn bridge_call_errors_when_manager_channel_closed() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<DbRequest>();
        drop(cmd_rx);
        assert_eq!(
            bridge_call(&cmd_tx, DbOp::ListUnits).unwrap_err(),
            "manager channel closed"
        );
    }
}

/// A control operation routed from a D-Bus method handler to the manager.
#[derive(Debug)]
pub enum DbOp {
    ListUnits,
    GetUnit(String),
    StartUnit(String),
    StopUnit(String),
    ListUnits1,
    GetUnit1(String),
    LoadUnit(String),
    ListJobs,
    GetUnitProcesses(String),
}

/// A request from a D-Bus method handler to the manager. Carries its own
/// reply channel so concurrent method calls cannot race on a shared receiver.
pub struct DbRequest {
    pub reply: SyncSender<DbReply>,
    pub op: DbOp,
}

/// The manager's reply to a D-Bus method handler.
#[derive(Debug)]
pub enum DbReply {
    UnitList(Vec<UnitEntry>),
    Unit(Option<UnitEntry>),
    UnitStarted,
    UnitStopped,
    Error(String),
    UnitList1(Vec<UnitInfo1>),
    Unit1(Option<UnitInfo1>),
    UnitPath(String),
    JobList(Vec<JobInfo>),
    ProcessList(Vec<ProcessInfo>),
}

/// Name-ownership events forwarded from the monitor thread to the manager.
#[derive(Debug)]
pub enum DbEvent {
    NameAcquired(String),
    NameLost(String),
}

/// A full snapshot of the manager's unit state, pushed from the manager to the
/// monitor thread for the systemd1-compatible surface. The monitor thread
/// coalesces bursts (it only ever consumes/serves the most recent snapshot).
#[derive(Debug)]
pub enum DbUnitEvent {
    Snapshot(Vec<UnitEntry>),
}

/// Manager → monitor thread: which bus names to watch.
#[derive(Debug)]
pub enum DbWatch {
    Add(String),
    Remove(String),
}

/// The manager-side handle to the D-Bus bridge: channels to drain plus the
/// sender used to register/unregister name watches.
pub struct DbusHandle {
    pub events: Receiver<DbEvent>,
    pub commands: Receiver<DbRequest>,
    pub watch_tx: Sender<DbWatch>,
    pub unit_tx: Sender<DbUnitEvent>,
}

/// Start the D-Bus bridge on a dedicated thread and return the manager-side
/// handle. The connection itself happens on the thread; if the bus is
/// unreachable the thread logs a warning and exits, leaving the manager fully
/// functional without D-Bus (a `Type=dbus` unit then times out, matching
/// systemd's behaviour when the bus is absent).
pub fn spawn(user: bool, manager_uid: u32) -> DbusHandle {
    let (event_tx, event_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (watch_tx, watch_rx) = mpsc::channel();
    let (unit_tx, unit_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("rystemd-dbus".into())
        .spawn(move || run_thread(user, event_tx, cmd_tx, watch_rx, unit_rx, manager_uid))
        .ok();
    DbusHandle {
        events: event_rx,
        commands: cmd_rx,
        watch_tx,
        unit_tx,
    }
}

/// The manager interface: methods are CamelCased on the wire (`ListUnits`,
/// `GetUnit`, `StartUnit`, `StopUnit`) and introspection is served
/// automatically by zbus. Blocking methods are called on zbus executor
/// threads; each one bridges to the manager and blocks for its reply.
struct ManagerIface {
    cmd_tx: Sender<DbRequest>,
    /// The manager's own UID, used to authorize mutating calls.
    manager_uid: u32,
}

/// Pure policy: may a caller with `caller_uid` issue a mutating manager call
/// on a manager owned by `manager_uid`? The manager's owner, or root.
fn uid_allowed(caller_uid: u32, manager_uid: u32) -> bool {
    caller_uid == manager_uid || caller_uid == 0
}

/// Authorize a mutating call from the caller identified by `sender` on `conn`.
/// Identity comes from the bus (`UnixUserID`), never from the request body. An
/// unknown or non-matching caller gets `AccessDenied`.
async fn authorize(
    conn: &zbus::Connection,
    sender: Option<&zbus::names::UniqueName<'_>>,
    manager_uid: u32,
) -> zbus::fdo::Result<()> {
    let sender = sender
        .map(|u| u.as_str())
        .ok_or_else(|| zbus::fdo::Error::AccessDenied("no sender identity".into()))?;
    let reply = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionCredentials",
            &(sender),
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("caller credential lookup failed: {e}")))?;
    let creds: zbus::fdo::ConnectionCredentials = reply
        .body()
        .deserialize()
        .map_err(|e| zbus::fdo::Error::Failed(format!("bad credentials reply: {e}")))?;
    let caller = creds
        .unix_user_id()
        .ok_or_else(|| zbus::fdo::Error::AccessDenied("no unix identity".into()))?;
    if uid_allowed(caller, manager_uid) {
        Ok(())
    } else {
        Err(zbus::fdo::Error::AccessDenied(
            "caller not authorized".into(),
        ))
    }
}

#[interface(name = "org.rystemd.Manager1.Manager")]
impl ManagerIface {
    /// List loaded units: array of `(name, load, active, sub, description)`.
    fn list_units(&self) -> zbus::fdo::Result<Vec<UnitRow>> {
        match self.call(DbOp::ListUnits) {
            Ok(DbReply::UnitList(rows)) => Ok(rows
                .into_iter()
                .map(|r| (r.name, r.load, r.active, r.sub, r.description))
                .collect()),
            Ok(_) => Err(zbus::fdo::Error::Failed("unexpected reply".into())),
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }

    /// Look up one unit by name.
    fn get_unit(&self, name: String) -> zbus::fdo::Result<UnitRow> {
        match self.call(DbOp::GetUnit(name)) {
            Ok(DbReply::Unit(Some(r))) => Ok((r.name, r.load, r.active, r.sub, r.description)),
            Ok(DbReply::Unit(None)) => Err(zbus::fdo::Error::Failed("no such unit".into())),
            Ok(_) => Err(zbus::fdo::Error::Failed("unexpected reply".into())),
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }

    /// Start a unit. Mutating: only the manager's owner (or root) may call.
    async fn start_unit(
        &self,
        name: String,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        authorize(conn, header.sender(), self.manager_uid).await?;
        match self.call(DbOp::StartUnit(name)) {
            Ok(DbReply::UnitStarted) => Ok(()),
            Ok(DbReply::Error(e)) => Err(zbus::fdo::Error::Failed(e)),
            Ok(_) => Err(zbus::fdo::Error::Failed("unexpected reply".into())),
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }

    /// Stop a unit. Mutating: only the manager's owner (or root) may call.
    async fn stop_unit(
        &self,
        name: String,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        authorize(conn, header.sender(), self.manager_uid).await?;
        match self.call(DbOp::StopUnit(name)) {
            Ok(DbReply::UnitStopped) => Ok(()),
            Ok(DbReply::Error(e)) => Err(zbus::fdo::Error::Failed(e)),
            Ok(_) => Err(zbus::fdo::Error::Failed("unexpected reply".into())),
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }

    /// Manager version string.
    #[zbus(property)]
    fn version(&self) -> String {
        crate::VERSION.to_string()
    }
}

impl ManagerIface {
    /// Send a request to the manager and block for its reply.
    fn call(&self, op: DbOp) -> Result<DbReply, String> {
        bridge_call(&self.cmd_tx, op)
    }
}

/// Send a request to the manager and block for its reply (with a timeout,
/// so a wedged manager can't hang a D-Bus worker thread forever). Shared by
/// both the native `org.rystemd.Manager1` interface and the systemd1
/// `org.freedesktop.systemd1.Manager` surface.
fn bridge_call(cmd_tx: &Sender<DbRequest>, op: DbOp) -> Result<DbReply, String> {
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    cmd_tx
        .send(DbRequest {
            reply: reply_tx,
            op,
        })
        .map_err(|_| "manager channel closed".to_string())?;
    reply_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "manager did not reply (timeout)".to_string())
}

/// The systemd1-compatible `org.freedesktop.systemd1.Manager` interface. It
/// serves the `Version` property plus the systemd1 read methods
/// (`ListUnits`/`GetUnit`/`LoadUnit`/`ListJobs`/`GetUnitProcesses`), each
/// bridged to the manager over `cmd_tx`. The `UnitNew`/`UnitRemoved` signals
/// are emitted by the monitor thread, not declared here.
struct Systemd1ManagerIface {
    cmd_tx: Sender<DbRequest>,
}

impl Systemd1ManagerIface {
    /// Send a request to the manager and block for its reply.
    fn call(&self, op: DbOp) -> Result<DbReply, String> {
        bridge_call(&self.cmd_tx, op)
    }
}

#[interface(name = "org.freedesktop.systemd1.Manager")]
impl Systemd1ManagerIface {
    /// systemd1-compatible manager version string.
    #[zbus(property)]
    fn version(&self) -> String {
        crate::VERSION.to_string()
    }

    /// List loaded units. Returns the systemd1 `UnitInfo` struct array:
    /// `(name, description, load_state, active_state, sub_state, following,
    /// unit_path, job_id, job_type, job_path)`.
    #[allow(clippy::type_complexity)]
    fn list_units(
        &self,
    ) -> zbus::fdo::Result<
        Vec<(
            String,
            String,
            String,
            String,
            String,
            String,
            ObjectPath<'static>,
            u32,
            String,
            ObjectPath<'static>,
        )>,
    > {
        match self.call(DbOp::ListUnits1) {
            Ok(DbReply::UnitList1(rows)) => Ok(rows
                .into_iter()
                .map(|r| {
                    (
                        r.name,
                        r.description,
                        r.load,
                        r.active,
                        r.sub,
                        r.following,
                        ObjectPath::try_from(r.object_path).unwrap(),
                        r.job_id,
                        r.job_type,
                        ObjectPath::try_from(r.job_object_path).unwrap(),
                    )
                })
                .collect()),
            Ok(_) => Err(zbus::fdo::Error::Failed("unexpected reply".into())),
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }

    /// Return the object path of a named unit, loading it first.
    fn get_unit(&self, name: String) -> zbus::fdo::Result<ObjectPath<'static>> {
        match self.call(DbOp::GetUnit1(name.clone())) {
            Ok(DbReply::Unit1(Some(r))) => ObjectPath::try_from(r.object_path)
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string())),
            Ok(DbReply::Unit1(None)) => {
                Err(zbus::fdo::Error::Failed(format!("No such unit '{name}'")))
            }
            Ok(_) => Err(zbus::fdo::Error::Failed("unexpected reply".into())),
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }

    /// Load a unit file from disk and return the unit's object path.
    fn load_unit(&self, name: String) -> zbus::fdo::Result<ObjectPath<'static>> {
        match self.call(DbOp::LoadUnit(name)) {
            Ok(DbReply::UnitPath(p)) => {
                ObjectPath::try_from(p).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
            }
            Ok(DbReply::Error(e)) => Err(zbus::fdo::Error::Failed(e)),
            Ok(_) => Err(zbus::fdo::Error::Failed("unexpected reply".into())),
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }

    /// List queued jobs. Returns the systemd1 `JobInfo` struct array:
    /// `(id, unit, job_type, state, unit_path, job_path)`.
    #[allow(clippy::type_complexity)]
    fn list_jobs(
        &self,
    ) -> zbus::fdo::Result<
        Vec<(
            u32,
            String,
            String,
            String,
            ObjectPath<'static>,
            ObjectPath<'static>,
        )>,
    > {
        match self.call(DbOp::ListJobs) {
            Ok(DbReply::JobList(jobs)) => Ok(jobs
                .into_iter()
                .map(|j| {
                    (
                        j.id,
                        j.unit,
                        j.job_type,
                        j.state,
                        ObjectPath::try_from(j.unit_path).unwrap(),
                        ObjectPath::try_from(j.job_path).unwrap(),
                    )
                })
                .collect()),
            Ok(_) => Err(zbus::fdo::Error::Failed("unexpected reply".into())),
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }

    /// Return the processes of one unit: `(cgroup, pid, cmdline)` tuples.
    fn get_unit_processes(&self, name: String) -> zbus::fdo::Result<Vec<(String, u32, String)>> {
        match self.call(DbOp::GetUnitProcesses(name)) {
            Ok(DbReply::ProcessList(procs)) => Ok(procs
                .into_iter()
                .map(|p| (p.cgroup, p.pid, p.cmdline))
                .collect()),
            Ok(_) => Err(zbus::fdo::Error::Failed("unexpected reply".into())),
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }
}

/// A single unit's `org.freedesktop.systemd1.Unit` property interface, served
/// at `/org/freedesktop/systemd1/unit/<escaped-name>`.
///
/// Property values are read from shared [`Arc`], [`Mutex`] state that the
/// monitor thread updates from each manager snapshot, so they always reflect
/// the latest manager state rather than a value frozen at registration time.
/// The struct is `Send + Sync` (its only field is `Arc<Mutex<UnitEntry>>`),
/// as zbus may move the interface across its executor threads.
struct UnitIface {
    state: Arc<Mutex<UnitEntry>>,
}

/// Like `Mutex::lock`, but recovers the inner value when the lock is
/// poisoned. Used on every D-Bus property getter so a panic elsewhere in the
/// bridge does not abort the manager under `panic = "abort"`.
fn lock_recover<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[interface(name = "org.freedesktop.systemd1.Unit")]
impl UnitIface {
    /// The name of this unit (e.g. `foo.service`).
    #[zbus(property)]
    fn id(&self) -> String {
        lock_recover(&self.state).name.clone()
    }

    /// The human-readable description of this unit.
    #[zbus(property)]
    fn description(&self) -> String {
        lock_recover(&self.state).description.clone()
    }

    /// The load state (e.g. `loaded`).
    #[zbus(property)]
    fn load_state(&self) -> String {
        lock_recover(&self.state).load.clone()
    }

    /// The active state (e.g. `active`).
    #[zbus(property)]
    fn active_state(&self) -> String {
        lock_recover(&self.state).active.clone()
    }

    /// The sub state (e.g. `running`).
    #[zbus(property)]
    fn sub_state(&self) -> String {
        lock_recover(&self.state).sub.clone()
    }

    /// The unit this unit follows, if any. Empty for now.
    #[zbus(property)]
    fn following(&self) -> String {
        String::new()
    }
}

/// Does the bus currently have an owner for `name`? `None` on lookup error.
fn name_has_owner(proxy: &zbus::blocking::fdo::DBusProxy<'_>, name: &str) -> Option<bool> {
    let bus_name = zbus::names::BusName::try_from(name).ok()?;
    proxy.name_has_owner(bus_name).ok()
}

/// The monitor thread body: connect, serve the interface, request the name,
/// then poll name ownership and forward transitions to the manager. When the
/// `org.freedesktop.systemd1` name is free it is also owned and the
/// systemd1-compatible surface (manager + per-unit `Unit` objects) is served.
fn run_thread(
    user: bool,
    event_tx: Sender<DbEvent>,
    cmd_tx: Sender<DbRequest>,
    watch_rx: Receiver<DbWatch>,
    unit_rx: Receiver<DbUnitEvent>,
    manager_uid: u32,
) {
    let bus = if user { "session" } else { "system" };
    let conn = match if user {
        zbus::blocking::Connection::session()
    } else {
        zbus::blocking::Connection::system()
    } {
        Ok(c) => c,
        Err(e) => {
            mgr_log(&format!("D-Bus: failed to connect to {bus} bus: {e}"));
            return;
        }
    };

    if let Err(e) = conn.object_server().at(
        OBJECT_PATH,
        ManagerIface {
            cmd_tx: cmd_tx.clone(),
            manager_uid,
        },
    ) {
        mgr_log(&format!("D-Bus: failed to register manager interface: {e}"));
        return;
    }
    if let Err(e) = conn.request_name(BUS_NAME) {
        mgr_log(&format!("D-Bus: failed to request name {BUS_NAME}: {e}"));
        return;
    }
    mgr_log(&format!(
        "D-Bus: manager interface live at {BUS_NAME} ({bus} bus)"
    ));

    // Runtime gate for the systemd1-compatible surface. Requesting the name
    // with `DoNotQueue` means the bus never queues us: if real systemd (or
    // any other owner) already holds `org.freedesktop.systemd1` the reply is
    // `Exists`, we log that the surface is disabled, and we keep running with
    // only the native interface — this is not an error.
    let mut systemd1_ready = false;
    match conn.request_name_with_flags(SYSTEMD1_BUS_NAME, RequestNameFlags::DoNotQueue.into()) {
        Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => {
            match conn.object_server().at(
                SYSTEMD1_OBJECT_PATH,
                Systemd1ManagerIface {
                    cmd_tx: cmd_tx.clone(),
                },
            ) {
                Ok(_) => {
                    mgr_log(&format!(
                        "D-Bus: systemd1-compatible surface live at {SYSTEMD1_BUS_NAME} ({bus} bus)"
                    ));
                    systemd1_ready = true;
                }
                Err(e) => {
                    mgr_log(&format!(
                        "D-Bus: failed to register {SYSTEMD1_BUS_NAME} manager interface: {e}"
                    ));
                }
            }
        }
        Ok(reply) => {
            mgr_log(&format!(
                "systemd1 disabled: {SYSTEMD1_BUS_NAME} is already owned (real systemd present?); request returned {reply}"
            ));
        }
        Err(e) => {
            mgr_log(&format!(
                "systemd1 disabled: failed to request {SYSTEMD1_BUS_NAME}: {e}"
            ));
        }
    }

    let proxy = match zbus::blocking::fdo::DBusProxy::new(&conn) {
        Ok(p) => p,
        Err(e) => {
            mgr_log(&format!("D-Bus: failed to create DBus proxy: {e}"));
            return;
        }
    };

    // name -> whether it was owned on the previous poll tick.
    let mut watched: HashMap<String, bool> = HashMap::new();
    // Units currently registered on the systemd1 surface: unit name -> the
    // shared state backing that object's `org.freedesktop.systemd1.Unit`
    // properties (so the objects must outlive registration, which the map
    // guarantees).
    let mut units: HashMap<String, Arc<Mutex<UnitEntry>>> = HashMap::new();

    loop {
        // Drain name-watch add/remove requests from the manager.
        while let Ok(w) = watch_rx.try_recv() {
            match w {
                DbWatch::Add(name) => {
                    // Check immediately to close the race where the service
                    // acquires the name before we register the watch.
                    if let Some(now) = name_has_owner(&proxy, &name) {
                        if now {
                            let _ = event_tx.send(DbEvent::NameAcquired(name.clone()));
                        }
                        watched.insert(name, now);
                    }
                }
                DbWatch::Remove(name) => {
                    watched.remove(&name);
                }
            }
        }

        // Coalesce the manager's unit snapshots: keep only the most recent.
        let mut snapshot: Option<Vec<UnitEntry>> = None;
        while let Ok(DbUnitEvent::Snapshot(s)) = unit_rx.try_recv() {
            snapshot = Some(s);
        }
        if systemd1_ready && let Some(snap) = snapshot {
            apply_unit_snapshot(&conn, &mut units, snap);
        }

        // Poll ownership transitions (false -> true).
        let mut acquired: Vec<String> = Vec::new();
        for (name, was_owned) in watched.iter_mut() {
            if let Some(now) = name_has_owner(&proxy, name) {
                if now && !*was_owned {
                    acquired.push(name.clone());
                }
                *was_owned = now;
            }
        }
        for name in acquired {
            let _ = event_tx.send(DbEvent::NameAcquired(name));
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Diff a fresh manager snapshot against the currently registered systemd1
/// unit objects: register new units (and emit `UnitNew`), update the shared
/// state of changed ones (no re-registration needed, since property getters
/// read the shared state), and deregister + remove units no longer present
/// (emitting `UnitRemoved`).
fn apply_unit_snapshot(
    conn: &zbus::blocking::Connection,
    units: &mut HashMap<String, Arc<Mutex<UnitEntry>>>,
    snapshot: Vec<UnitEntry>,
) {
    let mut seen: HashSet<String> = HashSet::with_capacity(snapshot.len());
    for entry in snapshot {
        let name = entry.name.clone();
        let path = unit_dbus_path(&name);
        if let Some(shared) = units.get(&name) {
            // Changed and/or unchanged: refresh the shared property state.
            *lock_recover(shared) = entry;
        } else {
            let shared = Arc::new(Mutex::new(entry));
            if let Err(e) = conn.object_server().at(
                path.as_str(),
                UnitIface {
                    state: shared.clone(),
                },
            ) {
                mgr_log(&format!(
                    "D-Bus: failed to register systemd1 unit {name} at {path}: {e}"
                ));
                continue;
            }
            emit_unit_signal(conn, "UnitNew", &name, &path);
            units.insert(name.clone(), shared);
        }
        seen.insert(name);
    }

    // Deregister units that vanished from the snapshot.
    for (name, _) in units.clone() {
        if seen.contains(&name) {
            continue;
        }
        let path = unit_dbus_path(&name);
        match conn.object_server().remove::<UnitIface, _>(path.as_str()) {
            Ok(_) => emit_unit_signal(conn, "UnitRemoved", &name, &path),
            Err(e) => mgr_log(&format!(
                "D-Bus: failed to remove systemd1 unit {name} at {path}: {e}"
            )),
        }
        units.remove(&name);
    }
}

/// Emit a `UnitNew`/`UnitRemoved` signal (signature `so`: unit name + object
/// path) from the systemd1 manager object.
fn emit_unit_signal(conn: &zbus::blocking::Connection, signal: &str, name: &str, path: &str) {
    let Ok(obj_path) = ObjectPath::try_from(path) else {
        mgr_log(&format!("D-Bus: invalid unit object path {path}"));
        return;
    };
    if let Err(e) = conn.emit_signal(
        None::<&str>,
        SYSTEMD1_OBJECT_PATH,
        "org.freedesktop.systemd1.Manager",
        signal,
        &(name.to_string(), obj_path),
    ) {
        mgr_log(&format!("D-Bus: failed to emit {signal} for {name}: {e}"));
    }
}
