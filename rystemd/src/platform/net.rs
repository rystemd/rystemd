//! Unix-socket IPC: the control stream (`systemctl`/`SocketClient` → daemon),
//! the `sd_notify` datagram, and the client-side request helper.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::Path;

use serde_json::Value;

/// Bind (and rebind) a stream listener for the control channel. Non-blocking:
/// the event loop drives `accept()` via `poll`, so a blocking accept would
/// stall the whole manager.
pub fn bind_control(path: &Path) -> Result<UnixListener, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path).map_err(|e| e.to_string())?;
    l.set_nonblocking(true).map_err(|e| e.to_string())?;
    // Owner-only regardless of umask. Peer access to control is further gated
    // by the manager's UID check; the restrictive mode keeps unprivileged
    // connect attempts from even reaching it.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    Ok(l)
}

/// Bind (and rebind) the datagram socket services use for `sd_notify`.
/// Non-blocking for the same reason as [`bind_control`].
pub fn bind_notify(path: &Path) -> Result<UnixDatagram, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _ = std::fs::remove_file(path);
    let s = UnixDatagram::bind(path).map_err(|e| e.to_string())?;
    s.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(s)
}

/// Send one JSON request line to `socket` and return the response `data`.
pub fn request(socket: &Path, req: &Value) -> Result<Value, String> {
    let stream = UnixStream::connect(socket)
        .map_err(|e| format!("Failed to connect to manager at {}: {e}", socket.display()))?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut out = String::new();
    reader
        .read_line(&mut out)
        .map_err(|e| format!("failed to read response: {e}"))?;
    let v: Value = serde_json::from_str(&out).map_err(|e| format!("bad response: {e}"))?;
    if v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        Ok(v.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string())
    }
}

/// UID of the peer on the other end of `stream`. Linux `SO_PEERCRED`.
#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 { Some(cred.uid) } else { None }
}

/// Non-Linux unix has no uniform `SO_PEERCRED`; defer to the caller.
#[cfg(not(target_os = "linux"))]
pub fn peer_uid(_stream: &UnixStream) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[cfg(target_os = "linux")]
    #[test]
    fn control_socket_mode_is_independent_of_umask() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        // Hostile umask: without an explicit permission the socket would be
        // created world-writable.
        unsafe {
            libc::umask(0);
        }
        let listener = bind_control(&sock).unwrap();
        unsafe {
            libc::umask(0o022);
        }
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode, 0o600,
            "control socket must be owner-only regardless of umask"
        );
        drop(listener);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn peer_uid_reports_the_connecting_process_identity() {
        use std::os::unix::net::{UnixListener, UnixStream};
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("peer.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let connector = UnixStream::connect(&sock).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        assert_eq!(peer_uid(&accepted), Some(unsafe { libc::geteuid() }));
        // The connecting end reports the same peer.
        assert_eq!(peer_uid(&connector), Some(unsafe { libc::geteuid() }));
    }
}
