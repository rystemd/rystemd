//! Win32 named-pipe control transport.
//!
//! The manager remains single-threaded: one blocking pipe acceptor thread
//! forwards complete JSON lines through a channel and waits for the manager's
//! serialized response. The manager never performs blocking pipe I/O.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use serde_json::Value;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GetLastError,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    WaitNamedPipeW,
};

struct ClientPipe(windows_sys::Win32::Foundation::HANDLE);

impl Drop for ClientPipe {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub struct PendingRequest {
    pub line: String,
    reply: mpsc::Sender<String>,
}
impl PendingRequest {
    pub fn respond(self, response: String) {
        let _ = self.reply.send(response);
    }
}

pub struct ControlListener {
    requests: Mutex<mpsc::Receiver<PendingRequest>>,
    stop: Arc<AtomicBool>,
    pipe_name: String,
}

impl ControlListener {
    pub fn drain(&self) -> Vec<PendingRequest> {
        self.requests.lock().unwrap().try_iter().collect()
    }
}

impl Drop for ControlListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(handle) = open_pipe(&self.pipe_name, 50) {
            unsafe {
                CloseHandle(handle);
            }
        }
    }
}

pub fn bind_control(path: &Path) -> Result<ControlListener, String> {
    let pipe_name = normalized_pipe_name(path);
    // Create the first instance synchronously: bind succeeds only when the
    // endpoint is actually ours, and FILE_FLAG_FIRST_PIPE_INSTANCE prevents
    // silently attaching to a pre-created pipe.
    let first = create_pipe(&pipe_name, true)
        .map_err(|error| format!("Failed to bind manager pipe {pipe_name}: {error}"))?;
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker_name = pipe_name.clone();
    let first_value = first as usize;
    if let Err(error) = std::thread::Builder::new()
        .name("rystemd-control-pipe".into())
        .spawn(move || accept_loop(&worker_name, first_value, worker_stop, tx))
    {
        unsafe {
            CloseHandle(first);
        }
        return Err(error.to_string());
    }
    Ok(ControlListener {
        requests: Mutex::new(rx),
        stop,
        pipe_name,
    })
}

pub fn request(socket: &Path, req: &Value) -> Result<Value, String> {
    let pipe_name = normalized_pipe_name(socket);
    let handle = ClientPipe(
        open_pipe(&pipe_name, 5_000)
            .map_err(|error| format!("Failed to connect to manager at {pipe_name}: {error}"))?,
    );
    let mut line = serde_json::to_string(req).map_err(|error| error.to_string())?;
    line.push('\n');
    write_all(handle.0, line.as_bytes()).map_err(|error| error.to_string())?;
    let response = read_line(handle.0).map_err(|error| error.to_string())?;
    let value: Value =
        serde_json::from_str(&response).map_err(|error| format!("bad response: {error}"))?;
    if value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        Ok(value.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string())
    }
}

fn accept_loop(
    name: &str,
    first: usize,
    stop: Arc<AtomicBool>,
    requests: mpsc::Sender<PendingRequest>,
) {
    let mut first = Some(first);
    while !stop.load(Ordering::SeqCst) {
        let handle = match first.take() {
            Some(handle) => handle as windows_sys::Win32::Foundation::HANDLE,
            None => match create_pipe(name, false) {
                Ok(handle) => handle,
                Err(_) => return,
            },
        };
        let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
        if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            unsafe {
                CloseHandle(handle);
            }
            continue;
        }
        if stop.load(Ordering::SeqCst) {
            unsafe {
                DisconnectNamedPipe(handle);
                CloseHandle(handle);
            }
            break;
        }
        if let Ok(line) = read_line(handle) {
            let (reply_tx, reply_rx) = mpsc::channel();
            if requests
                .send(PendingRequest {
                    line,
                    reply: reply_tx,
                })
                .is_ok()
                && let Ok(response) = reply_rx.recv()
            {
                let _ = write_all(handle, response.as_bytes());
            }
        }
        unsafe {
            FlushFileBuffers(handle);
            DisconnectNamedPipe(handle);
            CloseHandle(handle);
        }
    }
}

fn create_pipe(name: &str, first: bool) -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
    let wide = wide(name);
    let access = PIPE_ACCESS_DUPLEX
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            access,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

fn open_pipe(
    name: &str,
    timeout_ms: u32,
) -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
    let wide = wide(name);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(u64::from(timeout_ms));
    loop {
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(handle);
        }
        let error = unsafe { GetLastError() };
        if error == ERROR_FILE_NOT_FOUND && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        if error != ERROR_PIPE_BUSY {
            return Err(std::io::Error::from_raw_os_error(error as i32));
        }
        let remaining = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis()
            .min(u128::from(u32::MAX)) as u32;
        if remaining == 0 || unsafe { WaitNamedPipeW(wide.as_ptr(), remaining) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
}

fn read_line(handle: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                handle,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read as usize]);
        if bytes.contains(&b'\n') {
            break;
        }
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(bytes.len());
    String::from_utf8(bytes[..end].to_vec())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn write_all(
    handle: windows_sys::Win32::Foundation::HANDLE,
    mut bytes: &[u8],
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn normalized_pipe_name(path: &Path) -> String {
    let value = path.to_string_lossy();
    if value.starts_with(r"\\.\pipe\") {
        return value.into_owned();
    }
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!(r"\\.\pipe\rystemd-{hash:016x}")
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
