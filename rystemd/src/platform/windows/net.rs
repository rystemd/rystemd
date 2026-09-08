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
    INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, EqualSid, GetTokenInformation, RevertToSelf,
    SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WinBuiltinAdministratorsSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, ImpersonateNamedPipeClient,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};
use windows_sys::core::PWSTR;

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

pub fn bind_control(path: &Path, user: bool) -> Result<ControlListener, String> {
    let pipe_name = normalized_pipe_name(path);
    // Create the first instance synchronously: bind succeeds only when the
    // endpoint is actually ours, and FILE_FLAG_FIRST_PIPE_INSTANCE prevents
    // silently attaching to a pre-created pipe.
    let first = create_pipe(&pipe_name, true, user)
        .map_err(|error| format!("Failed to bind manager pipe {pipe_name}: {error}"))?;
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker_name = pipe_name.clone();
    let first_value = first as usize;
    if let Err(error) = std::thread::Builder::new()
        .name("rystemd-control-pipe".into())
        .spawn(move || accept_loop(&worker_name, first_value, user, worker_stop, tx))
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
    user: bool,
    stop: Arc<AtomicBool>,
    requests: mpsc::Sender<PendingRequest>,
) {
    let mut first = Some(first);
    while !stop.load(Ordering::SeqCst) {
        let handle = match first.take() {
            Some(handle) => handle as windows_sys::Win32::Foundation::HANDLE,
            None => match create_pipe(name, false, user) {
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
        // Per-caller authorization, defense in depth behind the kernel's DACL:
        // impersonate the connected client, read its token user SID, and
        // confirm it is one of the allowed identities. Mutating operations are
        // reachable only through this pipe, so authorizing the connection
        // authorizes every request it carries. On failure the peer is
        // disconnected without reading its request — fail closed.
        if !client_authorized(handle, user) {
            unsafe {
                DisconnectNamedPipe(handle);
                CloseHandle(handle);
            }
            continue;
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

fn create_pipe(
    name: &str,
    first: bool,
    user: bool,
) -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
    let wide = wide(name);
    let access = PIPE_ACCESS_DUPLEX
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    // Explicit DACL: this is the trust boundary for who may issue control
    // requests. The system manager's pipe admits SYSTEM + Administrators; the
    // user manager's pipe admits the owning user (resolved from the current
    // process token). Without this, CreateNamedPipeW with NULL lpSecurity
    // attributes gives the pipe the default DACL (typically "Everyone" on the
    // system bus) — fail open. We build a self-relative security descriptor
    // from an SDDL string and pass it in SECURITY_ATTRIBUTES, so the allow
    // list is enforced by the kernel at ConnectNamedPipe time (defense in
    // depth behind the per-caller check in accept_loop).
    let Some(security_attributes) = build_pipe_security(user) else {
        return Err(std::io::Error::from_raw_os_error(
            windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32,
        ));
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
            security_attributes.as_ptr(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

/// Build a `SECURITY_ATTRIBUTES` carrying an explicit DACL for the control
/// pipe. `user` selects the allow list. Returns `None` if the descriptor could
/// not be constructed (caller treats it as a hard bind failure).
///
/// The DACL is built from an SDDL string via the kernel's own
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW` (SDDL revision 1).
/// This yields a self-relative descriptor ready for `CreateNamedPipeW`, and —
/// because it only grants the allowed identities — the allow list is enforced
/// at `ConnectNamedPipe` time, so an unauthorized peer is refused before it
/// can send anything. `client_authorized` below is defense in depth.
///
/// ACLs:
/// - system manager: `D:(A;;GA;;;SY)(A;;GA;;;BA)` — SYSTEM + Builtin
///   Administrators, full access (GA = GENERIC_ALL).
/// - user manager: `D:(A;;GA;;;<owner-SID>)` — the manager's own primary SID,
///   resolved from this process's token. SYSTEM is intentionally omitted:
///   a user manager is owned by the user, not the machine.
fn build_pipe_security(user: bool) -> Option<SecurityAttributes> {
    // `owner_sid` lives in `buffer`; keep the buffer alive through the SDDL
    // construction below.
    let (buffer, owner_sid) = process_token_user_sid()?;
    let sddl = if user {
        // Grant the owning user full control. The SID string comes from the
        // token we just read, so it is exactly the manager's owner.
        format!("D:(A;;GA;;;{})", sid_to_string(owner_sid)?)
    } else {
        // SYSTEM + Administrators only. No Everyone/Users: control operations
        // (start/stop/restart units) must not be reachable by a normal user.
        "D:(A;;GA;;;SY)(A;;GA;;;BA)".to_string()
    };
    drop(buffer);
    let sddl_wide = wide(&sddl);
    let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut size = 0u32;
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            1, // SECURITY_DESCRIPTOR_REVISION
            &mut descriptor,
            &mut size,
        )
    };
    if ok == 0 || descriptor.is_null() {
        return None;
    }
    Some(SecurityAttributes {
        sa: SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        },
        _descriptor: descriptor,
    })
}

/// `SECURITY_ATTRIBUTES` plus ownership of the kernel-allocated descriptor
/// (must be released with `LocalFree`, which `Drop` does). Lifetime must
/// span the `CreateNamedPipeW` call so the descriptor stays valid; the pipe
/// installs its own copy of the DACL, so releasing afterwards is safe.
struct SecurityAttributes {
    sa: SECURITY_ATTRIBUTES,
    _descriptor: *mut core::ffi::c_void,
}
impl SecurityAttributes {
    fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.sa
    }
}
impl Drop for SecurityAttributes {
    fn drop(&mut self) {
        if !self._descriptor.is_null() {
            unsafe {
                LocalFree(self._descriptor as _);
            }
        }
    }
}

/// The primary SID of `token`, returned alongside the `Vec` buffer the SID
/// pointer lives in (the pointer is only valid while the buffer is alive).
fn token_user_sid(token: *mut core::ffi::c_void) -> Option<(Vec<u8>, *mut core::ffi::c_void)> {
    // First call with a null buffer returns the required size in return_length.
    let mut needed = 0u32;
    let _ = unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return None;
    }
    let mut buffer = vec![0u8; needed as usize];
    let mut got = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr() as *mut core::ffi::c_void,
            buffer.len() as u32,
            &mut got,
        )
    };
    if ok == 0 {
        return None;
    }
    let token_user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    Some((buffer, token_user.User.Sid))
}

/// Open the impersonation token of the current thread while it is impersonating
/// a pipe client. Returns the token handle or `None`. Caller must close it.
fn impersonated_client_token() -> Option<*mut core::ffi::c_void> {
    let thread = unsafe { GetCurrentThread() };
    let mut token: *mut core::ffi::c_void = std::ptr::null_mut();
    // open_as_self = FALSE: resolve against the thread's impersonation token.
    if unsafe { OpenThreadToken(thread, TOKEN_QUERY, 0, &mut token) } != 0 && !token.is_null() {
        Some(token)
    } else {
        None
    }
}

/// Whether the connected client's token user is one of the allowed identities
/// for the pipe's mode. Impersonates the client so we inspect the client's
/// token, then reverts before returning.
///
/// - system manager: the impersonated client is a member of
///   BUILTIN\Administrators (SYSTEM is itself a member, so this admits both
///   the SYSTEM service and interactive admins).
/// - user manager: the impersonated client's primary SID equals the manager's
///   owner SID.
///
/// Fail closed: any impersonation, token-open, token-read, or membership-check
/// error denies the client.
fn client_authorized(handle: windows_sys::Win32::Foundation::HANDLE, user: bool) -> bool {
    let impersonated = unsafe { ImpersonateNamedPipeClient(handle) };
    if impersonated == 0 {
        let _ = unsafe { RevertToSelf() };
        return false;
    }
    let result = match impersonated_client_token() {
        Some(client_token) => {
            // `token_user_sid` keeps the SID pointer and its backing buffer
            // together; the buffers must stay alive across the comparison.
            let client = token_user_sid(client_token);
            let verdict = match client {
                Some((client_buffer, client_sid)) => {
                    if user {
                        // Identity equality: client's primary SID == owner's.
                        let equal = match process_token_user_sid() {
                            Some((owner_buffer, owner_sid)) => {
                                let equal = unsafe { EqualSid(owner_sid, client_sid) } != 0;
                                drop(owner_buffer);
                                equal
                            }
                            None => false,
                        };
                        drop(client_buffer);
                        equal
                    } else {
                        // Membership: client is in BUILTIN\Administrators.
                        let mut is_member: windows_sys::core::BOOL = 0;
                        let checked = match admin_sid() {
                            Some((_keep_alive, admin)) => unsafe {
                                CheckTokenMembership(client_token, admin, &mut is_member)
                            },
                            None => 0,
                        };
                        drop(client_buffer);
                        checked != 0 && is_member != 0
                    }
                }
                None => false,
            };
            unsafe {
                CloseHandle(client_token);
            }
            verdict
        }
        None => {
            let _ = unsafe { RevertToSelf() };
            return false;
        }
    };
    let _ = unsafe { RevertToSelf() };
    result
}

/// The primary SID of the current *process* token (the manager's owner),
/// returned with the buffer it lives in. Caller must keep the buffer alive.
fn process_token_user_sid() -> Option<(Vec<u8>, *mut core::ffi::c_void)> {
    let process = unsafe { GetCurrentProcess() };
    let mut token: *mut core::ffi::c_void = std::ptr::null_mut();
    let ok = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
    if ok == 0 || token.is_null() {
        return None;
    }
    let sid = token_user_sid(token);
    unsafe {
        CloseHandle(token);
    }
    sid
}

/// Convert `sid` to its string form (`S-1-5-...`), or `None` on failure.
fn sid_to_string(sid: *mut core::ffi::c_void) -> Option<String> {
    let mut ptr: PWSTR = std::ptr::null_mut();
    let ok = unsafe { ConvertSidToStringSidW(sid, &mut ptr) };
    if ok == 0 || ptr.is_null() {
        return None;
    }
    // Walk the null-terminated wide string to its length.
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let mut chars = Vec::with_capacity(len);
    for i in 0..len {
        chars.push(unsafe { *ptr.add(i) });
    }
    let value = String::from_utf16(&chars).ok();
    unsafe {
        LocalFree(ptr as _);
    }
    value
}

/// The BUILTIN\Administrators well-known SID, freshly allocated, or `None`.
///
/// Returns the SID as a raw pointer valid only while the returned allocation
/// (a `Box<[u8]>`) is alive, so the caller must keep the `Box` in scope for
/// the duration of any API call that uses the pointer.
fn admin_sid() -> Option<(Box<[u8]>, *mut core::ffi::c_void)> {
    // CreateWellKnownSid needs a buffer of SECURITY_MAX_SID_SIZE bytes.
    let mut buffer = vec![0u8; SECURITY_MAX_SID_SIZE as usize].into_boxed_slice();
    let mut size = SECURITY_MAX_SID_SIZE;
    let ptr = buffer.as_mut_ptr() as *mut core::ffi::c_void;
    let ok = unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            ptr,
            &mut size,
        )
    };
    if ok == 0 {
        return None;
    }
    Some((buffer, ptr))
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

    /// Round-trip a `SecurityAttributes` descriptor to its DACL SDDL string.
    fn dacl_sddl(attrs: &SecurityAttributes) -> String {
        let mut ptr: windows_sys::core::PWSTR = std::ptr::null_mut();
        let mut len = 0u32;
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                attrs.sa.lpSecurityDescriptor,
                1,
                DACL_SECURITY_INFORMATION,
                &mut ptr,
                &mut len,
            )
        };
        assert_ne!(ok, 0, "security descriptor -> sddl failed");
        let mut chars = Vec::new();
        let mut i = 0usize;
        while unsafe { *ptr.add(i) } != 0 {
            chars.push(unsafe { *ptr.add(i) });
            i += 1;
        }
        let sddl = String::from_utf16(&chars).expect("sddl utf16");
        unsafe {
            LocalFree(ptr as _);
        }
        sddl
    }

    /// The user-mode pipe DACL must grant only the owning user and must not be
    /// fail-open (i.e. must not auto-grant Everyone, Authenticated Users, or
    /// the Users group). Issue #6.
    #[test]
    fn user_pipe_dacl_is_not_fail_open() {
        let attrs = build_pipe_security(true).expect("user security attrs");
        let sddl = dacl_sddl(&attrs).to_lowercase();
        // DACL present and ACEs granted to a single trustee.
        assert!(sddl.starts_with("d:"), "missing DACL fragment: {sddl}");
        // The granted trustee must be a real resolved SID (S-1-5-...), which
        // the manager's own user is.
        assert!(
            sddl.contains("(a;;ga;;;s-1-5-"),
            "user pipe must grant exactly the owner SID: {sddl}"
        );
        // Not fail-open: no well-known broad logon trustees.
        assert!(!sddl.contains("s-1-1-0"), "grants Everyone: {sddl}");
        assert!(
            !sddl.contains("s-1-5-11"),
            "grants Authenticated Users: {sddl}"
        );
        assert!(!sddl.contains("s-1-5-32-545"), "grants Users group: {sddl}");
    }

    /// The system-mode pipe DACL must grant SYSTEM (SY) and Administrators
    /// (BA), and nothing else. Issue #6.
    #[test]
    fn system_pipe_dacl_grants_system_and_administrators_only() {
        let attrs = build_pipe_security(false).expect("system security attrs");
        let sddl = dacl_sddl(&attrs);
        assert!(sddl.starts_with("D:"), "missing DACL fragment: {sddl}");
        assert!(sddl.contains("(A;;GA;;;SY)"), "must grant SYSTEM: {sddl}");
        assert!(
            sddl.contains("(A;;GA;;;BA)"),
            "must grant Administrators: {sddl}"
        );
        assert!(!sddl.contains("S-1-1-0"), "grants Everyone: {sddl}");
        assert!(!sddl.contains("S-1-5-32-545"), "grants Users group: {sddl}");
        let lower = sddl.to_lowercase();
        // Only two ACEs: exactly SYSTEM and Administrators.
        assert_eq!(
            lower.matches("(a;;ga;;;").count(),
            2,
            "system pipe should have exactly two granted ACEs: {sddl}"
        );
    }
}
