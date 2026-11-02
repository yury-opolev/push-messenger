//! The named pipe: a DACL for the user and SYSTEM, and a peer SID check both ways.

#![allow(unsafe_code)]

use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;
use std::time::Duration;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_NO_DATA, ERROR_PIPE_BUSY, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::core::PWSTR;

use super::{Stream, wrong_endpoint};
use crate::endpoint::Endpoint;

/// LocalSystem.
const SYSTEM_SID: &str = "S-1-5-18";
/// How often a client retries a busy pipe, 20 ms apart.
const BUSY_RETRIES: u32 = 50;

/// The pipe's SDDL for a user SID: protected, full access for the user and SYSTEM only.
pub fn pipe_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;{user_sid})(A;;GA;;;SY)")
}

/// The current process's user SID, e.g. `S-1-5-21-…`.
pub fn current_user_sid() -> io::Result<String> {
    // SAFETY: GetCurrentProcess has no preconditions; its pseudo handle needs no closing.
    let process = unsafe { GetCurrentProcess() };
    process_user_sid(process)
}

pub(super) struct Listener {
    name: String,
    /// The security descriptor every pipe instance gets; lives as long as the listener.
    descriptor: SecurityDescriptor,
    /// The instance waiting for the next client: there is always one, so the name never
    /// disappears while the listener lives.
    next: NamedPipeServer,
    our_sid: String,
    pub(super) refused: u64,
}

impl Listener {
    pub(super) fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        let Endpoint::Pipe(name) = endpoint else {
            return Err(wrong_endpoint());
        };
        let our_sid = current_user_sid()?;
        let descriptor = SecurityDescriptor::from_sddl(&pipe_sddl(&our_sid))?;
        let next = create_instance(name, &descriptor, true).map_err(|error| {
            let code = error.raw_os_error();
            if code == Some(ERROR_ACCESS_DENIED as i32) || code == Some(ERROR_PIPE_BUSY as i32) {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("the pipe {name} already exists"),
                )
            } else {
                error
            }
        })?;
        Ok(Listener {
            name: name.clone(),
            descriptor,
            next,
            our_sid,
            refused: 0,
        })
    }

    pub(super) async fn accept(&mut self) -> io::Result<Stream> {
        loop {
            match self.next.connect().await {
                Ok(()) => {}
                // The client came and went before we saw it: start over on a fresh instance.
                Err(error) if error.raw_os_error() == Some(ERROR_NO_DATA as i32) => {
                    self.next = create_instance(&self.name, &self.descriptor, false)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            let fresh = create_instance(&self.name, &self.descriptor, false)?;
            let connected = std::mem::replace(&mut self.next, fresh);
            if self.admits_client(&connected) {
                return Ok(Box::new(connected));
            }
            self.refused += 1;
        }
    }

    fn admits_client(&self, pipe: &NamedPipeServer) -> bool {
        let mut pid = 0u32;
        // SAFETY: the handle is the connected pipe instance we own; pid is a valid out
        // pointer.
        let ok = unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) };
        if ok == 0 {
            return false;
        }
        match pid_user_sid(pid) {
            Ok(sid) => sid == self.our_sid || sid == SYSTEM_SID,
            Err(_) => false,
        }
    }

    pub(super) fn dacl_sddl(&self) -> io::Result<String> {
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: the handle is our live pipe instance; the owner/group/DACL/SACL out
        // pointers may be null; descriptor is a valid out pointer, LocalFree'd below.
        let status = unsafe {
            GetSecurityInfo(
                self.next.as_raw_handle(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let descriptor = SecurityDescriptor(descriptor);
        let mut text: PWSTR = ptr::null_mut();
        // SAFETY: descriptor is the valid descriptor GetSecurityInfo returned; text is a
        // valid out pointer; the length out pointer may be null.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.0,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success text is a NUL-terminated, LocalAlloc'd wide string.
        Ok(unsafe { take_local_string(text) })
    }
}

pub(super) async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
    let Endpoint::Pipe(name) = endpoint else {
        return Err(wrong_endpoint());
    };
    let mut retries = 0;
    let client = loop {
        match ClientOptions::new().open(name) {
            Ok(client) => break client,
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && retries < BUSY_RETRIES =>
            {
                retries += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error),
        }
    };
    let mut pid = 0u32;
    // SAFETY: the handle is the client pipe we own; pid is a valid out pointer.
    let ok = unsafe { GetNamedPipeServerProcessId(client.as_raw_handle(), &mut pid) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let server_sid = pid_user_sid(pid)?;
    if server_sid != current_user_sid()? && server_sid != SYSTEM_SID {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the pipe's server runs as another user",
        ));
    }
    Ok(Box::new(client))
}

fn create_instance(
    name: &str,
    descriptor: &SecurityDescriptor,
    first: bool,
) -> io::Result<NamedPipeServer> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: attributes is a valid SECURITY_ATTRIBUTES for the duration of the call and
    // its descriptor is a valid self-relative descriptor that outlives the call.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
}

/// The user SID of the process with this id.
fn pid_user_sid(pid: u32) -> io::Result<String> {
    // SAFETY: OpenProcess takes no pointers; it returns null on failure.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = OwnedHandle(process);
    process_user_sid(process.0)
}

fn process_user_sid(process: HANDLE) -> io::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: process is a valid process handle with query access; token is a valid out
    // pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    let mut length = 0u32;
    // SAFETY: a size query: a null buffer of length 0 and a valid out pointer; it fails
    // with ERROR_INSUFFICIENT_BUFFER and sets length.
    unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut length) };
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64s: TOKEN_USER holds a pointer, so the buffer must be pointer-aligned.
    let mut buffer = vec![0u64; (length as usize).div_ceil(8)];
    // SAFETY: buffer is writable, 8-aligned and at least `length` bytes long.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation(TokenUser) wrote a TOKEN_USER at the buffer's start; its
    // SID points into the buffer, which outlives the use below.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    sid_string(sid)
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text: PWSTR = ptr::null_mut();
    // SAFETY: sid is a valid SID; text is a valid out pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success text is a NUL-terminated, LocalAlloc'd wide string.
    Ok(unsafe { take_local_string(text) })
}

/// Copies a wide string into a `String` and frees it.
///
/// # Safety
///
/// `text` is a NUL-terminated wide string allocated with `LocalAlloc`, not used after.
unsafe fn take_local_string(text: PWSTR) -> String {
    let mut length = 0;
    // SAFETY: the string is NUL-terminated (the caller's promise); we stop at the NUL.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: the `length` units before the NUL are initialized and readable.
    let string = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    // SAFETY: LocalAlloc'd (the caller's promise) and freed exactly once, here.
    unsafe { LocalFree(text.cast()) };
    string
}

/// A LocalAlloc'd security descriptor, freed on drop.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is a heap block only this value owns; nothing mutates it after
// creation, so moving it to or reading it from another thread is fine.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above, all access is read-only.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: wide is NUL-terminated; descriptor is a valid out pointer; the size out
        // pointer may be null.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(SecurityDescriptor(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor was LocalAlloc'd by the API that returned it and is freed
        // exactly once, here.
        unsafe { LocalFree(self.0) };
    }
}

/// A kernel handle we opened, closed on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is valid, ours, and closed exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}
