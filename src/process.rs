use std::io;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};

#[cfg(unix)]
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
#[cfg(unix)]
use signal_hook::iterator::Signals;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
const TERMINATION_GRACE: Duration = Duration::from_millis(750);

#[cfg(unix)]
static ACTIVE_PROCESS_GROUPS: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());
#[cfg(unix)]
static NEXT_OWNER: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
const OWNER_ENV: &str = "AGENTCTL_PROCESS_OWNER";
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
    #[cfg(unix)]
    owner: String,
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
        enable_subreaper()?;
        configure_process_group(command);
        install_signal_forwarder()?;
        #[cfg(unix)]
        let owner = format!(
            "{}-{}",
            std::process::id(),
            NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
        );
        #[cfg(unix)]
        command.env(OWNER_ENV, &owner);
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
                owner,
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
        #[cfg(unix)]
        terminate_process_tree(self.process_group, &self.owner);
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

        #[cfg(unix)]
        terminate_process_tree(self.process_group, &self.owner);
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
fn terminate_process_tree(process_group: i32, owner: &str) {
    if process_group <= 0 {
        return;
    }
    let mut descendants = BTreeSet::new();
    for signal in [SIGTERM, libc::SIGKILL] {
        let deadline = Instant::now() + TERMINATION_GRACE;
        loop {
            collect_owned_descendants(process_group, owner, &mut descendants);
            unsafe {
                libc::kill(-process_group, signal);
            }
            for &pid in &descendants {
                unsafe {
                    libc::kill(pid, signal);
                }
            }
            // Do not consume the direct child's ExitStatus or any unrelated
            // child's wait status. Retain known PIDs until reaped, since a
            // zombie no longer exposes its environment ownership marker.
            descendants.retain(|&pid| {
                let mut status = 0;
                let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                result != pid && (unsafe { libc::kill(pid, 0) } == 0)
            });
            if !process_group_exists(process_group) && descendants.is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(unix)]
fn process_group_exists(process_group: i32) -> bool {
    if unsafe { libc::kill(-process_group, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// A subreaper can also adopt unrelated host children. Restrict cleanup to
// this process group or its inherited ownership marker, which survives setsid.
#[cfg(target_os = "linux")]
fn collect_owned_descendants(group: i32, owner: &str, descendants: &mut BTreeSet<i32>) {
    let marker = format!("{OWNER_ENV}={owner}");
    let mut pending: Vec<i32> = linux_child_pids(std::process::id() as i32)
        .into_iter()
        .filter(|&pid| {
            if pid == group {
                return false;
            }
            let same_group = unsafe { libc::getpgid(pid) } == group;
            same_group
                || std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|env| {
                    env.split(|byte| *byte == 0)
                        .any(|entry| entry == marker.as_bytes())
                })
        })
        .collect();
    while let Some(pid) = pending.pop() {
        if descendants.insert(pid) {
            pending.extend(linux_child_pids(pid));
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn collect_owned_descendants(_group: i32, _owner: &str, _descendants: &mut BTreeSet<i32>) {}

#[cfg(target_os = "linux")]
fn linux_child_pids(pid: i32) -> Vec<i32> {
    let mut children = BTreeSet::new();
    if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
        for task in tasks.flatten() {
            if let Ok(raw) = std::fs::read_to_string(task.path().join("children")) {
                children.extend(
                    raw.split_whitespace()
                        .filter_map(|value| value.parse::<i32>().ok()),
                );
            }
        }
    }
    children.into_iter().collect()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;
    #[cfg(unix)]
    use std::process::Stdio;

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
    #[cfg(unix)]
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
