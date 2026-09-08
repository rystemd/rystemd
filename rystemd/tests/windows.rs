#![cfg(windows)]

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rystemd::control::{Control, SocketClient};
use rystemd::manager::{Manager, ManagerCfg};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Scratch {
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    pipe: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let units = dir.path().join("units");
        let config = dir.path().join("config");
        let runtime = dir.path().join("runtime");
        for path in [&units, &config, &runtime] {
            std::fs::create_dir_all(path).unwrap();
        }
        let pipe = PathBuf::from(format!(
            r"\\.\pipe\rystemd-test-{}-{}",
            std::process::id(),
            dir.path().file_name().unwrap().to_string_lossy()
        ));
        unsafe {
            std::env::set_var("RYSTEMD_UNIT_PATH", &units);
            std::env::set_var("RYSTEMD_CONFIG_DIR", &config);
            std::env::set_var("RYSTEMD_RUNTIME_DIR", &runtime);
            std::env::set_var("RYSTEMD_SOCKET", &pipe);
        }
        Self {
            _lock: lock,
            dir,
            pipe,
        }
    }

    fn write_unit(&self, name: &str, body: &str) {
        std::fs::write(self.dir.path().join("units").join(name), body).unwrap();
    }
}

struct Daemon(Option<std::thread::JoinHandle<()>>);

impl Daemon {
    fn start() -> Self {
        Self(Some(std::thread::spawn(|| {
            let mut manager = Manager::new(ManagerCfg::for_mode(true).unwrap()).unwrap();
            manager.load_all();
            manager.bind_ipc().unwrap();
            manager.setup_signals();
            manager.run();
        })))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Ok(client) = rystemd::client::Client::for_mode(true) {
            let _ = client.simple_op("shutdown");
        }
        if let Some(handle) = self.0.take() {
            handle.join().unwrap();
        }
    }
}

fn wait_for(timeout: Duration, mut test: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if test() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    test()
}

fn wait_for_client() -> SocketClient {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match SocketClient::for_mode(true) {
            Ok(client) => return client,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("manager named pipe did not become available: {error}"),
        }
    }
}

#[test]
fn user_manager_runs_oneshot_service_over_named_pipe() {
    let scratch = Scratch::new();
    scratch.write_unit(
        "hello.service",
        "[Unit]\nDescription=Windows hello\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=cmd.exe /D /C exit 0\n",
    );
    let _daemon = Daemon::start();
    let mut client = wait_for_client();
    assert!(wait_for(Duration::from_secs(5), || client
        .list_units(&[], None)
        .is_ok()));

    client.start(&["hello.service"]).unwrap();
    assert!(wait_for(Duration::from_secs(5), || client
        .status(&["hello.service"])
        .is_ok_and(|status| status.first().is_some_and(|s| {
            s.active == "active" && s.sub == "exited"
        }))));
    let status = client.status(&["hello.service"]).unwrap();
    assert_eq!(status[0].description, "Windows hello");
    assert!(scratch.pipe.to_string_lossy().starts_with(r"\\.\pipe\"));
}

#[test]
fn tcp_socket_unit_activates_matching_service() {
    let scratch = Scratch::new();
    let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    scratch.write_unit(
        "echo.socket",
        &format!("[Socket]\nListenStream=127.0.0.1:{port}\n"),
    );
    scratch.write_unit(
        "echo.service",
        "[Service]\nType=simple\nExecStart=cmd.exe /D /C ping -n 30 127.0.0.1 >NUL\n",
    );
    let _daemon = Daemon::start();
    let mut client = wait_for_client();
    assert!(wait_for(Duration::from_secs(5), || client
        .list_units(&[], None)
        .is_ok()));

    client.start(&["echo.socket"]).unwrap();
    assert_eq!(client.is_active(&["echo.service"]).unwrap(), ["inactive"]);
    let _connection = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert!(wait_for(Duration::from_secs(5), || client
        .is_active(&["echo.service"])
        .is_ok_and(|state| state == ["active"])));
}

#[test]
fn user_daemon_subprocess_accepts_named_pipe_control() {
    let scratch = Scratch::new();
    scratch.write_unit(
        "subprocess.service",
        "[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=cmd.exe /D /C exit 0\n",
    );
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rystemd"))
        .args(["daemon", "--user"])
        .env("RYSTEMD_UNIT_PATH", scratch.dir.path().join("units"))
        .env("RYSTEMD_CONFIG_DIR", scratch.dir.path().join("config"))
        .env("RYSTEMD_RUNTIME_DIR", scratch.dir.path().join("runtime"))
        .env("RYSTEMD_SOCKET", &scratch.pipe)
        .spawn()
        .unwrap();
    let mut client = wait_for_client();
    assert!(wait_for(Duration::from_secs(5), || client
        .list_units(&[], None)
        .is_ok()));
    client.start(&["subprocess.service"]).unwrap();
    assert!(wait_for(Duration::from_secs(5), || client
        .is_active(&["subprocess.service"])
        .is_ok_and(|state| state == ["active"])));
    rystemd::client::Client::for_mode(true)
        .unwrap()
        .simple_op("shutdown")
        .unwrap();
    assert!(wait_for(Duration::from_secs(5), || child
        .try_wait()
        .unwrap()
        .is_some()));
}

#[test]
fn unsupported_windows_service_types_fail_loudly() {
    let scratch = Scratch::new();
    scratch.write_unit(
        "forking.service",
        "[Service]\nType=forking\nExecStart=cmd.exe /D /C exit 0\nPIDFile=C:\\\\missing.pid\n",
    );
    let _daemon = Daemon::start();
    let mut client = wait_for_client();
    assert!(wait_for(Duration::from_secs(5), || client
        .list_units(&[], None)
        .is_ok()));
    client.start(&["forking.service"]).unwrap();
    assert!(wait_for(Duration::from_secs(5), || client
        .status(&["forking.service"])
        .is_ok_and(|status| status
            .first()
            .is_some_and(|s| s.active == "failed"))));
    let status = client.status(&["forking.service"]).unwrap();
    assert!(
        status[0]
            .log
            .iter()
            .any(|line| line.contains("not supported on Windows"))
    );
}

#[test]
fn one_tcp_connection_triggers_oneshot_only_once() {
    let scratch = Scratch::new();
    let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let marker = scratch.dir.path().join("socket-hits.txt");
    scratch.write_unit(
        "once.socket",
        &format!("[Socket]\nListenStream=127.0.0.1:{port}\n"),
    );
    scratch.write_unit(
        "once.service",
        &format!(
            "[Service]\nType=oneshot\nExecStart=cmd.exe /D /C echo hit>>{}\n",
            marker.display().to_string().replace('\\', "/")
        ),
    );
    let _daemon = Daemon::start();
    let mut client = wait_for_client();
    assert!(wait_for(Duration::from_secs(5), || client
        .list_units(&[], None)
        .is_ok()));
    client.start(&["once.socket"]).unwrap();
    let _connection = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert!(wait_for(Duration::from_secs(5), || marker.exists()));
    std::thread::sleep(Duration::from_millis(300));
    let hits = std::fs::read_to_string(marker).unwrap();
    assert_eq!(
        hits.lines().count(),
        1,
        "one connection must be consumed once"
    );
}

#[test]
fn named_pipe_bind_rejects_an_existing_endpoint() {
    let scratch = Scratch::new();
    let first = rystemd::platform::net::bind_control(&scratch.pipe).unwrap();
    let second = rystemd::platform::net::bind_control(&scratch.pipe);
    assert!(
        second.is_err(),
        "a second manager must not claim the same pipe"
    );
    drop(first);
}

#[test]
fn short_lived_windows_service_preserves_captured_output() {
    let scratch = Scratch::new();
    scratch.write_unit(
        "output.service",
        "[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=cmd.exe /D /C echo final-output\n",
    );
    let _daemon = Daemon::start();
    let mut client = wait_for_client();
    assert!(wait_for(Duration::from_secs(5), || client
        .list_units(&[], None)
        .is_ok()));
    client.start(&["output.service"]).unwrap();
    assert!(wait_for(Duration::from_secs(5), || client
        .status(&["output.service"])
        .is_ok_and(|status| status.first().is_some_and(|unit| {
            unit.log.iter().any(|line| line.contains("final-output"))
        }))));
}

#[test]
fn tcp_socket_retriggers_a_service_after_a_failed_launch() {
    let scratch = Scratch::new();
    let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let marker = scratch.dir.path().join("failed-socket-hits.txt");
    scratch.write_unit(
        "retry.socket",
        &format!("[Socket]\nListenStream=127.0.0.1:{port}\n"),
    );
    scratch.write_unit(
        "retry.service",
        &format!(
            "[Service]\nType=oneshot\nExecStart=cmd.exe /D /C echo hit>>{} & exit 1\n",
            marker.display().to_string().replace('\\', "/")
        ),
    );
    let _daemon = Daemon::start();
    let mut client = wait_for_client();
    assert!(wait_for(Duration::from_secs(5), || client
        .list_units(&[], None)
        .is_ok()));
    client.start(&["retry.socket"]).unwrap();

    let _first = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert!(wait_for(Duration::from_secs(5), || client
        .is_active(&["retry.service"])
        .is_ok_and(|state| state == ["failed"])));

    let _second = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert!(wait_for(Duration::from_secs(5), || {
        std::fs::read_to_string(&marker).is_ok_and(|hits| hits.lines().count() == 2)
    }));
}
