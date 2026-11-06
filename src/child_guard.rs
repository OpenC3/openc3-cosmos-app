// Copyright 2026 OpenC3, Inc.
// All Rights Reserved.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See LICENSE.md for more details.
//
// This file may also be used under the terms of a commercial license
// if purchased from OpenC3, Inc.

//! Tie host-microservice child processes to this app's lifetime, so they never
//! outlive it — even when the app is force-killed or crashes and gets no chance
//! to stop them. Without this, an orphaned host interface keeps its serial/USB
//! device open (so the next launch can't open it) and keeps its hub pairing.
//!
//! Three mechanisms, layered:
//!
//! * **Stdin lifeline (all platforms).** Each child gets a pipe on stdin that
//!   only this process holds the write end of, plus
//!   `OPENC3_STOP_ON_STDIN_EOF=1`. The host runner
//!   (`host_interface_microservice.py`) treats EOF on stdin as a stop request.
//!   The OS closes the write end however we exit, so the child always notices.
//!   Closing it deliberately is also the graceful stop on Windows, which has
//!   no SIGINT for windowless processes.
//! * **Linux:** `PR_SET_PDEATHSIG` makes the kernel send the child SIGTERM when
//!   its parent dies. (It fires when the *spawning thread* exits; children are
//!   spawned from the operator thread, which only exits after stopping them.)
//! * **Windows:** each child runs in its own Job Object with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: when our handle closes (including on
//!   process death) the OS kills everything in the job. That includes the real
//!   interpreter behind the venv's `python.exe` launcher, which a plain
//!   `Child::kill` of the launcher would miss. The child is created suspended
//!   and only resumed once it's in the job, so it can't start the interpreter
//!   before it's contained.
//!
//! macOS has no OS-level equivalent, so it relies on the stdin lifeline (plus
//! the startup sweep in [`crate::orphans`] as a final backstop).

use std::io;
use std::process::{Child, ChildStdin, Command, Stdio};

/// Env var telling the host runner to stop when its stdin reaches EOF. Must
/// match `host_interface_microservice.py` in the COSMOS repo.
pub const STOP_ON_STDIN_EOF_ENV: &str = "OPENC3_STOP_ON_STDIN_EOF";

/// Configured before spawn; turned into a [`ChildGuard`] by [`Prepared::attach`].
pub struct Prepared {
    #[cfg(windows)]
    job: Option<windows::Job>,
}

/// Keeps a spawned child tied to us. Dropping it releases the lifeline: on
/// Windows that kills the child's job.
pub struct ChildGuard {
    stdin: Option<ChildStdin>,
    #[cfg(windows)]
    job: Option<windows::Job>,
}

/// Configure `cmd` (before `spawn`) so the child is tied to our lifetime.
pub fn prepare(cmd: &mut Command) -> Prepared {
    cmd.stdin(Stdio::piped());
    cmd.env(STOP_ON_STDIN_EOF_ENV, "1");

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: the closure only makes async-signal-safe calls (prctl,
        // getppid, raise) between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // We may have died between fork and prctl; then no signal will
                // ever come, so act on it now.
                if libc::getppid() != parent {
                    libc::raise(libc::SIGTERM);
                }
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        let job = windows::Job::new();
        // Only start suspended when there's a job to put it in (we must resume
        // it afterwards either way).
        let flags = if job.is_some() { CREATE_NO_WINDOW | CREATE_SUSPENDED } else { CREATE_NO_WINDOW };
        cmd.creation_flags(flags);
        Prepared { job }
    }

    #[cfg(not(windows))]
    Prepared {}
}

impl Prepared {
    /// Finish tying the just-spawned `child` to us. On Windows this assigns it
    /// to its job and resumes it; if resuming fails the child is killed and an
    /// error returned (it would otherwise sit suspended forever).
    pub fn attach(self, child: &mut Child) -> io::Result<ChildGuard> {
        let stdin = child.stdin.take();

        #[cfg(windows)]
        {
            let mut job = self.job;
            if let Some(j) = &job {
                if let Err(e) = j.assign(child) {
                    // Still usable without containment (stdin lifeline remains).
                    crate::logging::warn("MicroserviceOperator", &format!("could not assign host microservice to a job object: {e}"));
                    job = None;
                }
                if let Err(e) = windows::resume(child) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e);
                }
            }
            Ok(ChildGuard { stdin, job })
        }

        #[cfg(not(windows))]
        {
            let _ = self;
            Ok(ChildGuard { stdin })
        }
    }
}

impl ChildGuard {
    /// Ask the child to stop by closing its stdin (EOF). Graceful on every
    /// platform; the only graceful option on Windows.
    pub fn request_stop(&mut self) {
        self.stdin = None;
    }

    /// Forcefully kill the child and anything it started (Windows: the whole
    /// job, i.e. the venv launcher *and* the interpreter). Elsewhere this is a
    /// no-op; the caller's `Child::kill` covers the single process.
    pub fn terminate(&mut self) {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    #[link(name = "ntdll")]
    extern "system" {
        /// Resumes every thread of a process. Used because `std::process::Child`
        /// doesn't expose the primary thread handle needed by `ResumeThread`.
        fn NtResumeProcess(process: HANDLE) -> i32;
    }

    /// A job object that kills its processes when its last handle closes.
    pub struct Job(HANDLE);

    // SAFETY: a job handle is a kernel handle usable from any thread.
    unsafe impl Send for Job {}

    impl Job {
        pub fn new() -> Option<Self> {
            // SAFETY: plain Win32 calls with valid arguments; the handle is
            // owned by the returned Job and closed in Drop.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let job = Job(handle);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                (ok != 0).then_some(job)
            }
        }

        pub fn assign(&self, child: &Child) -> io::Result<()> {
            // SAFETY: both handles are valid for the duration of the call.
            let ok = unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) };
            if ok == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub fn terminate(&self) {
            // SAFETY: valid job handle.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: we own the handle. Closing the last handle kills the job's
            // processes (KILL_ON_JOB_CLOSE).
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn resume(child: &Child) -> io::Result<()> {
        // SAFETY: valid process handle owned by `child`.
        let status = unsafe { NtResumeProcess(child.as_raw_handle() as HANDLE) };
        if status < 0 {
            Err(io::Error::other(format!("NtResumeProcess failed (NTSTATUS {status:#x})")))
        } else {
            Ok(())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_exit(child: &mut Child) -> Option<std::process::ExitStatus> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        None
    }

    #[test]
    fn closing_the_lifeline_gives_the_child_eof() {
        // `cat` exits exactly when its stdin reaches EOF.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "cat >/dev/null"]);
        let prepared = prepare(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let mut guard = prepared.attach(&mut child).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(child.try_wait().unwrap().is_none(), "child must run while the lifeline is held");
        guard.request_stop();
        assert!(wait_exit(&mut child).is_some(), "child should see EOF and exit");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn child_gets_sigterm_when_its_spawning_thread_dies() {
        use std::os::unix::process::ExitStatusExt;
        // Simulates the operator (the spawning thread) dying without cleanup.
        let mut child = std::thread::spawn(|| {
            let mut cmd = Command::new("sleep");
            cmd.arg("30");
            let prepared = prepare(&mut cmd);
            let mut child = cmd.spawn().unwrap();
            // Keep the lifeline open so only the death signal can end it.
            std::mem::forget(prepared.attach(&mut child).unwrap());
            child
        })
        .join()
        .unwrap();
        let status = wait_exit(&mut child).expect("child should be signalled when its parent thread exits");
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }
}
