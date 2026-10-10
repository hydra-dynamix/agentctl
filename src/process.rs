use std::io;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
#[cfg(all(unix, not(target_os = "linux")))]
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};

#[cfg(unix)]
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
#[cfg(unix)]
use signal_hook::iterator::Signals;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(all(unix, not(target_os = "linux")))]
const TERMINATION_GRACE: Duration = Duration::from_millis(750);

#[cfg(unix)]
static ACTIVE_PROCESS_GROUPS: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());
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
    job: crate::windows_process::Job,
    reaped: bool,
}

/// Spawn a detached supervisor as its own process-group leader. The supervisor
/// is responsible for recording completion and terminating its group.
#[cfg(not(windows))]
pub(crate) fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    configure_process_group(command);
    command.spawn()
}

impl ManagedChild {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        configure_process_group(command);
        install_signal_forwarder()?;
        #[cfg(target_os = "linux")]
        crate::linux_process::configure_supervisor(command)?;
        // Serialize spawn/registration against signal forwarding.
        #[cfg(unix)]
        let mut active = ACTIVE_PROCESS_GROUPS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        #[cfg(not(windows))]
        let child = command.spawn()?;
        #[cfg(windows)]
        let (child, job) = crate::windows_process::Job::spawn(command)?;

        #[cfg(unix)]
        {
            let process_group = child.id() as i32;
            active.insert(process_group);
            Ok(Self {
                child,
                process_group,
                reaped: false,
            })
        }

        #[cfg(not(unix))]
        Ok(Self {
            child,
            #[cfg(windows)]
            job,
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

    fn clean_remaining_descendants(&self) {
        #[cfg(all(unix, not(target_os = "linux")))]
        terminate_process_tree(self.process_group);
        #[cfg(windows)]
        self.job.terminate();
    }

    fn clear_active_group(&self) {
        #[cfg(unix)]
        {
            ACTIVE_PROCESS_GROUPS
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&self.process_group);
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }

        #[cfg(target_os = "linux")]
        unsafe {
            libc::kill(self.process_group, SIGTERM);
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        terminate_process_tree(self.process_group);
        #[cfg(windows)]
        self.job.terminate();
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
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn install_signal_forwarder() -> io::Result<()> {
    if SIGNAL_FORWARDER.get().is_some() {
        return Ok(());
    }

    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])?;
    SIGNAL_FORWARDER.get_or_init(|| {
        std::thread::spawn(move || {
            for signal in signals.forever() {
                let active = ACTIVE_PROCESS_GROUPS
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if !active.is_empty() {
                    for &process_group in active.iter() {
                        // Negative PIDs address the complete process group.
                        unsafe {
                            libc::kill(-process_group, signal);
                        }
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

#[cfg(not(unix))]
fn install_signal_forwarder() -> io::Result<()> {
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn terminate_process_tree(process_group: i32) {
    for signal in [SIGTERM, libc::SIGKILL] {
        unsafe {
            libc::kill(-process_group, signal);
        }
        let deadline = Instant::now() + TERMINATION_GRACE;
        while Instant::now() < deadline {
            if unsafe { libc::kill(-process_group, 0) } != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::*;
    #[cfg(target_os = "linux")]
    use std::process::Stdio;

    #[test]
    #[cfg(target_os = "linux")]
    fn capture_survives_spawning_thread_exit() {
        let mut managed = std::thread::spawn(|| {
            let mut command = Command::new("sleep");
            command.arg("0.1");
            ManagedChild::spawn(&mut command).unwrap()
        })
        .join()
        .unwrap();
        assert!(managed.wait().unwrap().success());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn supervisor_reaps_escaped_children_without_environment_markers() {
        for zombie in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let pid_file = temp.path().join("child.pid");
            let escaped = if zombie {
                "import os,sys; os.setsid(); open(sys.argv[1], 'w').write(str(os.getpid()))"
            } else {
                "import os,sys,signal,time; os.setsid(); signal.signal(signal.SIGTERM, signal.SIG_IGN); open(sys.argv[1], 'w').write(str(os.getpid())); time.sleep(30)"
            };
            let script = "import os,sys,time; pid=os.fork(); (os.execve(sys.executable,[sys.executable,'-c',sys.argv[2],sys.argv[1]],{}) if pid==0 else None); time.sleep(0.3)";
            let mut command = Command::new("python");
            command
                .args(["-c", script])
                .arg(&pid_file)
                .arg(escaped)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut managed = ManagedChild::spawn(&mut command).unwrap();
            assert!(managed.wait().unwrap().success());
            let pid: i32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
            let survived = unsafe { libc::kill(pid, 0) } == 0;
            if survived {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
            assert!(!survived, "escaped child remained (zombie={zombie})");
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn supervisor_preserves_exec_errors_and_host_subreaper_setting() {
        let mut before: libc::c_int = 0;
        unsafe {
            assert_eq!(libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut before), 0);
        }
        let error = ManagedChild::spawn(&mut Command::new("/agentctl-nonexistent-executable"));
        assert!(matches!(error, Err(ref error) if error.kind() == io::ErrorKind::NotFound));
        let mut after: libc::c_int = 0;
        unsafe {
            assert_eq!(libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut after), 0);
        }
        assert_eq!(before, after, "library changed the host subreaper setting");
    }

    #[test]
    #[cfg(windows)]
    fn windows_job_terminates_descendants_after_leader_exits() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let pid_path = temp.path().join("descendant.pid");
        let mut command = Command::new("python");
        command.args(["-c", "import subprocess,sys; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)']); open(sys.argv[1],'w').write(str(p.pid))"])
            .arg(&pid_path);
        let mut child = ManagedChild::spawn(&mut command).unwrap();
        assert!(child.wait().unwrap().success());
        let pid: u32 = std::fs::read_to_string(pid_path).unwrap().parse().unwrap();
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if !handle.is_null() {
                let state = WaitForSingleObject(handle, 5000);
                CloseHandle(handle);
                assert_eq!(state, WAIT_OBJECT_0, "descendant survived its job");
            }
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn cleanup_preserves_unrelated_children_and_other_captures() {
        let mut unrelated = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("unrelated child");
        let mut long_command = Command::new("sleep");
        long_command.arg("30");
        let mut other = ManagedChild::spawn(&mut long_command).expect("other capture");
        let mut short = ManagedChild::spawn(&mut Command::new("true")).expect("short capture");
        assert!(short.wait().expect("wait short").success());
        let unrelated_alive = unrelated.try_wait().expect("poll unrelated").is_none();
        let other_alive = other.child.try_wait().expect("poll other").is_none();
        let _ = unrelated.kill();
        let _ = unrelated.wait();
        drop(other);
        assert!(
            unrelated_alive,
            "capture cleanup killed an unrelated host child"
        );
        assert!(other_alive, "capture cleanup killed a concurrent capture");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn wait_terminates_descendants_left_by_the_direct_child() {
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
}

#[cfg(all(test, target_os = "linux"))]
mod upstream_tests {
    use super::*;
    use std::process::Stdio;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

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
