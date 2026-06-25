use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::config::{AgentConfig, CONFIG_DIR, SummaryConfig};
use crate::exec::{CapturedRun, format_command};
use crate::summary::{Summary, summarize, summarize_with_command};

#[derive(Debug, Clone)]
pub struct AgentRunOptions {
    pub agent_name: String,
    pub agent: AgentConfig,
    pub prompt: String,
    pub cwd: PathBuf,
    pub iterations: u32,
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
        let run = run_agent_once(options, iteration)?;
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
            bail!(
                "agent iteration {iteration}/{} exited with code {exit_code}",
                options.iterations
            );
        }
    }
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
    let stdout = fs::File::create(&log_path)
        .with_context(|| format!("creating agent log {}", log_path.display()))?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("cloning agent log {}", log_path.display()))?;

    let script = detached_script(
        &command_argv(&options.agent, &options.prompt),
        options.agent.prompt_stdin,
        options.iterations,
        &exit_code_path,
    );
    let mut command = ProcessCommand::new("bash");
    command.arg("-lc").arg(script);
    command.current_dir(&options.cwd);
    command.env("AGENTCTL_AGENT_PROMPT", &options.prompt);
    command.env("AGENTCTL_AGENT_CWD", &options.cwd);
    command.stdin(Stdio::null());
    command.stdout(Stdio::from(stdout));
    command.stderr(Stdio::from(stderr));

    let child = command.spawn().with_context(|| {
        format!(
            "launching detached agent `{}`",
            format_command(&command_argv(&options.agent, &options.prompt))
        )
    })?;

    let stored = StoredJobRecord {
        id: job_id,
        agent: options.agent_name.clone(),
        pid: child.id(),
        started_at,
        command: format_command(&command_argv(&options.agent, &options.prompt)),
        cwd: options.cwd.clone(),
        iterations: options.iterations,
        log_path,
        exit_code_path,
    };
    fs::write(&job_path, toml::to_string_pretty(&stored)?)
        .with_context(|| format!("writing {}", job_path.display()))?;
    record_from_stored(stored)
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

fn run_agent_once(options: &AgentRunOptions, iteration: u32) -> Result<CapturedRun> {
    let argv = command_argv(&options.agent, &options.prompt);
    let raw_log_path =
        create_job_output_log(&format!("{}-iteration-{iteration}", options.agent_name))?;
    let start = Instant::now();
    let mut command = ProcessCommand::new(&argv[0]);
    command.args(&argv[1..]);
    command.current_dir(&options.cwd);
    command.env("AGENTCTL_AGENT_PROMPT", &options.prompt);
    command.env("AGENTCTL_AGENT_CWD", &options.cwd);
    command.env("AGENTCTL_AGENT_ITERATION", iteration.to_string());
    command.env("AGENTCTL_AGENT_ITERATIONS", options.iterations.to_string());
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if options.agent.prompt_stdin {
        command.stdin(Stdio::piped());
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("launching agent `{}`", format_command(&argv)))?;
    if options.agent.prompt_stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin
            .write_all(options.prompt.as_bytes())
            .context("writing prompt to agent stdin")?;
    }

    let output = child.wait_with_output().context("waiting for agent")?;
    let elapsed = start.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    persist_raw_log(&raw_log_path, &stdout, &stderr)?;

    Ok(CapturedRun {
        command: argv,
        exit_status: output.status,
        elapsed,
        stdout,
        stderr,
        raw_log_path,
    })
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

fn detached_script(
    argv: &[String],
    prompt_stdin: bool,
    iterations: u32,
    exit_code_path: &Path,
) -> String {
    let command = shell_command(argv);
    let display = shell_single_quote(&format_command(argv));
    let exit_code_path = shell_single_quote(&exit_code_path.display().to_string());
    let invoke = if prompt_stdin {
        format!(
            "printf '%s' \"$AGENTCTL_AGENT_PROMPT\" | AGENTCTL_AGENT_ITERATION=\"$iteration\" {command}"
        )
    } else {
        format!("AGENTCTL_AGENT_ITERATION=\"$iteration\" {command}")
    };
    format!(
        "status=0; for iteration in $(seq 1 {iterations}); do if [ {iterations} -gt 1 ]; then printf '[iteration %s/{iterations}] %s\\n' \"$iteration\" {display}; fi; {invoke}; status=$?; if [ $status -ne 0 ]; then break; fi; done; finished_at=$(date +%s); {{ printf '%s\\n' \"$status\"; printf 'finished_at=%s\\n' \"$finished_at\"; }} > {exit_code_path}; exit $status"
    )
}

fn shell_command(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| shell_single_quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(CONFIG_DIR).join("jobs"))
}

fn create_job_output_log(label: &str) -> Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = jobs_dir()?.join(format!("{}-{}", timestamp, safe_name(label)));
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join("output.log"))
}

fn persist_raw_log(path: &Path, stdout: &str, stderr: &str) -> Result<()> {
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
    fs::write(path, raw).with_context(|| format!("writing {}", path.display()))
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

fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    ProcessCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
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
        };

        let record = record_from_stored(stored).expect("record");

        assert_eq!(record.state, JobState::Stale);
    }
}
