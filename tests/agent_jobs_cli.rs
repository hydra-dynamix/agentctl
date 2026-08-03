use assert_cmd::Command;
use serde_json::Value;

fn write_agent_config(project_dir: &tempfile::TempDir) {
    std::fs::write(
        project_dir.path().join("agentctl.toml"),
        platform_agent_config(),
    )
    .expect("write config");
}

#[cfg(unix)]
fn platform_agent_config() -> &'static str {
    r#"
[summary]
max_output_bytes = 16384
tail_bytes = 4096
max_preview_lines = 12

[agents.trivial]
command = ["bash", "-c", "echo done"]
prompt_stdin = false

[agents.stdin_loop]
command = ["bash", "-c", "prompt=$(cat); echo prompt=$prompt; echo iteration=$AGENTCTL_AGENT_ITERATION"]
prompt_stdin = true
"#
}

#[cfg(windows)]
fn platform_agent_config() -> &'static str {
    r#"
[summary]
max_output_bytes = 16384
tail_bytes = 4096
max_preview_lines = 12

[agents.trivial]
command = ["cmd", "/C", "echo done"]
prompt_stdin = false

[agents.stdin_loop]
command = ["cmd", "/V:ON", "/C", "set /p prompt=& echo prompt=!prompt!& echo iteration=!AGENTCTL_AGENT_ITERATION!"]
prompt_stdin = true
"#
}

#[cfg(unix)]
fn default_codex_test_config() -> &'static str {
    r#"
[summary]
max_output_bytes = 16384
tail_bytes = 4096
max_preview_lines = 12

[agents.codex]
command = ["bash", "-c", "prompt=$(cat); echo default-codex=$prompt"]
prompt_stdin = true
"#
}

#[cfg(windows)]
fn default_codex_test_config() -> &'static str {
    r#"
[summary]
max_output_bytes = 16384
tail_bytes = 4096
max_preview_lines = 12

[agents.codex]
command = ["cmd", "/V:ON", "/C", "set /p prompt=& echo default-codex=!prompt!"]
prompt_stdin = true
"#
}

#[test]
fn run_defaults_to_codex_and_reads_prompt_from_stdin() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");
    std::fs::write(
        project_dir.path().join("agentctl.toml"),
        default_codex_test_config(),
    )
    .expect("write config");

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["run", "--json"])
        .write_stdin("hello from stdin")
        .output()
        .expect("run default codex agent");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("run json");
    let reports = json.as_array().expect("iteration reports");
    assert_eq!(reports.len(), 1);
    assert_foreground_iteration_schema(&reports[0]);
    assert!(
        reports[0]["summary"]["preview_lines"]
            .to_string()
            .contains("default-codex=hello from stdin")
    );
}

#[test]
fn foreground_agent_reports_each_iteration_as_summary_json() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");
    write_agent_config(&project_dir);

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args([
            "run",
            "stdin_loop",
            "--prompt",
            "hello",
            "--iterations",
            "2",
            "--json",
        ])
        .output()
        .expect("run foreground agent");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("run json");
    let reports = json.as_array().expect("iteration reports");
    assert_eq!(reports.len(), 2);
    assert_foreground_iteration_schema(&reports[0]);
    assert_foreground_iteration_schema(&reports[1]);
    assert_eq!(reports[0].get("iteration").and_then(Value::as_u64), Some(1));
    assert_eq!(reports[1].get("iteration").and_then(Value::as_u64), Some(2));
    assert!(
        reports[0]["summary"]["preview_lines"]
            .to_string()
            .contains("hello")
    );
    assert!(
        reports[1]["summary"]["preview_lines"]
            .to_string()
            .contains("iteration=2")
    );
}

#[test]
fn detached_trivial_agent_round_trips_run_status_logs() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");
    write_agent_config(&project_dir);

    let run_output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["run", "trivial", "--prompt", "hello", "--detach", "--json"])
        .output()
        .expect("run detached agent");

    assert!(run_output.status.success());
    let run_json: Value = serde_json::from_slice(&run_output.stdout).expect("run json");
    assert_job_record_schema(&run_json);
    let job_id = run_json["id"].as_str().expect("job id");

    let mut status_json = None;
    for _ in 0..20 {
        let status_output = Command::cargo_bin("agentctl")
            .expect("agentctl binary")
            .current_dir(project_dir.path())
            .env("HOME", home_dir.path())
            .args(["status", job_id, "--json"])
            .output()
            .expect("status detached agent");
        assert!(status_output.status.success());
        let json: Value = serde_json::from_slice(&status_output.stdout).expect("status json");
        assert_status_schema(&json);
        if json["record"]["state"] == "succeeded" {
            status_json = Some(json);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let status_json = status_json.expect("job succeeded");
    assert_eq!(status_json["record"]["exit_code"], 0);

    let list_output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["list", "--json"])
        .output()
        .expect("list detached jobs");

    assert!(list_output.status.success());
    let list_json: Value = serde_json::from_slice(&list_output.stdout).expect("list json");
    let jobs = list_json.as_array().expect("job list");
    assert!(jobs.iter().any(|job| job["id"] == job_id));
    for job in jobs {
        assert_job_record_schema(job);
    }

    let logs_output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["logs", job_id])
        .output()
        .expect("logs detached agent");

    assert!(logs_output.status.success());
    let logs = String::from_utf8(logs_output.stdout).expect("logs utf8");
    assert!(logs.contains("done"));
}

#[test]
#[cfg(unix)]
fn detached_supervisor_reaps_descendants_that_create_a_new_session() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");
    let descendant_pid_path = project_dir.path().join("detached-descendant.pid");
    std::fs::write(
        project_dir.path().join("agentctl.toml"),
        format!(
            r#"
[summary]
max_output_bytes = 1024
tail_bytes = 256
max_preview_lines = 12

[agents.detached_tree]
command = ["bash", "-c", "setsid sleep 30 & echo $! > '{}'; exit 0"]
prompt_stdin = false
"#,
            descendant_pid_path.display()
        ),
    )
    .expect("write config");

    let run_output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args([
            "run",
            "detached_tree",
            "--prompt",
            "test",
            "--detach",
            "--json",
        ])
        .output()
        .expect("run detached process tree");
    assert!(run_output.status.success());
    let run_json: Value = serde_json::from_slice(&run_output.stdout).expect("run json");
    let job_id = run_json["id"].as_str().expect("job id");

    let mut succeeded = false;
    for _ in 0..100 {
        let status_output = Command::cargo_bin("agentctl")
            .expect("agentctl binary")
            .current_dir(project_dir.path())
            .env("HOME", home_dir.path())
            .args(["status", job_id, "--json"])
            .output()
            .expect("status detached process tree");
        let status: Value = serde_json::from_slice(&status_output.stdout).expect("status json");
        if status["record"]["state"] == "succeeded" {
            succeeded = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(succeeded, "detached supervisor did not record success");

    let descendant_pid = std::fs::read_to_string(&descendant_pid_path)
        .expect("detached descendant pid")
        .trim()
        .parse::<i32>()
        .expect("numeric detached descendant pid");
    let descendant_exists = unsafe { libc::kill(descendant_pid, 0) } == 0;
    assert!(
        !descendant_exists,
        "detached descendant {descendant_pid} survived its supervisor"
    );
    assert!(
        !home_dir
            .path()
            .join(".agentctl/jobs")
            .join(job_id)
            .join("supervisor.toml")
            .exists(),
        "transient supervisor spec should be removed"
    );
}

#[test]
fn dead_pid_without_exit_code_is_stale() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");
    let job_dir = home_dir
        .path()
        .join(".agentctl")
        .join("jobs")
        .join("stale-job");
    std::fs::create_dir_all(&job_dir).expect("job dir");
    let log_path = job_dir.join("output.log");
    let exit_code_path = job_dir.join("exit-code");
    std::fs::write(&log_path, "started\n").expect("log");
    std::fs::write(
        job_dir.join("job.toml"),
        format!(
            r#"
id = "stale-job"
agent = "trivial"
pid = 0
started_at = 1
command = "bash -c true"
cwd = "{}"
iterations = 1
log_path = "{}"
exit_code_path = "{}"
"#,
            toml_path(project_dir.path()),
            toml_path(&log_path),
            toml_path(&exit_code_path)
        ),
    )
    .expect("job record");

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["status", "stale-job", "--json"])
        .output()
        .expect("status stale job");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("status json");
    assert_status_schema(&json);
    assert_eq!(json["record"]["state"], "stale");
}

fn toml_path(path: &std::path::Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

#[test]
fn foreground_agent_accepts_prompt_file_and_cwd_override() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");
    let run_dir = tempfile::tempdir().expect("run tempdir");
    write_agent_config(&project_dir);
    let prompt_path = project_dir.path().join("prompt.txt");
    std::fs::write(&prompt_path, "from-file").expect("write prompt");

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args([
            "run",
            "stdin_loop",
            "--prompt-file",
            prompt_path.to_str().expect("prompt path"),
            "--cwd",
            run_dir.path().to_str().expect("cwd path"),
            "--json",
        ])
        .output()
        .expect("run foreground agent");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("run json");
    let reports = json.as_array().expect("iteration reports");
    assert_eq!(reports.len(), 1);
    assert_foreground_iteration_schema(&reports[0]);
    assert!(
        reports[0]["summary"]["preview_lines"]
            .to_string()
            .contains("from-file")
    );
}

#[test]
#[cfg(unix)]
fn terminating_foreground_agentctl_terminates_and_reaps_the_agent_tree() {
    use std::os::unix::process::ExitStatusExt;

    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");
    let descendant_pid_path = project_dir.path().join("descendant.pid");
    std::fs::write(
        project_dir.path().join("agentctl.toml"),
        format!(
            r#"
[summary]
max_output_bytes = 1024
tail_bytes = 256
max_preview_lines = 12

[agents.tree]
command = ["bash", "-c", "trap 'exit 143' TERM; setsid sleep 30 & echo $! > '{}'; while :; do sleep 1; done"]
prompt_stdin = false
"#,
            descendant_pid_path.display()
        ),
    )
    .expect("write config");

    let mut agentctl = std::process::Command::new(assert_cmd::cargo::cargo_bin!("agentctl"))
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["run", "tree", "--prompt", "test"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn foreground agentctl");

    for _ in 0..100 {
        if descendant_pid_path.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let descendant_pid = std::fs::read_to_string(&descendant_pid_path)
        .expect("descendant pid")
        .trim()
        .parse::<i32>()
        .expect("numeric descendant pid");

    assert!(
        agentctl
            .try_wait()
            .expect("check foreground agentctl")
            .is_none(),
        "foreground agentctl exited before it could be signaled"
    );
    let signal_result = unsafe { libc::kill(agentctl.id() as i32, libc::SIGTERM) };
    assert_eq!(signal_result, 0, "signal foreground agentctl");
    let status = agentctl.wait().expect("wait for foreground agentctl");
    assert!(!status.success());
    assert!(status.code().is_some() || status.signal().is_some());

    let descendant_exists = unsafe { libc::kill(descendant_pid, 0) } == 0;
    assert!(
        !descendant_exists,
        "agent descendant {descendant_pid} survived agentctl termination"
    );
}

fn assert_foreground_iteration_schema(json: &Value) {
    let object = json.as_object().expect("foreground iteration object");
    assert_key_set(
        object.keys().map(String::as_str).collect(),
        &["iteration", "iterations", "summary"],
    );
    assert!(json.get("iteration").and_then(Value::as_u64).is_some());
    assert!(json.get("iterations").and_then(Value::as_u64).is_some());
    assert_summary_schema(&json["summary"]);
}

fn assert_status_schema(json: &Value) {
    let object = json.as_object().expect("status object");
    assert_key_set(
        object.keys().map(String::as_str).collect(),
        &["record", "tail_lines"],
    );
    assert_job_record_schema(&json["record"]);
    assert!(json.get("tail_lines").and_then(Value::as_array).is_some());
}

fn assert_job_record_schema(json: &Value) {
    let object = json.as_object().expect("job record object");
    assert_key_set(
        object.keys().map(String::as_str).collect(),
        &[
            "id",
            "agent",
            "state",
            "pid",
            "started_at",
            "finished_at",
            "exit_code",
            "command",
            "cwd",
            "iterations",
            "log_path",
        ],
    );
    assert!(json.get("id").and_then(Value::as_str).is_some());
    assert!(json.get("agent").and_then(Value::as_str).is_some());
    assert!(matches!(
        json.get("state").and_then(Value::as_str),
        Some("running" | "succeeded" | "failed" | "stale")
    ));
    assert!(json.get("pid").and_then(Value::as_u64).is_some());
    assert!(json.get("started_at").and_then(Value::as_u64).is_some());
    assert!(json.get("finished_at").is_some());
    assert!(json.get("exit_code").is_some());
    assert!(json.get("command").and_then(Value::as_str).is_some());
    assert!(json.get("cwd").and_then(Value::as_str).is_some());
    assert!(json.get("iterations").and_then(Value::as_u64).is_some());
    assert!(json.get("log_path").and_then(Value::as_str).is_some());
}

fn assert_summary_schema(json: &Value) {
    let object = json.as_object().expect("summary object");
    assert_key_set(
        object.keys().map(String::as_str).collect(),
        &[
            "command",
            "exit_code",
            "duration_ms",
            "preview_lines",
            "raw_log_path",
        ],
    );
    assert!(json.get("command").and_then(Value::as_str).is_some());
    assert!(json.get("exit_code").and_then(Value::as_i64).is_some());
    assert!(json.get("duration_ms").and_then(Value::as_u64).is_some());
    assert!(
        json.get("preview_lines")
            .and_then(Value::as_array)
            .is_some()
    );
    assert!(json.get("raw_log_path").and_then(Value::as_str).is_some());
}

fn assert_key_set(mut actual: Vec<&str>, expected: &[&str]) {
    let mut expected = expected.to_vec();
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected);
}
