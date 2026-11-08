//! Process liveness and kill-on-close job objects.

use std::io;
use std::os::windows::io::{AsRawHandle, HandleOrInvalid, HandleOrNull, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};

use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenProcess, OpenThread, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, ResumeThread, THREAD_SUSPEND_RESUME, WaitForSingleObject,
};

/// Whether a process id names a running process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The process is running.
    Alive,
    /// No such process, or it has exited.
    Gone,
    /// The state could not be determined (for example access denied).
    /// Callers treat this as alive.
    Unknown,
}

/// Whether the process `pid` is running. A pid can be reused after the
/// process exits; this only answers for the current owner of the id.
pub fn liveness(pid: u32) -> Liveness {
    if pid == 0 {
        return Liveness::Unknown;
    }
    // SAFETY: OpenProcess has no pointer arguments; it returns null or a
    // new handle that we own.
    let raw = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    // SAFETY: `raw` is null or a handle we own, as `HandleOrNull` requires.
    let handle = match OwnedHandle::try_from(unsafe { HandleOrNull::from_raw_handle(raw) }) {
        Ok(handle) => handle,
        Err(_) => {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                Liveness::Gone
            } else {
                Liveness::Unknown
            };
        }
    };
    // SAFETY: the handle is valid and has SYNCHRONIZE access; a zero
    // timeout only polls.
    match unsafe { WaitForSingleObject(handle.as_raw_handle() as HANDLE, 0) } {
        WAIT_OBJECT_0 => Liveness::Gone,
        WAIT_TIMEOUT => Liveness::Alive,
        _ => Liveness::Unknown,
    }
}

/// A job object whose members are killed when the job is terminated or
/// its last handle is closed. The handle is not inheritable, so dropping
/// the `Job` kills every remaining member, including descendants created
/// after the leader started.
#[derive(Debug)]
pub struct Job {
    handle: OwnedHandle,
}

impl Job {
    fn new() -> io::Result<Job> {
        // SAFETY: null security attributes (not inheritable) and a null
        // name are accepted; the result is null or a new handle we own.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        // SAFETY: `raw` is null or a handle we own.
        let handle = OwnedHandle::try_from(unsafe { HandleOrNull::from_raw_handle(raw) })
            .map_err(|_| io::Error::last_os_error())?;
        // SAFETY: an all-zero JOBOBJECT_EXTENDED_LIMIT_INFORMATION is a
        // valid "no limits" value.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` is the struct this class expects and is
        // readable for its full size.
        let ok = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Job { handle })
    }

    /// Terminate every process in the job with exit code `code`.
    pub fn terminate(&self, code: u32) -> io::Result<()> {
        // SAFETY: the job handle is valid for the lifetime of `self`.
        if unsafe { TerminateJobObject(self.handle.as_raw_handle() as HANDLE, code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn assign(&self, child: &Child) -> io::Result<()> {
        // SAFETY: both handles are valid; the child's process handle is
        // owned by `child` and has the rights CreateProcess grants.
        let ok = unsafe {
            AssignProcessToJobObject(
                self.handle.as_raw_handle() as HANDLE,
                child.as_raw_handle() as HANDLE,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Spawn `cmd` inside a new kill-on-close job before it runs any code.
///
/// The process is created suspended, assigned to the job, and its primary
/// thread is resumed, so every descendant it creates is in the job too
/// (the job does not allow breakaway). On any failure the child is killed
/// and reaped and the error is returned.
///
/// The standard library has no way to read creation flags back from a
/// `Command`, so this sets them to `CREATE_SUSPENDED` (replacing any flags
/// the caller set) and resets them to 0 afterwards. Do not rely on custom
/// creation flags with this function.
pub fn spawn_in_job(cmd: &mut Command) -> io::Result<(Child, Job)> {
    let job = Job::new()?;
    cmd.creation_flags(CREATE_SUSPENDED);
    let spawned = cmd.spawn();
    cmd.creation_flags(0);
    let mut child = spawned?;
    let started = job.assign(&child).and_then(|()| resume_primary(child.id()));
    if let Err(e) = started {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    Ok((child, job))
}

/// Resume the suspended threads of the freshly created process `pid` (a
/// new process has exactly one).
fn resume_primary(pid: u32) -> io::Result<()> {
    // SAFETY: no pointer arguments; returns INVALID_HANDLE_VALUE or a new
    // handle that we own.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    // SAFETY: `raw` is INVALID_HANDLE_VALUE or a handle we own.
    let snapshot = OwnedHandle::try_from(unsafe { HandleOrInvalid::from_raw_handle(raw) })
        .map_err(|_| io::Error::last_os_error())?;
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };
    let mut resumed = 0usize;
    // SAFETY: `entry` is writable and its dwSize is set as required.
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle() as HANDLE, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: no pointer arguments; returns null or a new handle.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            // SAFETY: `thread` is null or a handle we own.
            let thread = OwnedHandle::try_from(unsafe { HandleOrNull::from_raw_handle(thread) })
                .map_err(|_| io::Error::last_os_error())?;
            // SAFETY: the thread handle is valid with THREAD_SUSPEND_RESUME.
            if unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            resumed += 1;
        }
        // SAFETY: as for Thread32First.
        more = unsafe { Thread32Next(snapshot.as_raw_handle() as HANDLE, &mut entry) } != 0;
    }
    if resumed == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the suspended child has no thread to resume",
        ));
    }
    Ok(())
}
