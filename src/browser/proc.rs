//! Platform process helpers for short-lived browser probes.
//!
//! Windows-only: a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//! owns a probe's browser tree, so the kernel kills every process in the
//! job when the last handle closes (including if donsetch crashes). The
//! version probe spawns its browser suspended (`CREATE_SUSPENDED`),
//! assigns it to the job, then resumes it, so descendants cannot escape
//! the job between spawn and assignment.

#[cfg(windows)]
use windows_sys::Win32::Foundation as fnd;
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects as job;

// ntdll process suspend/resume : always loaded, linked directly.
#[cfg(windows)]
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtResumeProcess(proc_handle: fnd::HANDLE) -> i32;
}

/// Owned Windows Job Object that terminates every assigned process when
/// the handle closes. Browser probes use this primitive so job creation,
/// assignment and teardown have one owner.
#[cfg(windows)]
pub(crate) struct KillOnCloseJob {
    handle: fnd::HANDLE,
}

#[cfg(windows)]
impl KillOnCloseJob {
    pub(crate) fn new() -> Result<Self, String> {
        unsafe {
            let handle = job::CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(format!("CreateJobObjectW failed: {}", fnd::GetLastError()));
            }

            let mut info: job::JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = job::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if job::SetInformationJobObject(
                handle,
                job::JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            ) == 0
            {
                let error = fnd::GetLastError();
                fnd::CloseHandle(handle);
                return Err(format!("SetInformationJobObject failed: {error}"));
            }
            Ok(Self { handle })
        }
    }

    pub(crate) fn assign(&self, process: fnd::HANDLE) -> Result<(), u32> {
        if unsafe { job::AssignProcessToJobObject(self.handle, process) } == 0 {
            Err(unsafe { fnd::GetLastError() })
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        unsafe {
            fnd::CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
pub(crate) fn resume_process(process: fnd::HANDLE) -> Result<(), i32> {
    let status = unsafe { NtResumeProcess(process) };
    if status < 0 { Err(status) } else { Ok(()) }
}
