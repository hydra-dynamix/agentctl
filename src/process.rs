use std::io;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};

#[cfg(unix)]
use std::sync::OnceLock;
#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};

#[cfg(unix)]
use signal_hook::consts::signal::{SIGCHLD, SIGHUP, SIGINT, SIGQUIT, SIGTERM};
#[cfg(unix)]
use signal_hook::iterator::Signals;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(unix)]
use std::time::{Duration, Instant};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

#[cfg(unix)]
const TERMINATION_GRACE: Duration = Duration::from_millis(750);

#[cfg(unix)]
static ACTIVE_PROCESS_GROUP: AtomicI32 = AtomicI32::new(0);
#[cfg(unix)]
static SIGNAL_FORWARDER: OnceLock<()> = OnceLock::new();

/// A child that owns an isolated process group. Waiting for the command also
/// terminates descendants which outlive their group leader. Dropping it on an
/// error follows the same TERM-then-KILL cleanup path and always reaps the
/// direct child.
pub(crate) struct ManagedChild {
    child: Child,
    #[cfg(unix)]
    process_group: i32,
    #[cfg(windows)]
    job: HANDLE,
    reaped: bool,
}

/// Spawn a detached supervisor as its own process-group leader. The supervisor
/// is responsible for recording completion and terminating its group.
pub(crate) fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    #[cfg(windows)]
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    #[cfg(not(windows))]
    configure_process_group(command);
    command.spawn()
}

impl ManagedChild {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        enable_subreaper()?;
        configure_process_group(command);
        install_signal_forwarder()?;
        #[cfg(unix)]
        ACTIVE_PROCESS_GROUP.store(-1, Ordering::SeqCst);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                #[cfg(unix)]
                ACTIVE_PROCESS_GROUP.store(0, Ordering::SeqCst);
                return Err(error);
            }
        };

        #[cfg(unix)]
        {
            let process_group = child.id() as i32;
            ACTIVE_PROCESS_GROUP.store(process_group, Ordering::SeqCst);
            Ok(Self {
                child,
                process_group,
                reaped: false,
            })
        }

        #[cfg(windows)]
        {
            let mut child = child;
            let job = match create_kill_on_close_job(&child) {
                Ok(job) => job,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            Ok(Self {
                child,
                job,
                reaped: false,
            })
        }

        #[cfg(not(any(unix, windows)))]
        Ok(Self {
            child,
            reaped: false,
        })
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait()?;
        self.reaped = true;
        self.clean_remaining_descendants();
        self.clear_active_group();
        Ok(status)
    }

    fn clean_remaining_descendants(&mut self) {
        #[cfg(unix)]
        terminate_process_tree(self.process_group);
        #[cfg(windows)]
        if !self.job.is_null() {
            unsafe {
                TerminateJobObject(self.job, 1);
                CloseHandle(self.job);
            }
            self.job = std::ptr::null_mut();
        }
    }

    fn clear_active_group(&self) {
        #[cfg(unix)]
        {
            let _ = ACTIVE_PROCESS_GROUP.compare_exchange(
                self.process_group,
                0,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }

        self.clean_remaining_descendants();
        #[cfg(not(any(unix, windows)))]
        let _ = self.child.kill();

        let _ = self.child.wait();
        self.reaped = true;
        self.clear_active_group();
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    #[cfg(not(windows))]
    let _ = command;
}

#[cfg(windows)]
fn create_kill_on_close_job(child: &Child) -> io::Result<HANDLE> {
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&information as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        let error = io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
        }
        return Err(error);
    }

    let process = child.as_raw_handle() as HANDLE;
    if unsafe { AssignProcessToJobObject(job, process) } == 0 {
        let error = io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
        }
        return Err(error);
    }
    Ok(job)
}

#[cfg(unix)]
fn install_signal_forwarder() -> io::Result<()> {
    if SIGNAL_FORWARDER.get().is_some() {
        return Ok(());
    }

    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT, SIGCHLD])?;
    SIGNAL_FORWARDER.get_or_init(|| {
        std::thread::spawn(move || {
            for signal in signals.forever() {
                let process_group = ACTIVE_PROCESS_GROUP.load(Ordering::SeqCst);
                if signal == SIGCHLD {
                    if process_group > 0 {
                        reap_adopted_children_except(process_group);
                    }
                    continue;
                }
                if process_group > 0 {
                    // Negative PIDs address the complete process group.
                    unsafe {
                        libc::kill(-process_group, signal);
                    }
                } else {
                    // Do not turn agentctl into a process that ignores Ctrl-C
                    // while no managed child is active.
                    let _ = signal_hook::low_level::emulate_default_handler(signal);
                }
            }
        });
    });
    Ok(())
}

#[cfg(target_os = "linux")]
fn enable_subreaper() -> io::Result<()> {
    let result = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn enable_subreaper() -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn install_signal_forwarder() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn terminate_process_tree(process_group: i32) {
    if process_group <= 0 {
        return;
    }

    unsafe {
        libc::kill(-process_group, SIGTERM);
    }
    signal_adopted_descendants(SIGTERM);
    let deadline = Instant::now() + TERMINATION_GRACE;
    while Instant::now() < deadline {
        reap_adopted_children();
        signal_adopted_descendants(SIGTERM);
        if !process_tree_exists(process_group) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    signal_adopted_descendants(libc::SIGKILL);
    let kill_deadline = Instant::now() + TERMINATION_GRACE;
    while Instant::now() < kill_deadline {
        reap_adopted_children();
        signal_adopted_descendants(libc::SIGKILL);
        if !process_tree_exists(process_group) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    reap_adopted_children();
}

#[cfg(unix)]
fn process_tree_exists(process_group: i32) -> bool {
    process_group_exists(process_group) || adopted_children_exist()
}

#[cfg(unix)]
fn process_group_exists(process_group: i32) -> bool {
    let result = unsafe { libc::kill(-process_group, 0) };
    if result == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "linux")]
fn reap_adopted_children() {
    loop {
        let mut status = 0;
        let result = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if result <= 0 {
            break;
        }
    }
}

#[cfg(target_os = "linux")]
fn reap_adopted_children_except(excluded_pid: i32) {
    for pid in linux_child_pids(std::process::id() as i32) {
        if pid == excluded_pid {
            continue;
        }
        let mut status = 0;
        unsafe {
            libc::waitpid(pid, &mut status, libc::WNOHANG);
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn reap_adopted_children() {}

#[cfg(all(unix, not(target_os = "linux")))]
fn reap_adopted_children_except(_excluded_pid: i32) {}

#[cfg(target_os = "linux")]
fn adopted_children_exist() -> bool {
    !linux_child_pids(std::process::id() as i32).is_empty()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn adopted_children_exist() -> bool {
    false
}

#[cfg(target_os = "linux")]
fn signal_adopted_descendants(signal: i32) {
    let mut pending = linux_child_pids(std::process::id() as i32);
    while let Some(pid) = pending.pop() {
        pending.extend(linux_child_pids(pid));
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn signal_adopted_descendants(_signal: i32) {}

#[cfg(target_os = "linux")]
fn linux_child_pids(pid: i32) -> Vec<i32> {
    let path = format!("/proc/{pid}/task/{pid}/children");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|value| value.parse().ok())
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::sync::Mutex;

    static PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    #[cfg(unix)]
    fn wait_terminates_descendants_left_by_the_direct_child() {
        let _test_guard = PROCESS_TEST_LOCK.lock().expect("process test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let pid_path = temp.path().join("descendant.pid");
        let script = format!(
            "setsid sleep 30 & printf '%s' $! > '{}'; exit 0",
            pid_path.display()
        );
        let mut command = Command::new("bash");
        command
            .args(["-c", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = ManagedChild::spawn(&mut command).expect("spawn managed child");
        let status = child.wait().expect("wait managed child");
        assert!(status.success());

        let pid = std::fs::read_to_string(pid_path)
            .expect("descendant pid")
            .parse::<i32>()
            .expect("numeric pid");
        let running = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!running, "descendant {pid} survived its agent command");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn reaps_adopted_descendant_while_direct_child_is_running() {
        let _test_guard = PROCESS_TEST_LOCK.lock().expect("process test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let orphan_pid_path = temp.path().join("orphan.pid");
        let release_path = temp.path().join("release");
        let script = r#"
            sh -c 'sleep 0.05 & printf "%s" "$!" > "$1"' agentctl-inner "$1"
            while [ ! -e "$2" ]; do sleep 0.02; done
        "#;
        let mut command = Command::new("bash");
        command
            .args([
                "-c",
                script,
                "agentctl-test",
                orphan_pid_path.to_str().expect("UTF-8 orphan pid path"),
                release_path.to_str().expect("UTF-8 release path"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = ManagedChild::spawn(&mut command).expect("spawn managed child");
        let direct_pid = child.child.id() as i32;
        let waiter = std::thread::spawn(move || child.wait());

        let pid_deadline = Instant::now() + Duration::from_secs(2);
        while !orphan_pid_path.is_file() && Instant::now() < pid_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let orphan_pid = std::fs::read_to_string(&orphan_pid_path)
            .expect("orphan pid file")
            .parse::<i32>()
            .expect("numeric orphan pid");

        let reap_deadline = Instant::now() + Duration::from_secs(2);
        let mut orphan_reaped = false;
        while Instant::now() < reap_deadline {
            if unsafe { libc::kill(orphan_pid, 0) } != 0 {
                orphan_reaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let direct_child_was_running = unsafe { libc::kill(direct_pid, 0) } == 0;

        std::fs::write(&release_path, b"release").expect("release direct child");
        let status = waiter
            .join()
            .expect("waiter thread")
            .expect("wait managed child");

        assert!(status.success());
        assert!(
            direct_child_was_running,
            "direct child exited before the adopted descendant was checked"
        );
        assert!(
            orphan_reaped,
            "adopted descendant {orphan_pid} remained a zombie while the direct child was running"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::process::Stdio;
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    #[test]
    fn wait_terminates_descendants_in_the_windows_job_object() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pid_path = temp.path().join("descendant.pid");
        let escaped_path = pid_path.display().to_string().replace('\'', "''");
        let script = format!(
            "$child = Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep 30' -PassThru; Set-Content -NoNewline -LiteralPath '{escaped_path}' -Value $child.Id"
        );
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-Command", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = ManagedChild::spawn(&mut command).expect("spawn managed child");
        let status = child.wait().expect("wait managed child");
        assert!(status.success());

        let pid = std::fs::read_to_string(pid_path)
            .expect("descendant pid")
            .parse::<u32>()
            .expect("numeric pid");
        assert!(
            !process_is_running(pid),
            "descendant {pid} survived its Windows job object"
        );
    }

    fn process_is_running(pid: u32) -> bool {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let success = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
        unsafe {
            CloseHandle(handle);
        }
        success && exit_code == STILL_ACTIVE as u32
    }
}
