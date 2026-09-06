//! Shared helpers for integration tests: run the real manager against a
//! scratch filesystem by pointing the `RYSTEMD_*` env hooks at a temp dir.

// Each test binary (e2e, dbus, privileged) uses a different subset of these
// helpers, so an item that is reachable in one binary is dead code in another.
// Silence dead_code here rather than per-item.
#![allow(dead_code)]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use rystemd::control::SocketClient;

/// Serializes env-var mutation across tests within this binary (tests run in
/// parallel threads, and `RYSTEMD_*` are process-global).
static ENV_LOCK: Mutex<()> = Mutex::new(());

pub struct Scratch {
    _lock: std::sync::MutexGuard<'static, ()>,
    /// The temp dir (kept alive for the lifetime of this guard).
    pub dir: tempfile::TempDir,
}

impl Scratch {
    pub fn new() -> Scratch {
        // Poison-tolerant: a test that panics while holding the lock must not
        // cascade into every subsequent test in the binary.
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let units = root.join("units");
        let config = root.join("config");
        let run = root.join("run");
        let journal = root.join("journal");
        for d in [&units, &config, &run, &journal] {
            std::fs::create_dir_all(d).unwrap();
        }
        // SAFETY: guarded by ENV_LOCK, so no other test in this process is
        // reading these vars while we set them.
        unsafe {
            std::env::set_var("RYSTEMD_UNIT_PATH", &units);
            std::env::set_var("RYSTEMD_CONFIG_DIR", &config);
            std::env::set_var("RYSTEMD_RUNTIME_DIR", &run);
            std::env::set_var("RYSTEMD_SOCKET", run.join("control.sock"));
            std::env::set_var("RYSTEMD_JOURNAL_DIR", &journal);
        }
        Scratch { _lock: lock, dir }
    }

    /// Path to the unit-file directory.
    pub fn units(&self) -> std::path::PathBuf {
        self.dir.path().join("units")
    }

    /// Write a unit file into the scratch unit directory.
    pub fn write_unit(&self, name: &str, body: &str) {
        std::fs::write(self.units().join(name), body).unwrap();
    }

    /// Path to the isolated journal directory.
    pub fn journal(&self) -> std::path::PathBuf {
        self.dir.path().join("journal")
    }
}

/// Poll `f` until it returns true or `timeout` elapses.
pub fn wait_for<F: FnMut() -> bool>(timeout: Duration, mut f: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    f()
}

/// Run the manager daemon in a background thread and shut it down cleanly on
/// drop (via the control socket). Shared by the e2e, D-Bus, and privileged
/// integration tests.
pub struct Daemon {
    handle: Option<std::thread::JoinHandle<()>>,
    pub socket: std::path::PathBuf,
}

impl Daemon {
    /// Start the manager without the D-Bus bridge (the common case — most
    /// tests drive it over IPC).
    pub fn start() -> Daemon {
        Self::start_impl(false)
    }

    /// Start the manager *with* the D-Bus bridge (the dbus integration test).
    pub fn start_with_dbus() -> Daemon {
        Self::start_impl(true)
    }

    fn start_impl(dbus: bool) -> Daemon {
        let socket = std::env::var_os("RYSTEMD_SOCKET").unwrap().into();
        let handle = std::thread::Builder::new()
            .name("rystemd-manager".into())
            .spawn(move || {
                let mut mgr = rystemd::manager::Manager::new(
                    rystemd::manager::ManagerCfg::for_mode(false).unwrap(),
                )
                .unwrap();
                mgr.load_all();
                // Mirror the real daemon (daemon.rs::run_daemon_with_ready):
                // enumerate kernel devices before serving requests.
                #[cfg(all(target_os = "linux", feature = "udev"))]
                mgr.udev_init();
                if dbus {
                    // Best-effort: an absent/unreachable bus is logged and the
                    // manager keeps running without D-Bus.
                    #[cfg(all(target_os = "linux", feature = "dbus"))]
                    mgr.start_dbus().ok();
                }
                mgr.bind_ipc().unwrap();
                mgr.bind_notify().ok();
                mgr.setup_signals();
                mgr.run();
            })
            .unwrap();
        Daemon {
            handle: Some(handle),
            socket,
        }
    }

    pub fn client(&self) -> SocketClient {
        SocketClient::for_mode(false).unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Ask the manager to stop and exit, then reap the thread.
        let _ =
            rystemd::client::request_json(&self.socket, &serde_json::json!({ "op": "shutdown" }));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
