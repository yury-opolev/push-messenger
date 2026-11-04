//! A Windows job object that kills the adapter (and whatever it started) when the bridge
//! closes it or exits.

#![allow(unsafe_code)]

use std::io;
use std::os::windows::io::RawHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

/// An open job handle; closing it (on drop) kills every process in the job.
pub(super) struct Job(HANDLE);

// SAFETY: a job handle is a kernel handle, usable from any thread; `Job` only closes it.
unsafe impl Send for Job {}
// SAFETY: `Job` has no `&self` methods that touch the handle.
unsafe impl Sync for Job {}

impl Job {
    /// A new job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, with `process` assigned to it.
    pub(super) fn kill_on_close(process: RawHandle) -> io::Result<Job> {
        // SAFETY: null security attributes and a null name (an unnamed job) are allowed.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Job(handle);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(io::Error::other)?;
        // SAFETY: `job.0` is the job we just created; `info` is a valid
        // JOBOBJECT_EXTENDED_LIMIT_INFORMATION of `size` bytes that outlives the call.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `job.0` is our job; `process` is the live child's process handle, owned
        // by its `Child`, which outlives the call.
        let ok = unsafe { AssignProcessToJobObject(job.0, process.cast()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it exactly once.
        unsafe { CloseHandle(self.0) };
    }
}
