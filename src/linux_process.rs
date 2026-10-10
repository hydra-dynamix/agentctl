//! Per-capture Linux subreaper. The host never adopts or reaps command descendants.
//!
//! This runs in Command's pre_exec child, then forks the actual worker. The
//! worker returns to Rust's exec path; the supervisor never returns. All code
//! after fork uses stack storage and async-signal-safe libc operations only.
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn record_signal(signal: libc::c_int) {
    SIGNAL.store(signal, Ordering::Relaxed);
}

pub(crate) fn configure_supervisor(command: &mut Command) -> io::Result<()> {
    // Fail before launching a worker if the kernel child inventory is unavailable.
    std::fs::read("/proc/thread-self/children")?;
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: the closure and supervisor use no heap allocation, Rust locks,
    // formatting, destructors, or non-async-signal-safe library operations.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            // Watch the host process, not its spawning thread. PDEATHSIG is
            // thread-scoped and would kill captures moved between host threads.
            let parent_fd = libc::syscall(libc::SYS_pidfd_open, parent, 0) as i32;
            if parent_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            SIGNAL.store(0, Ordering::Relaxed);
            let signals = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];
            let mut originals: [libc::sigaction; 4] = std::mem::zeroed();
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record_signal as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            for (index, signal) in signals.iter().enumerate() {
                if libc::sigaction(*signal, &action, &mut originals[index]) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::getppid() != parent {
                libc::_exit(125);
            }
            // Raw fork-equivalent clone avoids invoking host pthread_atfork
            // callbacks a second time inside an already-forked process.
            let worker = libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) as libc::pid_t;
            if worker < 0 {
                return Err(io::Error::last_os_error());
            }
            if worker == 0 {
                libc::close(parent_fd);
                for (index, signal) in signals.iter().enumerate() {
                    if libc::sigaction(*signal, &originals[index], std::ptr::null_mut()) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                return Ok(());
            }
            if libc::dup2(parent_fd, 0) < 0 {
                libc::_exit(125);
            }
            supervise(worker)
        });
    }
    Ok(())
}

unsafe fn supervise(worker: libc::pid_t) -> ! {
    // The worker retains the exec-error pipe until exec succeeds or reports an
    // error. Closing our copy preserves Command::spawn's ordinary error result.
    unsafe {
        // FD 0 is the host pidfd; all other descriptors belong to the worker.
        if libc::syscall(libc::SYS_close_range, 1_u32, u32::MAX, 0_u32) != 0 {
            let mut limit: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                libc::_exit(125);
            }
            for fd in 1..limit.rlim_cur.min(i32::MAX as u64) {
                libc::close(fd as i32);
            }
        }
        let mut worker_status = None;
        let mut stopping_at = None;
        loop {
            let mut status = 0;
            let pid = libc::waitpid(-1, &mut status, libc::WNOHANG);
            if pid > 0 {
                if pid == worker {
                    worker_status = Some(status);
                }
                continue;
            }
            if pid < 0 && *libc::__errno_location() == libc::ECHILD {
                finish(worker_status.unwrap_or(125 << 8));
            }
            let mut host = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            if libc::poll(&mut host, 1, 0) > 0 {
                SIGNAL.store(libc::SIGTERM, Ordering::Relaxed);
            }
            let signal = SIGNAL.load(Ordering::Relaxed);
            if worker_status.is_some() || signal != 0 {
                let start = *stopping_at.get_or_insert_with(monotonic_ms);
                let signal = if monotonic_ms().saturating_sub(start) >= 750 {
                    libc::SIGKILL
                } else if signal != 0 {
                    signal
                } else {
                    libc::SIGTERM
                };
                signal_children(signal);
            }
            let delay = libc::timespec {
                tv_sec: 0,
                tv_nsec: 10_000_000,
            };
            libc::nanosleep(&delay, std::ptr::null_mut());
        }
    }
}

fn monotonic_ms() -> u64 {
    unsafe {
        let mut now: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
        (now.tv_sec as u64).saturating_mul(1000) + now.tv_nsec as u64 / 1_000_000
    }
}

unsafe fn signal_children(signal: i32) {
    unsafe {
        // Every child belongs to this capture, including adopted zombies and
        // descendants that call setsid or exec with a completely empty env.
        let fd = libc::open(c"/proc/thread-self/children".as_ptr(), libc::O_RDONLY);
        if fd < 0 {
            return;
        }
        let mut buffer = [0_u8; 4096];
        let mut pid: i32 = 0;
        loop {
            let count = libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len());
            if count < 0 {
                if *libc::__errno_location() == libc::EINTR {
                    continue;
                }
                // Never signal a partially parsed PID after a failed read.
                pid = 0;
                break;
            }
            if count == 0 {
                break;
            }
            for byte in &buffer[..count as usize] {
                if byte.is_ascii_digit() {
                    pid = pid.saturating_mul(10).saturating_add((byte - b'0') as i32);
                } else if pid > 0 {
                    libc::kill(pid, signal);
                    pid = 0;
                }
            }
        }
        if pid > 0 {
            libc::kill(pid, signal);
        }
        libc::close(fd);
    }
}

unsafe fn finish(status: i32) -> ! {
    unsafe {
        if libc::WIFEXITED(status) {
            libc::_exit(libc::WEXITSTATUS(status));
        }
        if libc::WIFSIGNALED(status) {
            let signal = libc::WTERMSIG(status);
            libc::signal(signal, libc::SIG_DFL);
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, signal);
            libc::sigprocmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
            libc::kill(libc::getpid(), signal);
            libc::_exit(128 + signal);
        }
        libc::_exit(125);
    }
}
