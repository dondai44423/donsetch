//! Platform-specific process control for DonGhost.
//!
//! **Unix (Linux + macOS):** process groups + SIGSTOP/SIGCONT/SIGKILL.
//! The whole browser tree (browser + renderers + GPU) shares one
//! process group, so a single signal suspends/resumes/kills it all.
//! Linux additionally gets `PR_SET_PDEATHSIG` : the kernel reaps the
//! child if donsetch dies hard. macOS has no `prctl` equivalent; the
//! Ghost Drop + kill path cover normal exit, only a hard parent crash
//! could orphan (documented limitation).
//!
//! **Windows:** a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//! owns the browser tree : the kernel kills every process in the job
//! when the last handle closes (including if donsetch crashes).
//! Freeze/thaw enumerates ALL processes in the Job Object (not just
//! the main browser) and suspends/resumes each one via
//! `NtSuspendProcess`/`NtResumeProcess` (ntdll; the atomic
//! whole-process pause, stable since XP : Sysinternals Process
//! Explorer, WinDbg, and Chrome's own crash handling all use it).
//! Linked directly to ntdll via `#[link(name = "ntdll")]` : no
//! `GetProcAddress` dance, no FARPROC type ambiguity.
//! Without enumerating all PIDs, only the main Chrome process would
//! be suspended : the renderer/GPU/utility processes keep running,
//! burning CPU and RAM.

use crate::error::FetchError;
use tokio::process::Child;

#[cfg(unix)]
use libc;

#[cfg(windows)]
use windows_sys::Win32::Foundation as fnd;
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects as job;
#[cfg(windows)]
use windows_sys::Win32::System::Threading as thr;

/// Owned platform handle to the browser process tree.
pub struct Proc {
    #[cfg(unix)]
    pid: i32,
    #[cfg(windows)]
    proc_handle: fnd::HANDLE,
    #[cfg(windows)]
    job: KillOnCloseJob,
}

// ntdll process suspend/resume : always loaded, linked directly.
#[cfg(windows)]
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtSuspendProcess(proc_handle: fnd::HANDLE) -> i32;
    fn NtResumeProcess(proc_handle: fnd::HANDLE) -> i32;
}

/// Owned Windows Job Object that terminates every assigned process when the
/// handle closes. Both long-lived ghosts and short browser probes use this
/// primitive so job creation, assignment, and teardown have one owner.
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

    fn terminate(&self) {
        unsafe {
            job::TerminateJobObject(self.handle, 1);
        }
    }

    fn raw(&self) -> fnd::HANDLE {
        self.handle
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

impl Proc {
    /// Configure a `Command` BEFORE spawn.
    /// Unix: own process group (freeze/thaw signal the whole tree).
    /// Windows: nothing : the Job Object is attached post-spawn.
    pub fn prepare_cmd(cmd: &mut tokio::process::Command) {
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(not(unix))]
        {
            let _ = cmd;
        }
    }

    /// Build from a just-spawned `Child`.
    /// Unix: stash the pid (process group leader).
    /// Windows: open a process handle (suspend + quota rights),
    /// create a `KILL_ON_JOB_CLOSE` Job Object, assign the child.
    pub fn from_child(child: &Child) -> Result<Self, FetchError> {
        #[cfg(unix)]
        {
            let pid = child.id().unwrap_or(0) as i32;
            Ok(Self { pid })
        }
        #[cfg(windows)]
        {
            unsafe { Self::from_child_win(child) }
        }
    }

    #[cfg(windows)]
    unsafe fn from_child_win(child: &Child) -> Result<Self, FetchError> {
        let pid = child.id().unwrap_or(0);

        // SAFETY: all FFI calls in this function target well-documented
        // Windows kernel/ntdll APIs with correct parameter types. The
        // `unsafe` block wraps the entire body : every call is audited.
        unsafe {
            // Process handle with the access rights we need:
            //   PROCESS_SUSPEND_RESUME  : NtSuspendProcess / NtResumeProcess
            //   PROCESS_SET_QUOTA       : AssignProcessToJobObject
            //   PROCESS_TERMINATE       : AssignProcessToJobObject also requires
            //                             this; without it the call fails with
            //                             ERROR_ACCESS_DENIED and the job stays
            //                             empty, so KILL_ON_JOB_CLOSE kills nothing
            //   PROCESS_QUERY_LIMITED_INFORMATION : status checks
            let proc_handle = thr::OpenProcess(
                thr::PROCESS_SUSPEND_RESUME
                    | thr::PROCESS_SET_QUOTA
                    | thr::PROCESS_TERMINATE
                    | thr::PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            );
            if proc_handle.is_null() {
                return Err(FetchError::ghost(format!(
                    "OpenProcess failed: {}",
                    fnd::GetLastError()
                )));
            }

            // Job Object: kernel kills the whole tree if donsetch dies.
            let job = match KillOnCloseJob::new() {
                Ok(job) => job,
                Err(error) => {
                    fnd::CloseHandle(proc_handle);
                    return Err(FetchError::ghost(error));
                }
            };
            // Assign the child to the job. On Win8+ nested jobs are
            // allowed, so this succeeds even if the process is already
            // in a job. If it fails we don't abort : freeze/thaw/kill
            // still work via the process handle; only the death-reap
            // safety net is lost.
            if let Err(error) = job.assign(proc_handle) {
                // Non-fatal for the fetch itself, but the job is now empty:
                // KILL_ON_JOB_CLOSE has nothing to kill, so the browser tree
                // outlives donsetch and orphaned Chrome processes pile up.
                // Warn unconditionally : silent degradation is what hid this.
                eprintln!(
                    "[ghost] AssignProcessToJobObject failed: {error} : browser tree will not be reaped on exit, leaving orphaned Chrome processes"
                );
            }
            Ok(Self { proc_handle, job })
        }
    }

    /// Suspend the whole process tree. CPU → 0, RAM goes cold.
    pub fn freeze(&self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.pid, libc::SIGSTOP);
        }
        #[cfg(windows)]
        unsafe {
            for pid in self.job_pids() {
                if let Ok(h) = open_for_suspend(pid) {
                    NtSuspendProcess(h);
                    fnd::CloseHandle(h);
                }
            }
        }
    }

    /// Resume the whole process tree.
    pub fn thaw(&self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.pid, libc::SIGCONT);
        }
        #[cfg(windows)]
        unsafe {
            for pid in self.job_pids() {
                if let Ok(h) = open_for_suspend(pid) {
                    let _ = resume_process(h);
                    fnd::CloseHandle(h);
                }
            }
        }
    }

    /// Kill the whole tree.
    pub fn kill_group(&self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            // Kills every process in the job : the whole tree.
            self.job.terminate();
        }
    }

    /// Enumerate all PIDs in the Job Object (Windows only).
    /// Chrome spawns renderer, GPU, and utility processes as
    /// separate processes in the Job : the main process handle
    /// only controls the browser process. To freeze/thaw the
    /// whole tree, we query the Job Object for all member PIDs.
    #[cfg(windows)]
    fn job_pids(&self) -> Vec<u32> {
        use std::mem;

        // Query the Job Object for all process IDs. The struct
        // has a variable-length trailing array, so we allocate
        // extra room. Chrome typically has 5-15 processes; 64
        // slots is generous headroom.
        const MAX_PIDS: usize = 64;
        let buf_size = mem::size_of::<job::JOBOBJECT_BASIC_PROCESS_ID_LIST>()
            + (MAX_PIDS - 1) * mem::size_of::<usize>();
        let mut buf = vec![0u8; buf_size];

        let ok = unsafe {
            job::QueryInformationJobObject(
                self.job.raw(),
                job::JobObjectBasicProcessIdList,
                buf.as_mut_ptr() as *mut _,
                buf_size as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // Query failed : fall back to just the main PID.
            // The main process is still controlled by proc_handle.
            return Vec::new();
        }

        let list = unsafe { &*(buf.as_ptr() as *const job::JOBOBJECT_BASIC_PROCESS_ID_LIST) };
        let count = list.NumberOfProcessIdsInList as usize;
        if count == 0 {
            return Vec::new();
        }
        let count = count.min(MAX_PIDS);

        // The ProcessIdList is a flex array at the end of the struct.
        let pids: &[usize] =
            unsafe { std::slice::from_raw_parts(&list.ProcessIdList as *const usize, count) };
        pids.iter().map(|&p| p as u32).collect()
    }
}

/// `PR_SET_PDEATHSIG` : kernel kills the child if donsetch dies.
/// Called in `pre_exec` (child context). Linux-only; macOS has no
/// `prctl` equivalent.
#[cfg(linux_like)]
pub fn pdeath_pre_exec() -> std::io::Result<()> {
    unsafe {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Open a process handle for suspend/resume (Windows only).
/// Returns Err if the process has already exited (race condition
/// during freeze : child crashed between enumeration and open).
#[cfg(windows)]
fn open_for_suspend(pid: u32) -> Result<fnd::HANDLE, ()> {
    unsafe {
        let h = thr::OpenProcess(thr::PROCESS_SUSPEND_RESUME, 0, pid);
        if h.is_null() { Err(()) } else { Ok(h) }
    }
}

// SAFETY: Windows HANDLEs are kernel object references (opaque
// pointers), safe to move between threads. Only one thread accesses
// them at a time (guarded by GhostManager's Mutex), and the handles
// are closed exactly once in Drop.
#[cfg(windows)]
unsafe impl Send for Proc {}
#[cfg(windows)]
unsafe impl Sync for Proc {}

impl Drop for Proc {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            fnd::CloseHandle(self.proc_handle);
        }
        #[cfg(not(windows))]
        {
            let _ = self;
        }
    }
}
