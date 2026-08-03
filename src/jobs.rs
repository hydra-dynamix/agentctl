use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::capture::{ExecutionAttempt, FailureKind};
use crate::config::{AgentConfig, CONFIG_DIR, SummaryConfig};
use crate::exec::{CaptureOptions, CapturedRun, ProcessOptions, format_command, run_captured_with};
use crate::platform;
use crate::process::spawn_detached;
use crate::summary::{Summary, summarize, summarize_with_command};

#[derive(Debug, Clone)]
pub struct AgentRunOptions {
    pub agent_name: String,
    pub agent: AgentConfig,
    pub prompt: String,
    pub cwd: PathBuf,
    pub iterations: u32,
    pub attempt: ExecutionAttempt,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForegroundIteration {
    pub iteration: u32,
    pub iterations: u32,
    pub summary: Summary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
    Stale,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobRecord {
    pub id: String,
    pub agent: String,
    pub state: JobState,
    pub pid: u32,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub exit_code: Option<i32>,
    pub command: String,
    pub cwd: PathBuf,
    pub iterations: u32,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub record: JobRecord,
    pub tail_lines: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct StoredJobRecord {
    id: String,
    agent: String,
    pid: u32,
    started_at: u64,
    command: String,
    cwd: PathBuf,
    iterations: u32,
    log_path: PathBuf,
    exit_code_path: PathBuf,
    #[serde(default)]
    attempt: Option<ExecutionAttempt>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SupervisorSpec {
    argv: Vec<String>,
    prompt_stdin: bool,
    prompt: String,
    cwd: PathBuf,
    iterations: u32,
    exit_code_path: PathBuf,
    label: String,
    attempt: ExecutionAttempt,
}

#[derive(Debug, Clone, Default)]
struct ExitRecord {
    exit_code: Option<i32>,
    finished_at: Option<u64>,
}

pub fn run_foreground(
    options: &AgentRunOptions,
    summary_config: &SummaryConfig,
) -> Result<Vec<ForegroundIteration>> {
    validate_options(options)?;
    let mut reports = Vec::new();
    for iteration in 1..=options.iterations {
        let run = match run_agent_once(options, summary_config, iteration) {
            Ok(run) => run,
            Err(error) => {
                options.attempt.record_error(FailureKind::Spawn)?;
                return Err(error);
            }
        };
        let summary = match &summary_config.summarizer_command {
            Some(command) => summarize_with_command(&run, summary_config, command)
                .unwrap_or_else(|_| summarize(&run, summary_config)),
            None => summarize(&run, summary_config),
        };
        let exit_code = run.exit_status.code().unwrap_or(1);
        reports.push(ForegroundIteration {
            iteration,
            iterations: options.iterations,
            summary,
        });
        if exit_code != 0 {
            options
                .attempt
                .record_error(failure_from_status(run.exit_status))?;
            bail!(
                "agent iteration {iteration}/{} exited with code {exit_code}",
                options.iterations
            );
        }
    }
    options.attempt.complete();
    Ok(reports)
}

pub fn launch_detached(options: &AgentRunOptions) -> Result<JobRecord> {
    validate_options(options)?;
    let started_at = unix_timestamp()?;
    let job_id = job_id(started_at, &options.agent_name);
    let job_dir = jobs_dir()?.join(&job_id);
    fs::create_dir_all(&job_dir).with_context(|| format!("creating {}", job_dir.display()))?;
    let log_path = job_dir.join("output.log");
    let exit_code_path = job_dir.join("exit-code");
    let job_path = job_dir.join("job.toml");
    let supervisor_path = job_dir.join("supervisor.toml");
    let stdout = fs::File::create(&log_path)
        .with_context(|| format!("creating agent log {}", log_path.display()))?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("cloning agent log {}", log_path.display()))?;

    let argv = command_argv(&options.agent, &options.prompt);
    let supervisor = SupervisorSpec {
        argv: argv.clone(),
        prompt_stdin: options.agent.prompt_stdin,
        prompt: options.prompt.clone(),
        cwd: options.cwd.clone(),
        iterations: options.iterations,
        exit_code_path: exit_code_path.clone(),
        label: options.agent_name.clone(),
        attempt: options.attempt.clone(),
    };
    fs::write(&supervisor_path, toml::to_string_pretty(&supervisor)?)
        .with_context(|| format!("writing {}", supervisor_path.display()))?;

    let executable = std::env::current_exe().context("locating agentctl executable")?;
    let mut command = ProcessCommand::new(executable);
    command.arg("__supervise").arg(&supervisor_path);
    command.current_dir(&options.cwd);
    command.stdin(Stdio::null());
    command.stdout(Stdio::from(stdout));
    command.stderr(Stdio::from(stderr));
    platform::configure_child_home(&mut command)?;

    let child = match spawn_detached(&mut command) {
        Ok(child) => child,
        Err(error) => {
            options.attempt.record_error(FailureKind::Spawn)?;
            return Err(error)
                .with_context(|| format!("launching detached agent `{}`", format_command(&argv)));
        }
    };

    let stored = StoredJobRecord {
        id: job_id,
        agent: options.agent_name.clone(),
        pid: child.id(),
        started_at,
        command: format_command(&argv),
        cwd: options.cwd.clone(),
        iterations: options.iterations,
        log_path,
        exit_code_path,
        attempt: Some(options.attempt.clone()),
    };
    fs::write(&job_path, toml::to_string_pretty(&stored)?)
        .with_context(|| format!("writing {}", job_path.display()))?;
    record_from_stored(stored)
}

pub fn supervise(path: &Path) -> Result<i32> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let spec: SupervisorSpec =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    // The prompt only needs to exist long enough for the detached supervisor
    // to start. Do not leave the transient spec on disk for completed jobs.
    let _ = fs::remove_file(path);

    let mut exit_code = 0;
    for iteration in 1..=spec.iterations {
        if spec.iterations > 1 {
            println!(
                "[iteration {iteration}/{}] {}",
                spec.iterations,
                format_command(&spec.argv)
            );
        }
        let capture_options = CaptureOptions {
            max_output_bytes: 16_384,
            tail_bytes: 4_096,
            label: Some(format!("{}-detached-iteration-{iteration}", spec.label)),
            stream_raw: true,
        };
        let process_options = agent_process_options(
            &spec.cwd,
            &spec.prompt,
            spec.prompt_stdin,
            iteration,
            spec.iterations,
        );
        match run_captured_with(&spec.argv, &capture_options, &process_options) {
            Ok(run) => {
                exit_code = exit_status_code(run.exit_status);
                if exit_code != 0 {
                    spec.attempt
                        .record_error(failure_from_status(run.exit_status))?;
                    break;
                }
            }
            Err(error) => {
                eprintln!("agent supervisor failed: {error:#}");
                spec.attempt.record_error(FailureKind::Spawn)?;
                exit_code = 1;
                break;
            }
        }
    }
    write_exit_record(&spec.exit_code_path, exit_code)?;
    if exit_code == 0 {
        spec.attempt.complete();
    }
    Ok(exit_code)
}

pub fn status(job_id: &str, tail: usize) -> Result<JobStatus> {
    let stored = read_job(job_id)?;
    let tail_lines = read_tail_lines(&stored.log_path, tail)?;
    Ok(JobStatus {
        record: record_from_stored(stored)?,
        tail_lines,
    })
}

pub fn list() -> Result<Vec<JobRecord>> {
    let dir = jobs_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut records = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path().join("job.toml");
        if path.exists()
            && let Ok(stored) = read_job_path(&path)
        {
            records.push(record_from_stored(stored)?);
        }
    }
    records.sort_by_key(|record| record.started_at);
    Ok(records)
}

pub fn logs(job_id: &str, follow: bool) -> Result<()> {
    let stored = read_job(job_id)?;
    let mut position = 0;
    loop {
        let mut file = fs::File::open(&stored.log_path)
            .with_context(|| format!("opening {}", stored.log_path.display()))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .with_context(|| format!("reading {}", stored.log_path.display()))?;
        if position < contents.len() {
            print!("{}", &contents[position..]);
            std::io::stdout().flush().context("flushing logs")?;
            position = contents.len();
        }
        if !follow || !matches!(record_from_stored(stored.clone())?.state, JobState::Running) {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

fn run_agent_once(
    options: &AgentRunOptions,
    summary_config: &SummaryConfig,
    iteration: u32,
) -> Result<CapturedRun> {
    let argv = command_argv(&options.agent, &options.prompt);
    let capture_options = CaptureOptions {
        max_output_bytes: summary_config.max_output_bytes,
        tail_bytes: summary_config.tail_bytes,
        label: Some(format!("{}-iteration-{iteration}", options.agent_name)),
        stream_raw: false,
    };
    let process_options = agent_process_options(
        &options.cwd,
        &options.prompt,
        options.agent.prompt_stdin,
        iteration,
        options.iterations,
    );
    run_captured_with(&argv, &capture_options, &process_options)
}

fn agent_process_options(
    cwd: &Path,
    prompt: &str,
    prompt_stdin: bool,
    iteration: u32,
    iterations: u32,
) -> ProcessOptions {
    ProcessOptions {
        cwd: Some(cwd.to_path_buf()),
        env: vec![
            ("AGENTCTL_AGENT_PROMPT".into(), prompt.into()),
            ("AGENTCTL_AGENT_CWD".into(), cwd.as_os_str().to_owned()),
            (
                "AGENTCTL_AGENT_ITERATION".into(),
                iteration.to_string().into(),
            ),
            (
                "AGENTCTL_AGENT_ITERATIONS".into(),
                iterations.to_string().into(),
            ),
        ],
        stdin: prompt_stdin.then(|| prompt.as_bytes().to_vec()),
    }
}

fn validate_options(options: &AgentRunOptions) -> Result<()> {
    if options.agent.command.is_empty() {
        bail!("agent command cannot be empty");
    }
    if options.prompt.trim().is_empty() {
        bail!("agent prompt cannot be empty");
    }
    if options.iterations == 0 {
        bail!("agent iterations must be greater than zero");
    }
    Ok(())
}

fn command_argv(agent: &AgentConfig, prompt: &str) -> Vec<String> {
    let mut argv = agent.command.clone();
    if !agent.prompt_stdin {
        argv.push(prompt.to_owned());
    }
    argv
}

fn record_from_stored(stored: StoredJobRecord) -> Result<JobRecord> {
    let exit_record = read_exit_record(&stored.exit_code_path)?;
    let state = if process_is_running(stored.pid) && exit_record.exit_code.is_none() {
        JobState::Running
    } else {
        match exit_record.exit_code {
            Some(0) => JobState::Succeeded,
            Some(_) => JobState::Failed,
            None => JobState::Stale,
        }
    };
    if state == JobState::Stale
        && let Some(attempt) = &stored.attempt
    {
        attempt.record_error(FailureKind::UnexpectedDisappearance)?;
    }
    Ok(JobRecord {
        id: stored.id,
        agent: stored.agent,
        state,
        pid: stored.pid,
        started_at: stored.started_at,
        finished_at: exit_record.finished_at,
        exit_code: exit_record.exit_code,
        command: stored.command,
        cwd: stored.cwd,
        iterations: stored.iterations,
        log_path: stored.log_path,
    })
}

fn read_exit_record(path: &Path) -> Result<ExitRecord> {
    if !path.exists() {
        return Ok(ExitRecord::default());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut lines = raw.lines();
    let exit_code = lines.next().and_then(|line| line.parse::<i32>().ok());
    let mut finished_at = None;
    for line in lines {
        if let Some(value) = line.strip_prefix("finished_at=") {
            finished_at = value.parse::<u64>().ok();
        }
    }
    Ok(ExitRecord {
        exit_code,
        finished_at,
    })
}

fn write_exit_record(path: &Path, exit_code: i32) -> Result<()> {
    let finished_at = unix_timestamp()?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = fs::File::create(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    file.write_all(format!("{exit_code}\nfinished_at={finished_at}\n").as_bytes())
        .with_context(|| format!("writing {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", temporary.display()))?;
    drop(file);
    fs::rename(&temporary, path)
        .with_context(|| format!("atomically publishing terminal record {}", path.display()))
}

fn exit_status_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|signal| 128 + signal).unwrap_or(1)
    }

    #[cfg(not(unix))]
    {
        1
    }
}

fn failure_from_status(status: ExitStatus) -> FailureKind {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return FailureKind::Signal(signal);
        }
    }
    FailureKind::ExitCode(status.code().unwrap_or(1))
}

fn read_job(job_id: &str) -> Result<StoredJobRecord> {
    read_job_path(&jobs_dir()?.join(job_id).join("job.toml"))
}

fn read_job_path(path: &Path) -> Result<StoredJobRecord> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn read_tail_lines(path: &Path, tail: usize) -> Result<Vec<String>> {
    let raw = fs::read_to_string(path).unwrap_or_default();
    let mut lines = raw
        .lines()
        .rev()
        .take(tail)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.reverse();
    Ok(lines)
}

fn jobs_dir() -> Result<PathBuf> {
    Ok(platform::home_dir()?.join(CONFIG_DIR).join("jobs"))
}

fn job_id(started_at: u64, agent_name: &str) -> String {
    format!(
        "{}-{}-{}",
        started_at,
        std::process::id(),
        safe_name(agent_name)
    )
}

fn safe_name(value: &str) -> String {
    value
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

fn unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before unix epoch")?
        .as_secs())
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return false;
    }
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

#[cfg(not(any(unix, windows)))]
fn process_is_running(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_job_reports_stale_when_pid_is_dead_and_exit_code_missing() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let stored = StoredJobRecord {
            id: "job".to_owned(),
            agent: "test".to_owned(),
            pid: 0,
            started_at: 1,
            command: "true".to_owned(),
            cwd: temp_dir.path().to_path_buf(),
            iterations: 1,
            log_path: temp_dir.path().join("output.log"),
            exit_code_path: temp_dir.path().join("exit-code"),
            attempt: None,
        };

        let record = record_from_stored(stored).expect("record");

        assert_eq!(record.state, JobState::Stale);
    }

    #[test]
    fn stale_supervisor_records_unexpected_disappearance() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let attempt =
            ExecutionAttempt::begin(temp_dir.path(), "test-agent").expect("execution intent");
        let stored = StoredJobRecord {
            id: "job".to_owned(),
            agent: "test".to_owned(),
            pid: 0,
            started_at: 1,
            command: "redacted".to_owned(),
            cwd: temp_dir.path().to_path_buf(),
            iterations: 1,
            log_path: temp_dir.path().join("output.log"),
            exit_code_path: temp_dir.path().join("exit-code"),
            attempt: Some(attempt),
        };

        let record = record_from_stored(stored).expect("record");

        assert_eq!(record.state, JobState::Stale);
        let inbox = temp_dir.path().join(".ldgr/recovery/inbox");
        let recovery = fs::read_dir(inbox)
            .expect("recovery inbox")
            .filter_map(|entry| entry.ok())
            .map(|entry| fs::read_to_string(entry.path()).expect("recovery record"))
            .find(|payload| payload.contains("unexpected-disappearance"))
            .expect("unexpected disappearance record");
        assert!(!recovery.contains("redacted"));
    }
}
