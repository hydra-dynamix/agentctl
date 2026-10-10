use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
};

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in process path",
        ));
    }
    value.push(0);
    Ok(value)
}

/// Launch without inheriting any caller handles. Rust's ordinary spawn can
/// inherit the caller's original stdout pipe even when child stdout is replaced,
/// making a detached launch appear to block until the supervisor exits.
pub(crate) fn spawn_supervisor(
    executable: &Path,
    spec: &Path,
    cwd: &Path,
    log: &Path,
) -> io::Result<u32> {
    // All arguments are file paths ending in a filename, so quoted arguments
    // cannot end in a backslash. Quotes are invalid in Windows file names.
    let mut command = std::ffi::OsString::from("\"");
    command.push(executable);
    command.push("\" __supervise \"");
    command.push(spec);
    command.push("\" --log \"");
    command.push(log);
    command.push("\"");
    let mut command = wide(&command)?;
    let executable = wide(executable.as_os_str())?;
    let cwd = wide(cwd.as_os_str())?;
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        if CreateProcessW(
            executable.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW,
            std::ptr::null(),
            cwd.as_ptr(),
            &startup,
            &mut process,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(process.dwProcessId)
}

/// Initialize a newly launched supervisor's output before it writes anything.
/// The caller must retain the returned file while using stdout/stderr.
#[doc(hidden)]
pub fn redirect_log(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().append(true).open(path)?;
    unsafe {
        if SetStdHandle(STD_OUTPUT_HANDLE, file.as_raw_handle()) == 0
            || SetStdHandle(STD_ERROR_HANDLE, file.as_raw_handle()) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(file)
}

use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

pub(crate) struct Job(OwnedHandle);

impl Job {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        let job = unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            Self(OwnedHandle::from_raw_handle(handle))
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            if SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        // Assign before the first instruction, so even a fast child cannot
        // spawn descendants outside the job between spawn and assignment.
        command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        let mut child = command.spawn()?;
        let result = (|| {
            unsafe {
                if AssignProcessToJobObject(job.0.as_raw_handle(), child.as_raw_handle()) == 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            resume_primary_thread(child.id())
        })();
        if let Err(error) = result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok((child, job))
    }

    pub(crate) fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.0.as_raw_handle(), 1);
        }
    }
}

fn resume_primary_thread(pid: u32) -> io::Result<()> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snapshot = OwnedHandle::from_raw_handle(snapshot);
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut found = Thread32First(snapshot.as_raw_handle(), &mut entry);
        while found != 0 {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let thread = OwnedHandle::from_raw_handle(thread);
                if ResumeThread(thread.as_raw_handle()) == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            found = Thread32Next(snapshot.as_raw_handle(), &mut entry);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "suspended child thread not found",
    ))
}
