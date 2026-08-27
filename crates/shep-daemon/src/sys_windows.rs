//! The Windows counterpart to `sys` (the unix-only module of the same
//! role): a job object per sheep
//!
//! This module exists for one reason — a Win32 job object is what replaces
//! the unix process group — and it is this crate's ONLY unsafe surface on
//! Windows, exactly as `sys.rs` is its only unsafe surface on unix (IR-22,
//! IR-23). Every `unsafe` block below carries its own `// SAFETY:`.
//!
//! # Why a job object, and why it is better than what it replaces
//!
//! `tokio_runner`'s unix arm spawns each sheep with `process_group(0)`, so
//! the sheep leads a group of its own and a negative-pid `SIGKILL` reaches
//! the whole tree. Windows has no process groups in that sense. It has job
//! objects: a kernel container a process is assigned to, which its children
//! inherit automatically, and which can be terminated as a unit.
//!
//! The substitution is close to exact, and in one respect stronger.
//! `kill.rs`'s own module comment records a hole in the unix design — a
//! grandchild that calls `setsid` escapes its group and outlives
//! `kill_tree`. A job has no equivalent hole: a process cannot leave a job
//! it has been assigned to, and a child can only be spawned outside it if
//! the job itself was created with `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, which
//! `Job::create` deliberately does not set. So `kill_tree` on Windows
//! reaches strictly more of the tree than its unix twin does.
//!
//! # What is deliberately NOT set
//!
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` would terminate every member the
//! moment the last handle to the job closed — which, since the daemon holds
//! that handle, means the whole flock would die with the shepherd.
//!
//! **That is not shep's behaviour on unix and it is not adopted here.** A
//! sheep whose shepherd is `SIGKILL`ed keeps running, orphaned; the flag
//! would make Windows the one platform where restarting the daemon also
//! stops every app it was supervising. Aligning the two platforms is worth
//! more than the flag's one genuine benefit (no orphan can outlive the
//! daemon and be duplicated by the next one's muster restore), and that
//! hazard is one shep already has, already documents, and should fix the
//! same way on both platforms if it fixes it at all.
//!
//! # Reporting the environment the job is created in
//!
//! Everything above describes a job shep creates at the top of the tree. It
//! is not always at the top: a CI runner, a container shim or a terminal host
//! may already have put shep itself into a job, and a job created inside
//! another one is a *nested* job with its own rules — the outer job's limit
//! flags constrain it, and Windows before 8 had no nesting at all.
//!
//! [`job_environment`] and [`probe_nested_job`] report those facts instead of
//! assuming them. They exist for `tests/real_runner_windows.rs`'s
//! `job_object_environment_reports_itself`, which prints what they find and
//! asserts nothing: it is an instrument for explaining why containment
//! behaves differently on one machine than another, not a claim about either.

#![allow(unsafe_code)] // IR-24 exception — the Windows twin of `sys.rs`'s, and
// the only one on this platform. Sixteen sites, all in this file: fourteen
// `unsafe` blocks plus the two `unsafe impl`s that make a kernel handle `Send`
// and `Sync`. Of the blocks, eleven are FFI calls (`CreateJobObjectW`,
// `SetInformationJobObject`, `AssignProcessToJobObject`, `TerminateJobObject`,
// `CloseHandle`, `GetStdHandle`, `SetHandleInformation`, `GetCurrentProcess` +
// `IsProcessInJob` sharing one block, `QueryInformationJobObject` twice, and
// `RtlGetVersion`), two are `mem::zeroed` for a Win32 limit struct, and one is
// a `ptr::read` of a kernel-filled variable-length header. Nothing outside
// this module writes `unsafe` on Windows.

use std::io;
use std::os::windows::io::RawHandle;

use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_MORE_DATA, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_AFFINITY, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PRESERVE_JOB_TIME, JOB_OBJECT_LIMIT_PRIORITY_CLASS,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
    JOB_OBJECT_LIMIT_SCHEDULING_CLASS, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_SUBSET_AFFINITY, JOB_OBJECT_LIMIT_WORKINGSET, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Marks this process's own stdio handles non-inheritable.
///
/// **The Windows counterpart to `shep-cli`'s `seal_inherited_fds`, and it is
/// needed for the same reason that one is.** `launch.rs`'s Windows arm used
/// to carry a comment claiming no counterpart was necessary, on the grounds
/// that Windows inherits only handles explicitly marked inheritable. That is
/// true and is not the whole story: `CreateProcess` with `bInheritHandles =
/// TRUE` — which `std` passes whenever any stdio is redirected — inherits
/// **every** inheritable handle the parent holds, not only the ones `std`
/// prepared.
///
/// So a shepherd spawned from a shell that gave `shep` a PIPE for stdout
/// inherits that pipe, and holds it open for its entire life. The daemon
/// never writes to it (its own stdout is redirected to a log file), but the
/// pipe cannot close while a handle to it exists, so the shell blocks
/// forever waiting for output that will never end. Measured: `shep start |
/// Out-Null` hung indefinitely while `shep start` alone returned at once,
/// and `shep flock | Out-Null` — which spawns nothing — was unaffected.
///
/// That is the whole bug, and it is worse than it sounds because piping is
/// the normal case in the place it matters most: a CI job, a
/// `$(shep start ...)`, anything reading shep's output.
///
/// Called immediately before the spawn rather than once at startup: it
/// changes only the INHERIT flag, not the handles themselves, so this
/// process goes on using its own stdio exactly as before — the same
/// narrowness `seal_inherited_fds` has on unix, where it sets `FD_CLOEXEC`
/// and leaves the descriptors open.
///
/// Best-effort by design. A failure here means one spawn may inherit a
/// handle it should not, which is the behaviour that existed before this
/// function; refusing to start a shepherd over it would be the worse
/// outcome, so there is nothing to report and nothing for a caller to do.
pub fn seal_std_handles() {
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` takes one of the three documented standard
        // handle ids and returns a borrowed handle this process already
        // owns — it is not a new reference and must not be closed. It
        // returns `INVALID_HANDLE_VALUE` or null for a stream this process
        // does not have, both of which are filtered before use.
        let handle = unsafe { GetStdHandle(id) };
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: `handle` is a live handle owned by this process (checked
        // valid just above). Clearing `HANDLE_FLAG_INHERIT` changes only
        // whether a future `CreateProcess` passes it on; it neither closes
        // the handle nor affects any I/O this process performs through it.
        // The return value is ignored deliberately — see this function's
        // own doc for why a failure is not worth reporting.
        unsafe {
            let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
        }
    }
}

/// An anonymous job object owning one sheep and everything it spawns.
///
/// Closed on drop. Dropping does **not** terminate the members — see this
/// module's doc for why `KILL_ON_JOB_CLOSE` is not set.
#[derive(Debug)]
pub(crate) struct Job(HANDLE);

// SAFETY: a job object HANDLE is a kernel handle, not a thread-affine
// resource. Win32 permits any thread to call `AssignProcessToJobObject`,
// `TerminateJobObject` or `CloseHandle` on it, and the handle value is
// plain data. The daemon moves a `Job` into the per-sheep task that owns
// its `RunningProcess`, which is `Send`, so this impl is required and is
// sound for the same reason `std`'s own process handles are `Send`.
unsafe impl Send for Job {}
// SAFETY: as above; every Win32 call this module makes on the handle is
// itself thread-safe, and `Job` exposes no interior mutability.
unsafe impl Sync for Job {}

impl Job {
    /// Creates an anonymous job object.
    ///
    /// Anonymous (a null name) on purpose: a *named* job would be visible in
    /// the kernel object namespace, where another process could open it by
    /// name and terminate the sheep. Nothing needs to find this job except
    /// the daemon that created it, and the daemon holds the handle.
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

        // Zeroed, then left zeroed: every limit flag stays off. This call is
        // not strictly required today, since a job with no limits is exactly
        // what a default-constructed one already is — it is made anyway so
        // that the limit block is a real, named thing in this file rather
        // than an implicit default, and so that adding a limit later is a
        // one-line edit at a site that already handles its own errors.
        //
        // `JOB_OBJECT_LIMIT_BREAKAWAY_OK` and
        // `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK` are the two flags whose
        // ABSENCE is load-bearing: without them a member cannot escape the
        // job, which is what makes `kill_tree` complete. See the module doc.
        // SAFETY: `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` is a plain Win32
        // POD struct — nested `LARGE_INTEGER`/`IO_COUNTERS`/`SIZE_T` fields
        // and nothing else. It holds no reference, no `NonNull` and no enum
        // with a restricted range, so all-zeroes is a valid inhabitant, and
        // is additionally the exact value wanted here: every limit flag off.
        let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
        // SAFETY: `job.0` is a live job handle (checked non-null above).
        // `JobObjectExtendedLimitInformation` is the information class that
        // pairs with `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`, and the pointer
        // and length describe exactly that stack local, which outlives the
        // call. On failure the `Job` is dropped by the `?`, closing the
        // handle.
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
    /// Every process the assigned one goes on to create is a member too,
    /// automatically and unavoidably — that inheritance is what makes this
    /// the process-group substitute.
    ///
    /// # Race window
    ///
    /// There is a real one, and it is worth naming rather than implying it
    /// away: the child is spawned first and assigned immediately after, so a
    /// child that forks a grandchild in the microseconds before the
    /// assignment lands leaves that grandchild outside the job. Closing it
    /// entirely needs the child created suspended (`CREATE_SUSPENDED`) and
    /// resumed after assignment, which `std`'s `Command` cannot express and
    /// which would mean re-implementing `CreateProcessW` here. The window is
    /// the same shape as the unix arm's — `process_group(0)` is applied in
    /// the child between `fork` and `exec`, so it is narrower there — and it
    /// is not one an app hits by accident.
    ///
    /// # Errors
    ///
    /// Whatever the OS says. `ERROR_ACCESS_DENIED` here typically means the
    /// process is already in a job that does not permit breakaway, which on
    /// a developer machine most often means shep itself is running under one
    /// (some CI runners and terminal hosts do this).
    pub(crate) fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: `self.0` is a live job handle for as long as `self` lives.
        // `process` is the caller's live child-process handle, borrowed from
        // a `tokio::process::Child` that outlives this call — the runner
        // assigns immediately after spawn and holds the `Child` afterwards.
        // Neither handle is consumed by this call.
        let ok = unsafe { AssignProcessToJobObject(self.0, process as HANDLE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Terminates every process in the job.
    ///
    /// The `kill_tree` rung: unblockable, exactly like the `SIGKILL` it
    /// stands in for. `exit_code` becomes the observed exit code of every
    /// member.
    ///
    /// # Errors
    ///
    /// Whatever the OS says. A job whose members have all already exited is
    /// **not** an error — this succeeds and does nothing, which is the same
    /// shape as `SIGKILL`ing an already-dead group.
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
        // has not been closed — nothing else in this module closes it, and
        // `Job` is neither `Copy` nor `Clone`, so this runs exactly once.
        // The return value is ignored deliberately: a failing `CloseHandle`
        // in a destructor has no caller to report to, and the process is
        // either exiting or leaking one handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Pairs each job limit flag with its own identifier as a string.
///
/// Written as a macro rather than as a hand-kept table of `(FLAG, "FLAG")`
/// pairs so a bit and its label cannot drift apart. The entire value of the
/// report below is that a reader trusts the names in it.
macro_rules! named_flags {
    ($($flag:ident),+ $(,)?) => { &[$(($flag, stringify!($flag))),+] };
}

/// Every `JOBOBJECT_BASIC_LIMIT_INFORMATION::LimitFlags` bit, by name.
///
/// The whole documented set, not just the four that motivated the report: an
/// unexpected flag is exactly the sort of thing this is looking for, and one
/// that decoded as a bare number would be the one nobody chased.
const KNOWN_LIMIT_FLAGS: &[(u32, &str)] = named_flags![
    JOB_OBJECT_LIMIT_WORKINGSET,
    JOB_OBJECT_LIMIT_PROCESS_TIME,
    JOB_OBJECT_LIMIT_JOB_TIME,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_AFFINITY,
    JOB_OBJECT_LIMIT_PRIORITY_CLASS,
    JOB_OBJECT_LIMIT_PRESERVE_JOB_TIME,
    JOB_OBJECT_LIMIT_SCHEDULING_CLASS,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_SUBSET_AFFINITY,
];

/// How many member pids [`probe_nested_job`] will read back from a job.
///
/// A shep job holds a sheep and its lambs, which is a handful; this is far
/// more than that so the list is never silently truncated in the one case the
/// report exists to explain. An overflow is still reported rather than hidden
/// — see [`JobMembers::assigned`].
const MEMBER_PID_CAPACITY: usize = 256;

/// What `RtlGetVersion` reports this Windows to be.
///
/// The build number is the load-bearing field: a GitHub Actions
/// `windows-latest` image and a developer machine can differ by thousands of
/// builds while both answering "Windows".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsVersion {
    /// `dwMajorVersion` — 10 for both Windows 10 and Windows 11.
    pub major: u32,
    /// `dwMinorVersion`.
    pub minor: u32,
    /// `dwBuildNumber`, the only field that distinguishes a Windows 11 or a
    /// Server image from a Windows 10 one.
    pub build: u32,
}

impl core::fmt::Display for WindowsVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.build)
    }
}

/// A job's `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`, decoded.
#[derive(Debug, Clone)]
pub struct JobLimits {
    /// `LimitFlags` exactly as the kernel reported it, so a bit this build
    /// does not know a name for is still visible.
    pub limit_flags: u32,
    /// The name of every bit set in [`Self::limit_flags`] that this module
    /// has a name for — the whole documented set — in ascending bit order.
    pub named_flags: Vec<&'static str>,
    /// `ActiveProcessLimit`, and `None` unless
    /// `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` is set — the field is meaningless
    /// otherwise, and reporting its residual value would invent a limit.
    pub active_process_limit: Option<u32>,
    /// `JobMemoryLimit` in bytes, `None` unless `JOB_OBJECT_LIMIT_JOB_MEMORY`
    /// is set.
    pub job_memory_limit: Option<usize>,
    /// `ProcessMemoryLimit` in bytes, `None` unless
    /// `JOB_OBJECT_LIMIT_PROCESS_MEMORY` is set.
    pub process_memory_limit: Option<usize>,
}

/// The pids a job currently contains.
#[derive(Debug, Clone)]
pub struct JobMembers {
    /// `NumberOfAssignedProcesses` — how many the kernel says are in the job.
    ///
    /// Larger than [`Self::pids`] when the job holds more members than the
    /// query buffer had room for, which is the only way that list is ever
    /// short and is why this count is reported beside it.
    pub assigned: u32,
    /// The pids that fit in the query buffer.
    pub pids: Vec<u32>,
}

/// The Win32 job-object environment this process is *already* running in.
///
/// Produced by [`job_environment`].
#[derive(Debug)]
pub struct JobEnvironment {
    /// `IsProcessInJob` against a null job handle: whether this process
    /// belongs to any job at all, or the OS error the question failed with.
    pub in_job: io::Result<bool>,
    /// The enclosing job's limits, and `None` when there is no enclosing job
    /// to ask about.
    ///
    /// These are the flags that constrain what a job created *inside* this
    /// one may do, which is the whole reason the field is here.
    pub outer_limits: Option<io::Result<JobLimits>>,
    /// What `RtlGetVersion` said, or the raw `NTSTATUS` it refused with.
    pub version: Result<WindowsVersion, i32>,
}

/// A job created *inside* this process's own job, with a live process
/// assigned to it, held open so a caller can look inside it and then
/// terminate it.
///
/// Deliberately not a one-shot "create, assign, terminate and report":
/// job membership propagates only to processes created *after* the
/// assignment, so a probe that assigned late would find a grandchild missing
/// on every machine and prove nothing. Handing the job back open lets a
/// caller assign at the moment `tokio_runner` does — immediately after spawn
/// — and inspect afterwards.
#[derive(Debug)]
pub struct NestedJobProbe {
    /// The live job, kept for [`Self::members`] and [`Self::terminate`], and
    /// `None` when creation failed.
    job: Option<Job>,
    /// What `CreateJobObjectW` said, through this module's own `Job::create`.
    pub create: io::Result<()>,
    /// What `AssignProcessToJobObject` said. `None` when no job was created.
    ///
    /// `ERROR_ACCESS_DENIED` here is the shape a nesting refusal takes.
    pub assign: Option<io::Result<()>>,
}

impl NestedJobProbe {
    /// Reads back which pids the nested job contains right now.
    ///
    /// **The sharpest fact available.** A grandchild that survives
    /// [`Self::terminate`] either never joined the job or was not killed by
    /// it, and only this distinguishes the two: a missing pid here means
    /// containment failed at inheritance, not at termination.
    ///
    /// `None` when there is no job to look inside.
    ///
    /// # Errors
    ///
    /// Whatever the OS says if the membership query is refused.
    #[must_use]
    pub fn members(&self) -> Option<io::Result<JobMembers>> {
        self.job.as_ref().map(|job| query_members(job.0))
    }

    /// Terminates the nested job, and everything the kernel considers a
    /// member of it.
    ///
    /// The exit code is 137, the number `kill_tree` uses, so a survivor's
    /// exit code stays recognisable. `None` when there is no job.
    ///
    /// # Errors
    ///
    /// Whatever the OS says. A job with no live members is not an error.
    pub fn terminate(&self) -> Option<io::Result<()>> {
        self.job.as_ref().map(|job| job.terminate(137))
    }
}

/// Reports the job object this process is already inside, if any.
///
/// Answers three questions a containment failure turns on: is shep itself in
/// a job, what does that job forbid, and which Windows is deciding. Nothing
/// in the daemon's own paths calls this — it is an instrument, and it makes
/// no claim about whether any of what it finds is a problem.
#[must_use]
pub fn job_environment() -> JobEnvironment {
    let mut in_job_flag: windows_sys::core::BOOL = 0;
    // SAFETY: `GetCurrentProcess` returns the current-process pseudo-handle,
    // which is always valid, is not a new reference and must not be closed.
    // A null `jobhandle` is the documented spelling of "any job at all", and
    // the out-pointer addresses a live `i32` local that outlives the call.
    let queried = unsafe {
        IsProcessInJob(
            GetCurrentProcess(),
            core::ptr::null_mut(),
            &raw mut in_job_flag,
        )
    };
    let in_job = if queried == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(in_job_flag != 0)
    };

    // A null job handle means "the job the calling process belongs to", so
    // the outer job can be inspected without ever holding a handle to it.
    // Skipped entirely when there is no such job: the call would fail with
    // ERROR_ACCESS_DENIED and that error would read as a finding.
    let outer_limits = matches!(in_job, Ok(true)).then(|| query_limits(core::ptr::null_mut()));

    JobEnvironment {
        in_job,
        outer_limits,
        version: windows_version(),
    }
}

/// Creates a job inside whatever job this process is already in and assigns
/// `process` to it — `tokio_runner`'s spawn-time containment sequence, with a
/// report where its `?`s are.
///
/// Call it immediately after spawning `process`, for the reason
/// [`NestedJobProbe`] gives. Nothing is terminated until the caller asks.
///
/// # Handle validity
///
/// `process` is handed to the kernel, which validates it: a stale or bogus
/// value comes back as an OS error in [`NestedJobProbe::assign`] rather than
/// as undefined behaviour, which is why this is a safe function. A value that
/// happens to name some *other* live object is a caller error the kernel
/// cannot catch — pass a handle borrowed from a `Child` that outlives the
/// call, exactly as this module's own `Job::assign` requires.
#[must_use]
pub fn probe_nested_job(process: RawHandle) -> NestedJobProbe {
    match Job::create() {
        Ok(job) => {
            let assign = job.assign(process);
            NestedJobProbe {
                job: Some(job),
                create: Ok(()),
                assign: Some(assign),
            }
        }
        Err(error) => NestedJobProbe {
            job: None,
            create: Err(error),
            assign: None,
        },
    }
}

/// Reads and decodes a job's extended limit information.
///
/// `job` may be null, which Win32 reads as "the job the calling process
/// belongs to".
fn query_limits(job: HANDLE) -> io::Result<JobLimits> {
    // SAFETY: `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` is a plain Win32 POD
    // struct — the same argument `Job::create` makes for the same type — so
    // all-zeroes is a valid inhabitant. It is overwritten by the call below
    // and only read when that call reports success.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: `job` is either null (documented above) or a live job handle
    // owned by the caller. The information class matches the struct type, the
    // pointer and length describe exactly that stack local which outlives the
    // call, and a null `lpReturnLength` is documented as "do not report it".
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            core::ptr::from_mut(&mut info).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .expect("the size of a fixed Win32 struct fits in u32"),
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let basic = info.BasicLimitInformation;
    let flags = basic.LimitFlags;
    let set = |flag: u32| flags & flag != 0;
    Ok(JobLimits {
        limit_flags: flags,
        named_flags: KNOWN_LIMIT_FLAGS
            .iter()
            .filter(|(bit, _)| set(*bit))
            .map(|(_, name)| *name)
            .collect(),
        active_process_limit: set(JOB_OBJECT_LIMIT_ACTIVE_PROCESS)
            .then_some(basic.ActiveProcessLimit),
        job_memory_limit: set(JOB_OBJECT_LIMIT_JOB_MEMORY).then_some(info.JobMemoryLimit),
        process_memory_limit: set(JOB_OBJECT_LIMIT_PROCESS_MEMORY)
            .then_some(info.ProcessMemoryLimit),
    })
}

/// Reads back the pids a job currently contains.
fn query_members(job: HANDLE) -> io::Result<JobMembers> {
    // `JOBOBJECT_BASIC_PROCESS_ID_LIST` is a variable-length struct: a header
    // followed by however many pids fit. The buffer is a `Vec<usize>` rather
    // than the more obvious `Vec<u8>` because a byte vector is only 1-aligned
    // and this struct needs pointer alignment — reading a header out of an
    // under-aligned allocation would be undefined behaviour rather than a
    // wrong answer, which is a poor trade for an instrument.
    const HEADER_WORDS: usize =
        core::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList) / size_of::<usize>();
    // Win32 spelling of "your buffer was too small"; the header and as many
    // pids as fit are still written, so it is a partial answer, not a failure.
    const MORE_DATA: i32 = ERROR_MORE_DATA as i32;

    let mut buffer = vec![0_usize; HEADER_WORDS + MEMBER_PID_CAPACITY];
    let bytes = u32::try_from(size_of_val(buffer.as_slice()))
        .expect("a buffer sized in this file fits in u32");
    // SAFETY: `job` is a live job handle owned by the caller.
    // `JobObjectBasicProcessIdList` pairs with the buffer being passed, whose
    // pointer and length describe exactly `buffer`'s live allocation — which
    // is `usize`-aligned, as the struct requires — and which outlives the
    // call. A null `lpReturnLength` is documented as "do not report it".
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            buffer.as_mut_ptr().cast(),
            bytes,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(MORE_DATA) {
            return Err(error);
        }
    }

    // SAFETY: the call above reported success (or `ERROR_MORE_DATA`, which
    // still fills the header), so the kernel has written a
    // `JOBOBJECT_BASIC_PROCESS_ID_LIST` at the start of `buffer`. The
    // allocation is large enough for the fixed header and correctly aligned
    // for it, and the type is `Copy` with no padding invariant, so reading a
    // copy out of it is a plain load.
    let header = unsafe {
        buffer
            .as_ptr()
            .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()
            .read()
    };
    let returned = usize::try_from(header.NumberOfProcessIdsInList)
        .unwrap_or(MEMBER_PID_CAPACITY)
        .min(MEMBER_PID_CAPACITY);
    Ok(JobMembers {
        assigned: header.NumberOfAssignedProcesses,
        pids: buffer[HEADER_WORDS..HEADER_WORDS + returned]
            .iter()
            .map(|pid| u32::try_from(*pid).unwrap_or(u32::MAX))
            .collect(),
    })
}

/// Asks ntdll what this Windows actually is.
///
/// `RtlGetVersion` rather than `GetVersionExW`: the latter is shimmed by
/// application-compatibility and reports 6.2 to a binary without a manifest
/// declaring newer support, which would make a runner image and a laptop
/// indistinguishable in the one report meant to tell them apart.
fn windows_version() -> Result<WindowsVersion, i32> {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: u32::try_from(size_of::<OSVERSIONINFOW>())
            .expect("the size of a fixed Win32 struct fits in u32"),
        ..Default::default()
    };
    // SAFETY: the pointer addresses a live, correctly aligned `OSVERSIONINFOW`
    // local whose `dwOSVersionInfoSize` is set to its own size, which is the
    // contract this call has for knowing how much it may write. The local
    // outlives the call and nothing else aliases it.
    let status = unsafe { RtlGetVersion(&raw mut info) };
    if status < 0 {
        return Err(status);
    }
    Ok(WindowsVersion {
        major: info.dwMajorVersion,
        minor: info.dwMinorVersion,
        build: info.dwBuildNumber,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a job cannot be created and terminated on this machine.
    ///
    /// Terminating an EMPTY job is the case worth pinning: `kill_tree` runs
    /// unconditionally at the top of the ladder's second rung, including
    /// against a sheep that already exited, so "no members" must read as
    /// success rather than as an error the ladder would log.
    #[test]
    fn an_empty_job_is_created_and_terminated_without_error() {
        let job = Job::create().expect("a job object must be creatable");
        job.terminate(1)
            .expect("terminating a job with no members must succeed");
    }

    /// fails if a real child assigned to a job survives `terminate`.
    ///
    /// The load-bearing assertion of this module: this is `kill_tree`. Uses
    /// a child that would otherwise run far longer than the test, so an exit
    /// can only be the termination.
    #[tokio::test]
    async fn a_child_assigned_to_a_job_is_killed_by_terminate() {
        let job = Job::create().unwrap();
        // `ping`, not `timeout /t`: `timeout.exe` refuses a non-console stdin
        // and exits in milliseconds, so it would assert nothing here. Same
        // choice, same measurement, as `real_runner_windows.rs`'s
        // `LONG_RUNNING`.
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

    /// fails if the limit decoder invents a flag on a job that has none.
    ///
    /// `Job::create` sets an all-zero limit block on purpose, so this is the
    /// one case where the right answer is known without asking the OS: any
    /// name at all in `named_flags` here, or any `Some` limit, means the
    /// decoder is reading the wrong bytes — which would make the whole report
    /// confidently wrong rather than visibly broken.
    #[test]
    fn a_fresh_jobs_limits_decode_as_no_limits_at_all() {
        let job = Job::create().unwrap();
        let limits = query_limits(job.0).expect("a live job must answer a limit query");

        assert_eq!(limits.limit_flags, 0, "{limits:?}");
        assert!(limits.named_flags.is_empty(), "{limits:?}");
        assert_eq!(limits.active_process_limit, None);
        assert_eq!(limits.job_memory_limit, None);
        assert_eq!(limits.process_memory_limit, None);
    }

    /// fails if the member-pid readback misses a process that is provably in
    /// the job.
    ///
    /// The riskiest new code in this file: `JOBOBJECT_BASIC_PROCESS_ID_LIST`
    /// is variable-length, so the pid array's offset is computed rather than
    /// obvious, and an off-by-one there reads garbage or zeroes. A wrong
    /// answer would read as "the grandchild never joined the job", which is
    /// exactly the conclusion the report exists to support — so it must not
    /// be reachable by a bug here.
    #[tokio::test]
    async fn a_jobs_member_list_names_the_child_assigned_to_it() {
        let job = Job::create().unwrap();
        let mut child = tokio::process::Command::new("cmd")
            .args(["/C", "ping", "-n", "600", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("cmd must be spawnable on Windows");
        let pid = child.id().expect("a just-spawned child has a pid");
        job.assign(
            child
                .raw_handle()
                .expect("a just-spawned child has a raw handle"),
        )
        .expect("assignment must succeed");

        let members = query_members(job.0).expect("a live job must answer a membership query");

        job.terminate(9).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await;

        assert!(
            members.pids.contains(&pid),
            "the job must name the child assigned to it, got {members:?} for pid {pid}"
        );
        assert!(members.assigned >= 1, "{members:?}");
    }

    /// fails if the environment report contradicts itself.
    ///
    /// Says nothing about what this machine's answer *is* — that is the
    /// report's business and varies per machine. What it pins is the one
    /// relationship the caller relies on: limits are present exactly when
    /// there is a job to have them, so a `None` can be read as "no enclosing
    /// job" rather than as "the query was skipped for some other reason".
    #[test]
    fn the_environment_report_only_carries_limits_when_there_is_a_job() {
        let environment = job_environment();
        assert_eq!(
            matches!(environment.in_job, Ok(true)),
            environment.outer_limits.is_some(),
            "{environment:?}"
        );
        assert!(
            environment.version.is_ok(),
            "RtlGetVersion does not fail on any Windows that can run this: {environment:?}"
        );
    }
}
