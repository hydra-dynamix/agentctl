use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::config::CONFIG_DIR;
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

pub fn run_captured(argv: &[String], options: &CaptureOptions) -> Result<CapturedRun> {
    if argv.is_empty() {
        bail!("exec requires a command after `--`");
    }

    let raw_log_path = create_raw_log_path(options.label.as_deref())?;
    let start = Instant::now();
    let mut command = ProcessCommand::new(&argv[0]);
    if argv.len() > 1 {
        command.args(&argv[1..]);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("executing `{}`", format_command(argv)))?;
    let stdout = child.stdout.take().context("capturing child stdout")?;
    let stderr = child.stderr.take().context("capturing child stderr")?;

    let stream_stdout = options.stream_raw;
    let stdout_handle =
        thread::spawn(move || read_stream(stdout, stream_stdout, StreamKind::Stdout));
    let stream_stderr = options.stream_raw;
    let stderr_handle =
        thread::spawn(move || read_stream(stderr, stream_stderr, StreamKind::Stderr));

    let status = child
        .wait()
        .with_context(|| format!("waiting on `{}`", format_command(argv)))?;
    let elapsed = start.elapsed();
    let stdout_bytes = stdout_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stdout capture thread panicked"))??;
    let stderr_bytes = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stderr capture thread panicked"))??;

    let stdout_raw = String::from_utf8_lossy(&stdout_bytes).to_string();
    let stderr_raw = String::from_utf8_lossy(&stderr_bytes).to_string();
    persist_raw_log(&raw_log_path, &stdout_raw, &stderr_raw)
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

fn read_stream<R: Read>(mut reader: R, stream_raw: bool, kind: StreamKind) -> Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let bytes_read = reader.read(&mut buffer).context("reading child output")?;
        if bytes_read == 0 {
            break;
        }
        captured.extend_from_slice(&buffer[..bytes_read]);
        if stream_raw {
            match kind {
                StreamKind::Stdout => {
                    let mut stdout = std::io::stdout().lock();
                    stdout
                        .write_all(&buffer[..bytes_read])
                        .context("streaming child stdout")?;
                    stdout.flush().context("flushing child stdout")?;
                }
                StreamKind::Stderr => {
                    let mut stderr = std::io::stderr().lock();
                    stderr
                        .write_all(&buffer[..bytes_read])
                        .context("streaming child stderr")?;
                    stderr.flush().context("flushing child stderr")?;
                }
            }
        }
    }
    Ok(captured)
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

fn persist_raw_log(path: &PathBuf, stdout: &str, stderr: &str) -> Result<()> {
    let mut raw = String::new();
    if !stdout.is_empty() {
        raw.push_str("stdout:\n");
        raw.push_str(stdout);
        if !stdout.ends_with('\n') {
            raw.push('\n');
        }
    }
    if !stderr.is_empty() {
        raw.push_str("stderr:\n");
        raw.push_str(stderr);
        if !stderr.ends_with('\n') {
            raw.push('\n');
        }
    }
    fs::write(path, raw).context("writing raw command output")
}
