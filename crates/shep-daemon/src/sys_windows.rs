//! The Windows counterpart to `sys`: a job object per sheep
//!
//! Windows has no process groups, so where the unix arm spawns each sheep
//! with `process_group(0)`, here the sheep is assigned to a job object: a
//! kernel container its children inherit and which terminates as a unit.
//! A member cannot escape a job created without
//! `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, so `kill_tree` reaches more of the tree
//! here than on unix, where a `setsid` grandchild escapes.
//!
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is not set: the daemon holds the
//! handle, so it would take the whole flock down with the shepherd. On unix
//! a sheep outlives a killed shepherd, and the platforms stay aligned.

#![allow(unsafe_code)] // every unsafe site on this platform is in this file

use std::io;
use std::os::windows::io::RawHandle;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};

/// Marks this process's own stdio handles non-inheritable.
///
/// The Windows counterpart to `shep-cli`'s `seal_inherited_fds`.
/// `CreateProcess` with `bInheritHandles = TRUE`, which `std` passes whenever
/// any stdio is redirected, inherits every inheritable handle the parent
/// holds, not only the ones `std` prepared. A shepherd spawned from a shell
/// that gave `shep` a pipe for stdout would hold that pipe open for life, so
/// `shep start | Out-Null` never returns.
///
/// Called immediately before a spawn: it changes only the inherit flag, so
/// this process goes on using its own stdio. Best-effort, nothing to report.
pub fn seal_std_handles() {
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` takes one of the three documented standard
        // handle ids and returns a handle this process already owns, not a
        // new reference, so it must not be closed. Null or
        // `INVALID_HANDLE_VALUE` is filtered before use.
        let handle = unsafe { GetStdHandle(id) };
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: `handle` is a live handle owned by this process, checked
        // just above. Clearing `HANDLE_FLAG_INHERIT` changes only whether a
        // future `CreateProcess` passes it on; it neither closes the handle
        // nor affects I/O through it. A failure leaves one spawn inheriting a
        // handle it should not, which is not worth refusing a start over.
        unsafe {
            let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
        }
    }
}

/// An anonymous job object owning one sheep and everything it spawns.
///
/// Closed on drop. Dropping does not terminate the members: this module's
/// doc covers why `KILL_ON_JOB_CLOSE` is not set.
#[derive(Debug)]
pub(crate) struct Job(HANDLE);

// SAFETY: a job object HANDLE is plain data, not a thread-affine resource;
// Win32 permits any thread to call `AssignProcessToJobObject`,
// `TerminateJobObject` or `CloseHandle` on it. A `Job` moves into the
// per-sheep task that owns its `RunningProcess`.
unsafe impl Send for Job {}
// SAFETY: every Win32 call this module makes on the handle is itself
// thread-safe, and `Job` exposes no interior mutability.
unsafe impl Sync for Job {}

impl Job {
    /// Creates an anonymous job object.
    ///
    /// Anonymous (a null name): a named job is visible in the kernel object
    /// namespace, where another process could open it and terminate the sheep.
    ///
    /// # Errors
    ///
    /// Whatever the OS says if the job cannot be created or its limits set.
    pub(crate) fn create() -> io::Result<Self> {
        // SAFETY: both arguments are the documented "no security attributes,
        // no name" nulls. `CreateJobObjectW` returns a null handle on
        // failure, which is checked immediately below before the value is
        // stored or used.
        let handle = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(handle);

        // Zeroed, then left zeroed: every limit flag stays off. Setting the
        // block explicitly makes adding a limit later a one-line edit at a
        // site that already handles its own errors.
        //
        // `JOB_OBJECT_LIMIT_BREAKAWAY_OK` and
        // `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK` are the two flags whose
        // absence is load-bearing: without them a member cannot escape the
        // job, which is what makes `kill_tree` complete.
        // SAFETY: `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` is a plain Win32 POD
        // struct holding no reference, no `NonNull` and no range-restricted
        // enum, so all-zeroes is a valid inhabitant, and is the value wanted:
        // every limit flag off.
        let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
        // SAFETY: `job.0` is a live job handle, checked non-null above.
        // `JobObjectExtendedLimitInformation` is the class that pairs with
        // `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`, and the pointer and length
        // describe a stack local that outlives the call.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                core::ptr::from_ref(&info).cast(),
                u32::try_from(core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .expect("the size of a fixed Win32 struct fits in u32"),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Assigns an already-spawned process to this job.
    ///
    /// Every process the assigned one creates is a member too, and that
    /// inheritance is what makes this the process-group substitute. A child
    /// that forks a grandchild between the spawn and the assignment leaves it
    /// outside the job; closing that window needs `CREATE_SUSPENDED`, which
    /// `std`'s `Command` cannot express.
    ///
    /// # Errors
    ///
    /// Whatever the OS says. `ERROR_ACCESS_DENIED` usually means shep itself
    /// runs in a job that does not permit breakaway.
    pub(crate) fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: `self.0` is a live job handle for as long as `self` lives,
        // and `process` is the caller's live child-process handle, borrowed
        // from a `tokio::process::Child` that outlives this call. Neither
        // handle is consumed.
        let ok = unsafe { AssignProcessToJobObject(self.0, process as HANDLE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Terminates every process in the job.
    ///
    /// Unblockable, like the `SIGKILL` it stands in for. `exit_code` becomes
    /// the observed exit code of every member.
    ///
    /// # Errors
    ///
    /// Whatever the OS says. A job whose members have all exited is not an
    /// error: this succeeds and does nothing.
    pub(crate) fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: `self.0` is a live job handle for as long as `self` lives.
        let ok = unsafe { TerminateJobObject(self.0, exit_code) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: `self.0` was returned non-null by `CreateJobObjectW` and
        // nothing else in this module closes it; `Job` is neither `Copy` nor
        // `Clone`, so this runs exactly once. A failing `CloseHandle` in a
        // destructor has no caller to report to.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a job cannot be created and terminated on this machine.
    ///
    /// An empty job is the case worth pinning: `kill_tree` runs against a
    /// sheep that may already have exited, so no members must read as success.
    #[test]
    fn an_empty_job_is_created_and_terminated_without_error() {
        let job = Job::create().expect("a job object must be creatable");
        job.terminate(1)
            .expect("terminating a job with no members must succeed");
    }

    /// fails if a real child assigned to a job survives `terminate`.
    ///
    /// The child would otherwise outrun the test, so an exit can only be the
    /// termination.
    #[tokio::test]
    async fn a_child_assigned_to_a_job_is_killed_by_terminate() {
        let job = Job::create().unwrap();
        // `ping`, not `timeout /t`: `timeout.exe` refuses a non-console stdin
        // and exits in milliseconds, so it would assert nothing here.
        let mut child = tokio::process::Command::new("cmd")
            .args(["/C", "ping", "-n", "600", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("cmd must be spawnable on Windows");

        let handle = child
            .raw_handle()
            .expect("a just-spawned child has a raw handle");
        job.assign(handle).expect("assignment must succeed");

        job.terminate(9).unwrap();

        let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .expect("a terminated job member must not outlive its job")
            .unwrap();
        assert!(
            !status.success(),
            "a job-terminated child must not report success"
        );
    }
}
