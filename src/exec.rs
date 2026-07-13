use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::config::CONFIG_DIR;
use crate::process::ManagedChild;
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub max_output_bytes: usize,
    pub tail_bytes: usize,
    pub label: Option<String>,
    pub stream_raw: bool,
}

#[derive(Debug, Clone)]
pub struct CapturedRun {
    pub command: Vec<String>,
    pub exit_status: ExitStatus,
    pub elapsed: Duration,
    pub stdout: String,
    pub stderr: String,
    pub raw_log_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProcessOptions {
    pub cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
    pub stdin: Option<Vec<u8>>,
}

pub fn run_captured(argv: &[String], options: &CaptureOptions) -> Result<CapturedRun> {
    run_captured_with(argv, options, &ProcessOptions::default())
}

pub(crate) fn run_captured_with(
    argv: &[String],
    options: &CaptureOptions,
    process_options: &ProcessOptions,
) -> Result<CapturedRun> {
    if argv.is_empty() {
        bail!("exec requires a command after `--`");
    }

    let raw_log_path = create_raw_log_path(options.label.as_deref())?;
    let stdout_path = raw_log_path.with_extension("stdout.tmp");
    let stderr_path = raw_log_path.with_extension("stderr.tmp");
    let start = Instant::now();
    let mut command = ProcessCommand::new(&argv[0]);
    if argv.len() > 1 {
        command.args(&argv[1..]);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(cwd) = &process_options.cwd {
        command.current_dir(cwd);
    }
    command.envs(process_options.env.iter().cloned());
    if process_options.stdin.is_some() {
        command.stdin(Stdio::piped());
    }

    let mut child = ManagedChild::spawn(&mut command)
        .with_context(|| format!("executing `{}`", format_command(argv)))?;
    let stdout = child.take_stdout().context("capturing child stdout")?;
    let stderr = child.take_stderr().context("capturing child stderr")?;

    let stream_stdout = options.stream_raw;
    let max_output_bytes = options.max_output_bytes;
    let tail_bytes = options.tail_bytes.min(max_output_bytes);
    let stdout_capture_path = stdout_path.clone();
    let stdout_handle = thread::spawn(move || {
        read_stream(
            stdout,
            stream_stdout,
            StreamKind::Stdout,
            max_output_bytes,
            tail_bytes,
            &stdout_capture_path,
        )
    });
    let stream_stderr = options.stream_raw;
    let stderr_capture_path = stderr_path.clone();
    let stderr_handle = thread::spawn(move || {
        read_stream(
            stderr,
            stream_stderr,
            StreamKind::Stderr,
            max_output_bytes,
            tail_bytes,
            &stderr_capture_path,
        )
    });

    if let Some(input) = &process_options.stdin
        && let Some(mut stdin) = child.take_stdin()
    {
        stdin
            .write_all(input)
            .context("writing managed child stdin")?;
    }

    let status = child
        .wait()
        .with_context(|| format!("waiting on `{}`", format_command(argv)))?;
    let elapsed = start.elapsed();
    let stdout_raw = stdout_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stdout capture thread panicked"))??;
    let stderr_raw = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stderr capture thread panicked"))??;

    persist_raw_log(&raw_log_path, &stdout_path, &stderr_path)
        .with_context(|| format!("writing raw log {}", raw_log_path.display()))?;

    Ok(CapturedRun {
        command: argv.to_owned(),
        exit_status: status,
        elapsed,
        stdout: stdout_raw,
        stderr: stderr_raw,
        raw_log_path,
    })
}

enum StreamKind {
    Stdout,
    Stderr,
}

fn read_stream<R: Read>(
    mut reader: R,
    stream_raw: bool,
    kind: StreamKind,
    max_output_bytes: usize,
    tail_bytes: usize,
    capture_path: &Path,
) -> Result<String> {
    let mut captured = BoundedCapture::new(max_output_bytes, tail_bytes);
    let mut capture_file = fs::File::create(capture_path)
        .with_context(|| format!("creating stream capture {}", capture_path.display()))?;
    let mut buffer = [0_u8; 8192];
    loop {
        let bytes_read = reader.read(&mut buffer).context("reading child output")?;
        if bytes_read == 0 {
            break;
        }
        let chunk = &buffer[..bytes_read];
        capture_file
            .write_all(chunk)
            .with_context(|| format!("writing stream capture {}", capture_path.display()))?;
        captured.push(chunk);
        if stream_raw {
            match kind {
                StreamKind::Stdout => {
                    let mut stdout = std::io::stdout().lock();
                    stdout.write_all(chunk).context("streaming child stdout")?;
                    stdout.flush().context("flushing child stdout")?;
                }
                StreamKind::Stderr => {
                    let mut stderr = std::io::stderr().lock();
                    stderr.write_all(chunk).context("streaming child stderr")?;
                    stderr.flush().context("flushing child stderr")?;
                }
            }
        }
    }
    capture_file
        .flush()
        .with_context(|| format!("flushing stream capture {}", capture_path.display()))?;
    Ok(captured.finish())
}

struct BoundedCapture {
    max_bytes: usize,
    head_limit: usize,
    tail_limit: usize,
    total_bytes: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
}

impl BoundedCapture {
    fn new(max_bytes: usize, tail_bytes: usize) -> Self {
        let tail_limit = tail_bytes.min(max_bytes);
        Self {
            max_bytes,
            head_limit: max_bytes.saturating_sub(tail_limit),
            tail_limit,
            total_bytes: 0,
            head: Vec::with_capacity(max_bytes.saturating_sub(tail_limit)),
            tail: VecDeque::with_capacity(tail_limit),
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        let head_remaining = self.head_limit.saturating_sub(self.head.len());
        let head_bytes = head_remaining.min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_bytes]);

        if self.tail_limit == 0 {
            return;
        }
        self.tail.extend(&bytes[head_bytes..]);
        while self.tail.len() > self.tail_limit {
            self.tail.pop_front();
        }
    }

    fn finish(self) -> String {
        if self.max_bytes == 0 {
            return String::new();
        }
        let mut bytes = self.head;
        if self.total_bytes > self.max_bytes {
            let retained = bytes.len() + self.tail.len();
            bytes.extend_from_slice(
                format!(
                    "\n\n[... omitted {} bytes ...]\n\n",
                    self.total_bytes.saturating_sub(retained)
                )
                .as_bytes(),
            );
        }
        bytes.extend(self.tail);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

pub fn format_command(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg.chars().any(|character| character.is_whitespace()) {
                let escaped = arg.replace('"', "\\\"");
                format!("\"{}\"", escaped)
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn create_raw_log_path(label: Option<&str>) -> Result<PathBuf> {
    let jobs_dir = home_jobs_dir()?;
    fs::create_dir_all(&jobs_dir)
        .with_context(|| format!("creating jobs directory {}", jobs_dir.display()))?;
    let job_id = job_id(label);
    let job_dir = jobs_dir.join(job_id);
    fs::create_dir_all(&job_dir)
        .with_context(|| format!("creating job directory {}", job_dir.display()))?;
    Ok(job_dir.join("output.log"))
}

fn home_jobs_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(CONFIG_DIR).join("jobs"))
}

fn job_id(label: Option<&str>) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let process_id = std::process::id();
    match label.map(sanitize_label).filter(|value| !value.is_empty()) {
        Some(label) => format!("{timestamp}-{process_id}-{label}"),
        None => format!("{timestamp}-{process_id}"),
    }
}

fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn persist_raw_log(path: &Path, stdout_path: &Path, stderr_path: &Path) -> Result<()> {
    let mut raw = fs::File::create(path).context("creating raw command output")?;
    append_capture(&mut raw, "stdout:\n", stdout_path)?;
    append_capture(&mut raw, "stderr:\n", stderr_path)?;
    raw.flush().context("flushing raw command output")?;
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);
    Ok(())
}

fn append_capture(raw: &mut fs::File, heading: &str, capture_path: &Path) -> Result<()> {
    let mut capture = fs::File::open(capture_path)
        .with_context(|| format!("opening stream capture {}", capture_path.display()))?;
    if capture.metadata()?.len() == 0 {
        return Ok(());
    }
    raw.write_all(heading.as_bytes())?;
    std::io::copy(&mut capture, raw)?;
    raw.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_capture_keeps_head_and_tail_without_retaining_the_middle() {
        let mut capture = BoundedCapture::new(10, 4);
        capture.push(b"012345");
        capture.push(b"6789abcdef");

        let output = capture.finish();

        assert!(output.starts_with("012345"));
        assert!(output.contains("omitted 6 bytes"));
        assert!(output.ends_with("cdef"));
    }

    #[test]
    fn bounded_capture_preserves_small_output_exactly() {
        let mut capture = BoundedCapture::new(10, 4);
        capture.push(b"hello");

        assert_eq!(capture.finish(), "hello");
    }
}
