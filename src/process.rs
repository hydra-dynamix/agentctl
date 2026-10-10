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
