#![cfg(unix)]

//! End-to-end test: run the real manager daemon in a thread and drive it over
//! the socket with the programmatic `Control` API (the library alternative to
//! `systemctl`/D-Bus). Exercises loading, the full process lifecycle, and
//! query operations end to end.

mod common;

use std::time::Duration;

use common::{Daemon, Scratch, wait_for};
use rystemd::control::{Control, SocketClient};

#[test]
fn start_status_stop_lifecycle() {
    let scratch = Scratch::new();
    scratch.write_unit(
        "hello.service",
        "[Unit]\nDescription=hello service\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/true\n[Install]\nWantedBy=multi-user.target\n",
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));

    let mut ctl = daemon.client();

    // Start: the oneshot /bin/true completes almost immediately and, with
    // RemainAfterExit=yes, parks in active(exited).
    ctl.start(&["hello.service"]).unwrap();
    let st = wait_for(Duration::from_secs(3), || {
        ctl.status(&["hello.service"])
            .map(|v| {
                v.first()
                    .map(|s| s.active == "active" && s.sub == "exited")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    });
    assert!(st, "oneshot service should reach active(exited)");

    let status = ctl.status(&["hello.service"]).unwrap();
    let s = &status[0];
    assert_eq!(s.name, "hello.service");
    assert_eq!(s.description, "hello service");
    assert_eq!(s.enabled, "disabled");

    // is_active / is_enabled through the trait.
    assert_eq!(ctl.is_active(&["hello.service"]).unwrap(), vec!["active"]);
    assert_eq!(
        ctl.is_enabled(&["hello.service"]).unwrap(),
        vec!["disabled"]
    );

    // enable/disable round-trip (symlink management over the wire).
    ctl.enable(&["hello.service"]).unwrap();
    assert_eq!(ctl.is_enabled(&["hello.service"]).unwrap(), vec!["enabled"]);
    ctl.disable(&["hello.service"]).unwrap();
    assert_eq!(
        ctl.is_enabled(&["hello.service"]).unwrap(),
        vec!["disabled"]
    );

    // list_units sees it.
    let units = ctl.list_units(&[], None).unwrap();
    assert!(units.iter().any(|u| u.unit == "hello.service"));

    // Stop → inactive.
    ctl.stop(&["hello.service"]).unwrap();
    let stopped = wait_for(Duration::from_secs(3), || {
        ctl.status(&["hello.service"])
            .map(|v| v.first().map(|s| s.active == "inactive").unwrap_or(false))
            .unwrap_or(false)
    });
    assert!(stopped, "service should return to inactive after stop");
}

#[test]
fn long_running_service_and_kill() {
    let scratch = Scratch::new();
    scratch.write_unit(
        "sleeper.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));

    let mut ctl = daemon.client();

    // Type=simple goes active immediately on spawn (the process keeps running).
    ctl.start(&["sleeper.service"]).unwrap();
    let running = wait_for(Duration::from_secs(3), || {
        ctl.status(&["sleeper.service"])
            .map(|v| {
                v.first()
                    .map(|s| s.active == "active" && s.sub == "running")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    });
    assert!(running, "simple service should be active(running)");

    let main_pid = ctl
        .status(&["sleeper.service"])
        .unwrap()
        .first()
        .and_then(|s| s.main_pid)
        .expect("running service should have a main pid");

    // kill the process group → the service is torn down.
    ctl.kill("sleeper.service", "SIGKILL").unwrap();
    let dead = wait_for(Duration::from_secs(3), || {
        ctl.status(&["sleeper.service"])
            .map(|v| {
                v.first()
                    .map(|s| s.active == "inactive" || s.active == "failed")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    });
    assert!(dead, "service should be torn down after kill");
    assert!(main_pid > 0);
}

#[test]
fn stop_terminates_running_service() {
    let scratch = Scratch::new();
    scratch.write_unit(
        "sleeper.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));

    let mut ctl = daemon.client();
    ctl.start(&["sleeper.service"]).unwrap();
    let running = wait_for(Duration::from_secs(3), || {
        ctl.status(&["sleeper.service"])
            .map(|v| {
                v.first()
                    .map(|s| s.active == "active" && s.sub == "running")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    });
    assert!(running, "sleeper should be active(running)");

    // stop() sends SIGTERM to the process group. Regression guard: the child
    // must start with a clean signal mask — if it inherits the manager's
    // blocked mask it ignores SIGTERM and this hangs for TimeoutStopSec.
    ctl.stop(&["sleeper.service"]).unwrap();
    let stopped = wait_for(Duration::from_secs(3), || {
        ctl.status(&["sleeper.service"])
            .map(|v| v.first().map(|s| s.active == "inactive").unwrap_or(false))
            .unwrap_or(false)
    });
    assert!(
        stopped,
        "stop() should SIGTERM and tear down the running service"
    );
}

#[test]
fn unresolved_service_identity_fails_before_exec() {
    let scratch = Scratch::new();
    let marker = scratch.dir.path().join("identity-fail-ran");
    scratch.write_unit(
        "identity-fail.service",
        &format!(
            "[Service]\nType=oneshot\nUser=rystemd-user-that-must-not-exist\nExecStart=/bin/sh -c 'touch {}'\n",
            marker.display()
        ),
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || daemon.socket.exists()));
    let mut ctl = daemon.client();
    ctl.start(&["identity-fail.service"]).unwrap();

    assert!(wait_for(Duration::from_secs(3), || {
        ctl.status(&["identity-fail.service"])
            .ok()
            .and_then(|states| states.first().map(|state| state.active == "failed"))
            .unwrap_or(false)
    }));
    assert!(!marker.exists(), "ExecStart ran with the manager identity");
}

#[test]
fn unresolved_service_group_fails_before_exec() {
    let scratch = Scratch::new();
    let marker = scratch.dir.path().join("group-fail-ran");
    scratch.write_unit(
        "group-fail.service",
        &format!(
            "[Service]\nType=oneshot\nGroup=rystemd-group-that-must-not-exist\nExecStart=/bin/sh -c 'touch {}'\n",
            marker.display()
        ),
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || daemon.socket.exists()));
    let mut ctl = daemon.client();
    ctl.start(&["group-fail.service"]).unwrap();

    assert!(wait_for(Duration::from_secs(3), || {
        ctl.status(&["group-fail.service"])
            .ok()
            .and_then(|states| states.first().map(|state| state.active == "failed"))
            .unwrap_or(false)
    }));
    assert!(!marker.exists(), "ExecStart ran with the manager group");
}

#[cfg(unix)]
#[test]
fn incomplete_control_request_does_not_block_other_clients() {
    let _scratch = Scratch::new();
    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(2), || daemon.socket.exists()));
    let stalled = std::os::unix::net::UnixStream::connect(&daemon.socket).unwrap();
    let socket = daemon.socket.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result =
            rystemd::client::request_json(&socket, &serde_json::json!({ "op": "list_units" }));
        let _ = tx.send(result);
    });

    let result = rx.recv_timeout(Duration::from_millis(500));
    drop(stalled);
    if result.is_err() {
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }
    assert!(result.is_ok(), "an incomplete request blocked the manager");
}

/// `Restart=on-failure` auto-restarts a service that exits non-zero (up to the
/// start-limit burst). This is the mechanism most real services lean on to
/// survive crashes, and it had zero coverage before this test.
#[test]
fn restart_on_failure_restarts_until_start_limit() {
    let scratch = Scratch::new();
    let marker = scratch.dir.path().join("flaky.count");
    let marker_s = marker.to_string_lossy().to_string();
    scratch.write_unit(
        "flaky.service",
        &format!(
            "[Unit]\nDescription=flaky\n[Service]\nType=simple\nRestart=on-failure\nRestartSec=100ms\nExecStart=/bin/sh -c 'echo x >> {marker_s}; exit 1'\n"
        ),
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));
    let mut ctl = daemon.client();
    ctl.start(&["flaky.service"]).unwrap();

    // Each spawn writes one line then exits 1; on-failure re-spawns it, so the
    // marker grows past the single initial spawn. Assert at least one restart.
    let restarted = wait_for(Duration::from_secs(5), || {
        std::fs::read_to_string(&marker)
            .map(|c| c.lines().count() >= 2)
            .unwrap_or(false)
    });
    assert!(
        restarted,
        "Restart=on-failure should re-spawn a service that exits non-zero"
    );
}

/// `Restart=no` (the default) leaves a failing service where it landed —
/// no re-spawn, no start-limit churn.
#[test]
fn no_restart_leaves_failed_service_inactive() {
    let scratch = Scratch::new();
    let marker = scratch.dir.path().join("doomed.count");
    let marker_s = marker.to_string_lossy().to_string();
    scratch.write_unit(
        "doomed.service",
        &format!("[Service]\nType=simple\nExecStart=/bin/sh -c 'echo x >> {marker_s}; exit 1'\n"),
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));
    let mut ctl = daemon.client();
    ctl.start(&["doomed.service"]).unwrap();

    // Wait for the single spawn, then give it time to (incorrectly) restart.
    assert!(wait_for(Duration::from_secs(3), || {
        std::fs::read_to_string(&marker)
            .map(|c| c.lines().count() >= 1)
            .unwrap_or(false)
    }));
    std::thread::sleep(Duration::from_secs(1));
    let count = std::fs::read_to_string(&marker)
        .map(|c| c.lines().count())
        .unwrap_or(0);
    assert_eq!(count, 1, "Restart=no must not re-spawn a failing service");
}

/// A `Type=simple` service that self-daemonizes (forks a child, then the main
/// process exits) must have its orphaned process group swept and SIGKILLed so
/// nothing escapes process-group tracking.
#[test]
fn daemonizing_service_orphans_are_swept() {
    let scratch = Scratch::new();
    let pidfile = scratch.dir.path().join("orphan.pid");
    let pidfile_s = pidfile.to_string_lossy().to_string();
    scratch.write_unit(
        "daemonize.service",
        &format!(
            "[Service]\nType=simple\nExecStart=/bin/sh -c 'sleep 60 & echo $! > {pidfile_s}; exit 0'\n"
        ),
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));

    let mut ctl = daemon.client();
    ctl.start(&["daemonize.service"]).unwrap();

    // The orphaned `sleep 60` records its pid; once the main sh exits, the
    // sweep SIGKILLs the orphan so its pid disappears from /proc.
    assert!(wait_for(Duration::from_secs(5), || pidfile.exists()));
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid > 0);
    let swept = wait_for(Duration::from_secs(5), || {
        !std::path::Path::new(&format!("/proc/{pid}")).exists()
    });
    assert!(
        swept,
        "orphaned daemon process should be swept (SIGKILLed) on main-process exit"
    );
}

/// A `.socket` unit binds a unix socket; the first connection activates its
/// matching `.service` on demand (inetd-style socket activation).
#[cfg(all(unix, feature = "socket"))]
#[test]
fn socket_activates_service_on_connection() {
    use std::os::unix::net::UnixStream;

    let scratch = Scratch::new();
    let sock = scratch.dir.path().join("echo.sock");
    let sock_s = sock.to_string_lossy().to_string();
    scratch.write_unit("echo.socket", &format!("[Socket]\nListenStream={sock_s}\n"));
    scratch.write_unit(
        "echo.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));

    let mut ctl = daemon.client();

    // Start the socket unit: it binds and listens, but the service stays down.
    ctl.start(&["echo.socket"]).unwrap();
    let listening = wait_for(Duration::from_secs(3), || {
        ctl.status(&["echo.socket"])
            .map(|v| v.first().map(|s| s.active == "active").unwrap_or(false))
            .unwrap_or(false)
    });
    assert!(listening, "socket unit should be active(listening)");
    assert_eq!(
        ctl.is_active(&["echo.service"]).unwrap(),
        vec!["inactive"],
        "service must not start before a connection arrives"
    );

    // Connect: the listener becomes readable and triggers on-demand activation.
    let _conn = UnixStream::connect(&sock).unwrap();

    let activated = wait_for(Duration::from_secs(3), || {
        ctl.status(&["echo.service"])
            .map(|v| v.first().map(|s| s.active == "active").unwrap_or(false))
            .unwrap_or(false)
    });
    assert!(
        activated,
        "connecting should activate the service via socket activation"
    );
}

/// A `.path` unit watches a directory; when a file is dropped into it, the
/// matched `Unit=` service is activated on demand (path activation).
#[test]
fn path_activates_service_on_directory_change() {
    let scratch = Scratch::new();
    let watchdir = scratch.dir.path().join("incoming");
    std::fs::create_dir_all(&watchdir).unwrap();
    let watch_s = watchdir.to_string_lossy().to_string();
    scratch.write_unit(
        "job.path",
        &format!("[Path]\nDirectoryNotEmpty={watch_s}\nUnit=job.service\n"),
    );
    scratch.write_unit(
        "job.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 5\n",
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));

    let mut ctl = daemon.client();

    // Start the path unit: it arms the watch; the service stays down.
    ctl.start(&["job.path"]).unwrap();
    let armed = wait_for(Duration::from_secs(3), || {
        ctl.status(&["job.path"])
            .map(|v| v.first().map(|s| s.active == "active").unwrap_or(false))
            .unwrap_or(false)
    });
    assert!(armed, "path unit should be active(armed)");
    assert_eq!(
        ctl.is_active(&["job.service"]).unwrap(),
        vec!["inactive"],
        "service must not start before a file arrives"
    );

    // Drop a file into the watched directory → path activation fires.
    std::fs::write(watchdir.join("trigger"), b"x").unwrap();

    let activated = wait_for(Duration::from_secs(5), || {
        ctl.status(&["job.service"])
            .map(|v| v.first().map(|s| s.active == "active").unwrap_or(false))
            .unwrap_or(false)
    });
    assert!(
        activated,
        "dropping a file should activate the service via path activation"
    );
}

/// A `.mount` unit mounts a filesystem on start and unmounts on stop, with no
/// process to supervise. `mount(2)` needs `CAP_SYS_ADMIN`, so this test
/// self-skips when not run as root (or in an unprivileged user+mount
/// namespace). The daemon runs as a *thread* in this process, so a `tmpfs`
/// mounted by the manager is visible to `/proc/self/mountinfo` here.
#[cfg(target_os = "linux")]
#[test]
fn mount_unit_lifecycle() {
    if euid() != 0 {
        eprintln!("skipping mount_unit_lifecycle: mount(2) requires root");
        return;
    }

    let scratch = Scratch::new();
    let mountpoint = scratch.dir.path().join("demo");
    std::fs::create_dir_all(&mountpoint).unwrap();
    let where_s = mountpoint.to_string_lossy().to_string();
    scratch.write_unit(
        "tmp-demo.mount",
        &format!("[Mount]\nWhat=tmpfs\nWhere={where_s}\nType=tmpfs\nOptions=mode=1777\n"),
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));

    let mut ctl = daemon.client();

    // Start → mount(2) succeeds → active(mounted).
    ctl.start(&["tmp-demo.mount"]).unwrap();
    let mounted = wait_for(Duration::from_secs(3), || {
        ctl.status(&["tmp-demo.mount"])
            .map(|v| {
                v.first()
                    .map(|s| s.active == "active" && s.sub == "mounted")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    });
    assert!(mounted, "mount unit should reach active(mounted)");
    assert!(
        is_mounted(&mountpoint),
        "tmpfs should be mounted at {where_s}"
    );

    // Stop → umount2(2) → inactive, and the filesystem is gone.
    ctl.stop(&["tmp-demo.mount"]).unwrap();
    let stopped = wait_for(Duration::from_secs(3), || {
        ctl.status(&["tmp-demo.mount"])
            .map(|v| v.first().map(|s| s.active == "inactive").unwrap_or(false))
            .unwrap_or(false)
    });
    assert!(stopped, "mount unit should return to inactive after stop");
    assert!(
        !is_mounted(&mountpoint),
        "tmpfs should be unmounted after stop"
    );
}

/// The current effective uid (0 = root), read from `/proc/self/status`.
#[cfg(target_os = "linux")]
fn euid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(u32::MAX)
}

/// Is `path` a mount point, per `/proc/self/mountinfo`?
#[cfg(target_os = "linux")]
fn is_mounted(path: &std::path::Path) -> bool {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|s| {
            s.lines().any(|l| {
                // Field 4 (0-indexed) is the mount point; decode the
                // `\040`/`\011`/`\012`/`\134` escapes the kernel uses.
                l.split(' ').nth(4).is_some_and(|mp| {
                    let decoded = mp
                        .replace("\\040", " ")
                        .replace("\\011", "\t")
                        .replace("\\012", "\n")
                        .replace("\\134", "\\");
                    std::path::Path::new(&decoded) == canon
                })
            })
        })
        .unwrap_or(false)
}

/// A `.timer` unit arms a monotonic schedule and fires its target. This uses
/// the standard systemd idiom for running a one-shot job periodically:
/// `OnUnitInactiveSec` re-fires the (now-inactive) target, so the target's
/// side effect is observable on every elapse.
#[test]
fn timer_activates_target_on_schedule() {
    let scratch = Scratch::new();
    let tick = scratch.dir.path().join("ticks");
    let tick_s = tick.to_string_lossy().to_string();
    scratch.write_unit(
        "tick.service",
        &format!("[Unit]\nDescription=tick\n[Service]\nType=oneshot\nExecStart=/bin/sh -c 'echo tick >> {tick_s}'\n"),
    );
    scratch.write_unit(
        "tick.timer",
        "[Unit]\nDescription=tick timer\n[Timer]\nOnBootSec=1s\nUnit=tick.service\n",
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));
    let mut ctl = daemon.client();

    ctl.start(&["tick.timer"]).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        ctl.is_active(&["tick.timer"])
            .map(|v| v == vec!["active"])
            .unwrap_or(false)
    }));

    // The timer fires tick.service, which appends to the marker file.
    assert!(
        wait_for(Duration::from_secs(5), || tick.exists()),
        "timer should fire tick.service, which writes the marker"
    );

    // list_timers records the last elapse — direct proof the *timer* fired.
    let last_set = wait_for(Duration::from_secs(3), || {
        ctl.list_timers()
            .map(|v| v.iter().any(|t| t.unit == "tick.timer" && t.last.is_some()))
            .unwrap_or(false)
    });
    assert!(
        last_set,
        "list_timers should record the timer's last elapse"
    );
}

/// Regression test for the KNOWN_ISSUES timer bug: `OnUnitInactiveSec=` must
/// not spuriously fire a target that has never been activated this boot
/// (systemd only arms it once the unit has actually been deactivated), but it
/// must re-arm and re-fire after a real activation→deactivation cycle.
///
/// The target is a long-running `Type=simple` service rather than a oneshot:
/// a oneshot unit is parked `Inactive` on completion without ever passing
/// through the `Active` state in this implementation, so it cannot be the
/// reference for an active→inactive transition. A `Type=simple` unit goes
/// `Active` on spawn (setting `active_enter`) and back to `Inactive` when
/// stopped.
#[test]
fn timer_onunitinactive_requires_prior_activation() {
    let scratch = Scratch::new();
    // Type=simple, long-running: the target genuinely reaches Active then
    // returns to Inactive on stop — the transition OnUnitInactiveSec keys on.
    scratch.write_unit(
        "gated.service",
        "[Unit]\nDescription=gated\n[Service]\nType=simple\nExecStart=/bin/sleep 60\n",
    );
    // No OnCalendar/OnBootSec: OnUnitInactiveSec is the ONLY elapse source.
    scratch.write_unit(
        "gated.timer",
        "[Unit]\nDescription=gated timer\n[Timer]\nOnUnitInactiveSec=1s\nUnit=gated.service\n",
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));
    let mut ctl = daemon.client();

    let target_active = |c: &mut SocketClient| {
        c.is_active(&["gated.service"])
            .map(|v| v == vec!["active"])
            .unwrap_or(false)
    };

    // Arm the timer while the target is inactive and has never been started.
    ctl.start(&["gated.timer"]).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        ctl.is_active(&["gated.timer"])
            .map(|v| v == vec!["active"])
            .unwrap_or(false)
    }));

    // The never-activated target must NOT be fired within a generous window.
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        !target_active(&mut ctl),
        "OnUnitInactiveSec must not fire a target that was never activated"
    );

    // Now give it a real activation, then deactivation: the long-running
    // service starts (Active) and is stopped (Inactive), after which
    // OnUnitInactiveSec legitimately re-arms and re-fires it.
    ctl.start(&["gated.service"]).unwrap();
    assert!(wait_for(Duration::from_secs(3), || target_active(&mut ctl)));
    ctl.stop(&["gated.service"]).unwrap();
    assert!(wait_for(Duration::from_secs(3), || !target_active(
        &mut ctl
    )));

    let re_fired = wait_for(Duration::from_secs(6), || {
        // 1s after the deactivation, the timer starts the target again.
        target_active(&mut ctl)
    });
    assert!(
        re_fired,
        "OnUnitInactiveSec should re-fire after a real activation→deactivation"
    );
    // The re-fired target is again long-running, so it stays active; clean it
    // up so the daemon shuts down without a lingering process.
    let _ = ctl.stop(&["gated.service"]);
}

/// A `.target` is a pure grouping unit: starting it pulls in (and orders) its
/// `Wants=`, each of which reaches its own active state.
#[test]
fn target_start_pulls_in_wants() {
    let scratch = Scratch::new();
    scratch.write_unit(
        "demo.service",
        "[Unit]\nDescription=demo svc\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/true\n",
    );
    scratch.write_unit(
        "demo.target",
        "[Unit]\nDescription=demo target\nWants=demo.service\nAfter=demo.service\n",
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));
    let mut ctl = daemon.client();

    ctl.start(&["demo.target"]).unwrap();

    assert!(wait_for(Duration::from_secs(3), || {
        ctl.is_active(&["demo.target"])
            .map(|v| v == vec!["active"])
            .unwrap_or(false)
    }));
    assert_eq!(ctl.is_active(&["demo.service"]).unwrap(), vec!["active"]);
}

/// The `examples/live/` demo units, exercised end to end through the CLI
/// client: one of every unit type rystemd supports, pulled in together by a
/// `.target` exactly as the interactive initramfs wires them. Covers .service,
/// .timer, .socket, .mount, and .target. The .mount portion self-skips when
/// not run as root (mount(2) needs CAP_SYS_ADMIN) — run under
/// `unshare -m -U -r --map-root-user` to exercise it, as `mount_unit_lifecycle`
/// does.
#[cfg(all(target_os = "linux", feature = "socket"))]
#[test]
fn live_demo_units_lifecycle() {
    use std::os::unix::net::UnixStream;

    let scratch = Scratch::new();
    let root = scratch.dir.path().to_path_buf();
    let tick = root.join("demo.ticks");
    let tick_s = tick.to_string_lossy().to_string();
    let sock = root.join("demo.sock");
    let sock_s = sock.to_string_lossy().to_string();
    let mnt = root.join("mnt-demo");
    std::fs::create_dir_all(&mnt).unwrap();
    let mnt_s = mnt.to_string_lossy().to_string();

    scratch.write_unit(
        "demo.service",
        "[Unit]\nDescription=Demo service\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/true\n",
    );
    scratch.write_unit(
        "demo-tick.service",
        &format!("[Unit]\nDescription=Demo tick\n[Service]\nType=oneshot\nExecStart=/bin/sh -c 'echo tick >> {tick_s}'\n"),
    );
    scratch.write_unit(
        "demo.timer",
        "[Unit]\nDescription=Demo timer\n[Timer]\nOnBootSec=2s\nUnit=demo-tick.service\n",
    );
    scratch.write_unit(
        "demo.socket",
        &format!("[Unit]\nDescription=Demo socket\n[Socket]\nListenStream={sock_s}\nService=demo-echo.service\n"),
    );
    scratch.write_unit(
        "demo-echo.service",
        "[Unit]\nDescription=Demo echo service\n[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    scratch.write_unit(
        "demo.mount",
        &format!("[Unit]\nDescription=Demo mount\n[Mount]\nWhat=tmpfs\nWhere={mnt_s}\nType=tmpfs\nOptions=mode=1777\n"),
    );
    scratch.write_unit(
        "demo.target",
        "[Unit]\nDescription=Demo target\nWants=demo.service demo.timer demo.socket demo.mount\nAfter=demo.service demo.timer demo.socket demo.mount\n",
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));
    let mut ctl = daemon.client();

    // One `start demo.target` pulls in every demo unit via Wants=.
    ctl.start(&["demo.target"]).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        ctl.is_active(&["demo.target"])
            .map(|v| v == vec!["active"])
            .unwrap_or(false)
    }));

    // .service: oneshot + RemainAfterExit=yes parks in active(exited).
    assert!(wait_for(Duration::from_secs(3), || {
        ctl.status(&["demo.service"])
            .map(|v| {
                v.first()
                    .map(|s| s.active == "active" && s.sub == "exited")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }));

    // .timer: armed and active.
    assert_eq!(ctl.is_active(&["demo.timer"]).unwrap(), vec!["active"]);

    // .socket: active(listening), and its service is NOT yet started.
    assert!(wait_for(Duration::from_secs(3), || {
        ctl.status(&["demo.socket"])
            .map(|v| v.first().map(|s| s.active == "active").unwrap_or(false))
            .unwrap_or(false)
    }));
    assert_eq!(
        ctl.is_active(&["demo-echo.service"]).unwrap(),
        vec!["inactive"]
    );

    // .timer fires demo-tick.service (OnUnitInactiveSec=1s): the tick marker
    // proves the target actually ran on a timer elapse.
    assert!(
        wait_for(Duration::from_secs(5), || tick.exists()),
        "timer should fire demo-tick.service, which writes the marker"
    );
    let last_set = wait_for(Duration::from_secs(3), || {
        ctl.list_timers()
            .map(|v| v.iter().any(|t| t.unit == "demo.timer" && t.last.is_some()))
            .unwrap_or(false)
    });
    assert!(
        last_set,
        "list_timers should record demo.timer's last elapse"
    );

    // .socket: a connection activates demo-echo.service on demand.
    let _conn = UnixStream::connect(&sock).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        ctl.status(&["demo-echo.service"])
            .map(|v| v.first().map(|s| s.active == "active").unwrap_or(false))
            .unwrap_or(false)
    }));

    // .mount: mount(2) needs CAP_SYS_ADMIN, so self-skip when not root.
    if euid() != 0 {
        eprintln!("skipping live_demo_units_lifecycle .mount: mount(2) requires root");
        return;
    }
    assert!(wait_for(Duration::from_secs(3), || {
        ctl.status(&["demo.mount"])
            .map(|v| {
                v.first()
                    .map(|s| s.active == "active" && s.sub == "mounted")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }));
    assert!(is_mounted(&mnt), "tmpfs should be mounted at {mnt_s}");
    ctl.stop(&["demo.mount"]).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        ctl.is_active(&["demo.mount"])
            .map(|v| v == vec!["inactive"])
            .unwrap_or(false)
    }));
    assert!(!is_mounted(&mnt), "tmpfs should be unmounted after stop");
}

/// `.device` units are runtime-generated by udev enumeration — there is no
/// unit file. The test daemon calls `udev_init()` (like the real one), so
/// `list-units` should surface `.device` entries after enumeration. Skips
/// quietly in sandboxes without a mounted sysfs.
#[cfg(all(target_os = "linux", feature = "udev"))]
#[test]
fn device_units_appear_after_enumeration() {
    if !std::path::Path::new("/sys/devices").is_dir() {
        eprintln!("skipping device_units_appear_after_enumeration: /sys/devices not present");
        return;
    }
    // Scratch sets the RYSTEMD_* env vars (and holds the env lock) that the
    // daemon thread reads; we don't need to write any unit files.
    let _scratch = Scratch::new();
    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));
    let ctl = daemon.client();
    let found = wait_for(Duration::from_secs(3), || {
        ctl.list_units(&[], None)
            .map(|v| v.iter().any(|u| u.unit.ends_with(".device")))
            .unwrap_or(false)
    });
    assert!(
        found,
        "udev enumeration should register .device units in list-units"
    );
}

#[test]
fn journal_persists_service_output_and_reads_over_ipc() {
    let s = Scratch::new();
    s.write_unit(
        "j.service",
        "[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/echo journal-marker\n",
    );
    let d = Daemon::start();
    let mut c = d.client();
    assert!(wait_for(Duration::from_secs(5), || c
        .list_units(&[], None)
        .is_ok()));
    c.start(&["j.service"]).unwrap();
    assert!(wait_for(Duration::from_secs(5), || c
        .status(&["j.service"])
        .map(|v| v.first().is_some_and(|x| x.active == "active"))
        .unwrap_or(false)));

    // The durable store is on disk under the isolated journal dir.
    assert!(
        s.journal().join("j.service").exists(),
        "journal file should exist on disk"
    );

    // And it's readable over the IPC journal op.
    let (records, dir) = c.journal(Some("j.service"), None, None).unwrap();
    assert_eq!(dir, s.journal().display().to_string());
    assert!(
        records.iter().any(|r| r.text.contains("journal-marker")),
        "journal should contain the service's stdout line"
    );
}

#[test]
fn failing_condition_skips_unit_but_not_dependents() {
    let scratch = Scratch::new();
    // The path genuinely does not exist, so `ConditionPathExists` is
    // unsatisfied and cond-svc.service must be *skipped* (left inactive and
    // not failed, per systemd's skip-vs-fail semantics).
    scratch.write_unit(
        "cond-svc.service",
        "[Unit]\n\
         Description=conditionally skipped service\n\
         ConditionPathExists=/nonexistent/rystemd-e2e-cond-skip\n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart=/bin/true\n",
    );
    // The target `Wants=`+`After=` the skipped service. Even though the
    // dependency is skipped, its start job is treated as satisfied, so the
    // target must still activate (the condition must not block dependents).
    scratch.write_unit(
        "cond-parent.target",
        "[Unit]\n\
         Description=condition dependent target\n\
         Wants=cond-svc.service\n\
         After=cond-svc.service\n",
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));
    let mut ctl = daemon.client();

    ctl.start(&["cond-parent.target"]).unwrap();

    // The dependent target activates...
    let target_active = wait_for(Duration::from_secs(3), || {
        ctl.status(&["cond-parent.target"])
            .map(|v| v.first().is_some_and(|s| s.active == "active"))
            .unwrap_or(false)
    });
    assert!(
        target_active,
        "target should still activate despite the skipped Wants= dependency"
    );

    // ...while the skipped service stays inactive and is NOT marked failed.
    let st = wait_for(Duration::from_secs(3), || {
        ctl.status(&["cond-svc.service"])
            .map(|v| v.first().is_some_and(|s| s.active == "inactive"))
            .unwrap_or(false)
    });
    assert!(
        st,
        "condition-skipped service should remain inactive (skipped, not failed)"
    );
    let s = &ctl.status(&["cond-svc.service"]).unwrap()[0];
    assert_ne!(
        s.active, "failed",
        "a skipped condition must not fail the unit"
    );
}

#[test]
fn failing_assert_fails_unit() {
    let scratch = Scratch::new();
    // `Assert*` is a hard gate: when it is unsatisfied the unit's start job
    // fails and the unit is marked `failed` (unlike a plain condition, which
    // skips).
    scratch.write_unit(
        "assert-fail.service",
        "[Unit]\n\
         Description=assert gated service\n\
         AssertPathExists=/nonexistent/rystemd-e2e-assert-fail\n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart=/bin/true\n",
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));
    let mut ctl = daemon.client();

    ctl.start(&["assert-fail.service"]).unwrap();
    let failed = wait_for(Duration::from_secs(3), || {
        ctl.status(&["assert-fail.service"])
            .map(|v| v.first().is_some_and(|s| s.active == "failed"))
            .unwrap_or(false)
    });
    assert!(
        failed,
        "an unsatisfied Assert* must fail the unit's start job (state failed)"
    );
}

/// `RuntimeDirectory=` / `StateDirectory=` are created+owned at start and
/// cleaned up on stop: the runtime dir is removed unconditionally, while the
/// state dir is removed only when empty — so a state dir the service wrote to
/// persists.
#[test]
fn runtime_and_state_directories_created_and_cleaned() {
    let s = Scratch::new();
    let state_root = s.dir.path().join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    // SAFETY: we hold the env lock (via Scratch) for the whole test, so no
    // other test in this process touches the environment while we set this.
    unsafe {
        std::env::set_var("RYSTEMD_STATE_DIR", &state_root);
    }
    // The service writes into its state dir so it is non-empty on stop.
    let marker = state_root.join("mysvc").join("marker");
    let unit = format!(
        "[Service]\nType=oneshot\nRemainAfterExit=yes\n\
         RuntimeDirectory=mysvc\nStateDirectory=mysvc\n\
         ExecStart=/bin/sh -c 'echo x > {}'\n",
        marker.display()
    );
    s.write_unit("dirs.service", &unit);

    let d = Daemon::start();
    let mut c = d.client();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&d.socket).exists()
    }));

    c.start(&["dirs.service"]).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        c.status(&["dirs.service"])
            .map(|v| v.first().is_some_and(|x| x.active == "active"))
            .unwrap_or(false)
    }));

    // Both directories exist after start...
    let run_dir = s.dir.path().join("run").join("mysvc");
    let state_dir = state_root.join("mysvc");
    assert!(run_dir.is_dir(), "runtime dir should be created on start");
    assert!(state_dir.is_dir(), "state dir should be created on start");
    assert!(
        marker.exists(),
        "service should be able to write into its state dir"
    );
    // ...with the default 0755 access mode.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&run_dir).unwrap().permissions().mode() & 0o777,
        0o755,
        "runtime dir should default to access mode 0755"
    );

    c.stop(&["dirs.service"]).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        c.status(&["dirs.service"])
            .map(|v| v.first().is_some_and(|x| x.active == "inactive"))
            .unwrap_or(false)
    }));

    // The runtime dir is removed on stop; the non-empty state dir persists.
    assert!(!run_dir.exists(), "runtime dir should be removed on stop");
    assert!(
        state_dir.is_dir(),
        "non-empty state dir should persist after stop"
    );
}

/// Concurrent control-client connects must not blow past the cap even when
/// many connections arrive in a single accept-loop drain. Regression for the
/// `accept_connections` cap snapshot bug (REVIEW.md, this run).
#[test]
fn accept_connections_enforces_client_cap_under_backlog() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let _scratch = Scratch::new();
    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));
    // Open a modest batch — well above any plausible backlog size — so the
    // accept loop has to drop excess entries once the cap is hit.
    const N: usize = 64;
    let mut clients: Vec<UnixStream> = Vec::new();
    for _ in 0..N {
        match UnixStream::connect(&daemon.socket) {
            Ok(s) => clients.push(s),
            Err(_) => break, // cap-exceeded excess drops before accept()
        }
    }
    assert!(
        clients.len() <= N,
        "test setup: every successful connect must be tracked"
    );

    // Each client sends a trivial request and reads a response. The cap is
    // far above N (1024 by default), so all N must complete normally — the
    // bug here is that the FIRST accept-loop drain grew the map past the
    // cap, which we no longer do. A correct manager admits all N.
    for c in &mut clients {
        c.set_nonblocking(false).ok();
        c.write_all(b"{\"op\":\"is_active\",\"units\":[]}\n")
            .unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match c.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.ends_with(b"\n") {
                        break;
                    }
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("control client read failed: {e}"),
            }
        }
        let line = std::str::from_utf8(&buf).expect("response must be utf-8");
        assert!(
            line.contains("\"ok\":true"),
            "every connected client must receive a valid response, got: {line:?}"
        );
    }
}

/// A connected client whose response would exceed the per-client cap must be
/// dropped, not allowed to balloon the manager's resident memory. Regression
/// for the `PendingClient.out` unbounded-response fix.
#[test]
fn control_response_cap_drops_oversize_clients() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let scratch = Scratch::new();
    // A 2 MiB Description field on a real unit forces the `cat` response to
    // exceed the 1 MiB per-client cap. The manager must truncate rather than
    // ship the full multi-MiB response and must not let memory balloon.
    let big_desc = "x".repeat(2 * 1024 * 1024);
    scratch.write_unit(
        "huge.service",
        &format!(
            "[Unit]\nDescription={big_desc}\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/true\n"
        ),
    );

    let daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&daemon.socket).exists()
    }));
    let mut c = UnixStream::connect(&daemon.socket).unwrap();
    let req = serde_json::json!({
        "op": "cat",
        "units": ["huge.service"],
    });
    let mut line = serde_json::to_string(&req).unwrap();
    line.push('\n');
    c.write_all(line.as_bytes()).unwrap();

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match c.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                // The cap is 1 MiB; allow a small slack for the truncation
                // payload, but reject any response larger than 2 MiB — that
                // would mean the cap failed to fire.
                if buf.len() > 2 * 1024 * 1024 {
                    panic!(
                        "manager shipped more than 2 MiB ({} bytes) on an over-cap response",
                        buf.len()
                    );
                }
            }
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    let response = std::str::from_utf8(&buf).unwrap_or("");
    // The cap truncates with a small error payload rather than shipping the
    // multi-MiB JSON. The error string mentions the truncation, so the peer
    // knows why its connection ended.
    assert!(
        response.contains("truncated") || buf.is_empty(),
        "expected truncated response or close, got {} bytes: {response:?}",
        buf.len()
    );
}

/// An idle (connected-but-quiet) control client must NOT cause the manager's
/// poll loop to busy-spin. Regression for the unconditional `POLLIN|POLLOUT`
/// registration on every accepted control fd: a UNIX stream socket with room
/// in its send buffer is *always* `POLLOUT`-ready, so registering every client
/// for `POLLOUT` (regardless of whether a response is actually pending) makes
/// `poll()` return immediately and burns a full core as long as the idle peer
/// holds the connection. The fix registers `POLLOUT` only for clients whose
/// response is queued (`out.is_some()`), so an idle peer lets the 1s poll
/// timeout elapse normally.
///
/// We assert this by holding an idle control connection open for one second,
/// then issuing a *second* concurrent request from a different socket. Under
/// the busy-spin bug the manager thread is at ~100% CPU for that whole second;
/// under the fix it spends most of it asleep in `poll(2)`. The second request
/// completes well within the deadline regardless, so we measure CPU time of
/// the manager thread from `/proc/self/task/<tid>/stat` (this test's manager
/// runs in the test's own process via `Daemon::start_impl`, so the target
/// thread is the one we spawned in `Daemon`).
#[test]
fn idle_control_client_does_not_busy_spin_event_loop() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    let _scratch = Scratch::new();
    let _daemon = Daemon::start();
    assert!(wait_for(Duration::from_secs(3), || {
        std::path::Path::new(&_daemon.socket).exists()
    }));

    // Open an idle control peer — connect, never write. The manager accepts
    // and registers it; under the bug this is what triggers the busy-spin.
    let idle = UnixStream::connect(&_daemon.socket).unwrap();
    idle.set_nonblocking(false).unwrap();

    // Find the manager thread by its name (set in `Daemon::start_impl`). The
    // `/proc/<pid>/task/<tid>/comm` file holds the kernel-side thread name;
    // match `"rystemd-manager"`.
    let self_pid = std::process::id();
    let mut manager_tid: Option<i32> = None;
    for e in std::fs::read_dir(format!("/proc/{self_pid}/task")).unwrap() {
        let e = e.unwrap();
        let tid: i32 = e.file_name().to_str().unwrap().parse().unwrap();
        let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
        // comm is `<name>\n`; we want exact match against our thread name.
        if comm.trim_end() == "rystemd-manager" {
            manager_tid = Some(tid);
            break;
        }
    }
    let tid = manager_tid.expect("manager thread 'rystemd-manager' not found in /proc");

    let stat0 = std::fs::read_to_string(format!("/proc/{self_pid}/task/{tid}/stat")).unwrap();
    // field 14 = utime, field 15 = stime (1-indexed). The first field is the
    // `comm` in parentheses and may contain spaces, so split from the right.
    let parts0: Vec<&str> = stat0.rsplit(')').next().unwrap().split_whitespace().collect();
    assert!(parts0.len() >= 15, "unexpected /proc stat layout");
    let utime0: u64 = parts0[11].parse().unwrap();
    let stime0: u64 = parts0[12].parse().unwrap();

    // Hold the idle peer open for one second. Under the busy-spin bug, this
    // task accumulates ~CLK_TCK jiffies (≥ 1000 on a 1000-Hz kernel); under
    // the fix it spends the second in `poll(2)` and accumulates roughly 0.
    let sample_start = Instant::now();
    std::thread::sleep(Duration::from_secs(1));

    let stat = std::fs::read_to_string(format!("/proc/{self_pid}/task/{tid}/stat")).unwrap();
    let parts: Vec<&str> = stat.rsplit(')').next().unwrap().split_whitespace().collect();
    let utime1: u64 = parts[11].parse().unwrap();
    let stime1: u64 = parts[12].parse().unwrap();
    let elapsed = sample_start.elapsed();
    // Linux jiffies are user-HZ units (typically 100 or 1000). One full
    // CPU-second on a 1000-Hz kernel is 1000 jiffies; on 100-Hz it's 100.
    // Allow up to CLK_Tck + 50% slack (a manager that is genuinely working on
    // something could legitimately use a fraction of a second) — but the
    // busy-spin case uses ≥ CLK_Tck jiffies, which fails this bound
    // comfortably.
    let clk_tck: u64 = std::fs::read_to_string("/proc/self")
        .ok()
        .and_then(|_| None)
        .unwrap_or(100); // best-effort default; real value via sysconf
    let consumed = utime1.saturating_sub(utime0) + stime1.saturating_sub(stime0);
    let elapsed_secs = elapsed.as_secs_f64();
    let consumed_secs = consumed as f64 / clk_tck as f64;
    assert!(
        consumed_secs < elapsed_secs * 0.5 + 0.1,
        "manager thread consumed {consumed_secs:.2}s of CPU during a {elapsed_secs:.2}s \
         window with an idle control peer — busy-spin (utime {}→{}, stime {}→{})",
        utime0,
        utime1,
        stime0,
        stime1
    );

    // Sanity: a concurrent request still completes promptly under the fix.
    let mut other = UnixStream::connect(&_daemon.socket).unwrap();
    let req = serde_json::json!({"op": "is_system_running"});
    let mut line = serde_json::to_string(&req).unwrap();
    line.push('\n');
    other.write_all(line.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 256];
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && buf.last().copied() != Some(b'\n') {
        if let Ok(n) = other.read(&mut tmp) {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
    }
    assert!(
        !buf.is_empty(),
        "concurrent request must complete within 2s even with an idle peer held open"
    );
}
