use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

#[test]
fn invalid_config_is_captured_before_worker_startup() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    fs::write(project.path().join("agentctl.toml"), "not = [valid").expect("invalid config");

    agentctl(project.path(), home.path())
        .args(["run", "broken", "--prompt", "redacted prompt"])
        .assert()
        .failure();

    let recovery = only_recovery(project.path());
    assert_eq!(recovery["format"], "ldgr-error-recovery");
    assert_eq!(recovery["error"]["code"], "configuration-invalid");
    assert!(
        !serde_json::to_string(&recovery)
            .expect("json")
            .contains("redacted prompt")
    );
}

#[test]
fn spawn_and_nonzero_exit_are_captured_outside_the_worker() {
    let home = tempfile::tempdir().expect("home");

    let spawn_project = tempfile::tempdir().expect("spawn project");
    fs::write(
        spawn_project.path().join("agentctl.toml"),
        format!(
            "{}\ncwd = \"missing-worker-directory\"\n",
            agent_config(spawnable_command())
        ),
    )
    .expect("spawn config");
    agentctl(spawn_project.path(), home.path())
        .args(["run", "fixture", "--prompt", "secret"])
        .assert()
        .failure();
    assert_eq!(
        only_recovery(spawn_project.path())["error"]["code"],
        "worker-spawn-failed"
    );

    let exit_project = tempfile::tempdir().expect("exit project");
    fs::write(
        exit_project.path().join("agentctl.toml"),
        agent_config(nonzero_command()),
    )
    .expect("exit config");
    agentctl(exit_project.path(), home.path())
        .args(["run", "fixture", "--prompt", "secret"])
        .assert()
        .failure();
    let recovery = only_recovery(exit_project.path());
    assert_eq!(recovery["error"]["code"], "nonzero-exit");
    assert_eq!(recovery["error"]["details"]["exit_code"], 7);
}

#[test]
fn no_writable_sink_fails_closed_before_config_or_worker_access() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let blocked_state = project.path().join("blocked-state");
    fs::write(project.path().join(".ldgr"), "not a directory").expect("block project sink");
    fs::write(&blocked_state, "not a directory").expect("block user sink");

    let output = agentctl(project.path(), home.path())
        .env("LOCALAPPDATA", &blocked_state)
        .env("XDG_STATE_HOME", &blocked_state)
        .args(["run", "worker-must-not-start", "--prompt", "secret"])
        .output()
        .expect("run fail-closed fixture");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "FATAL: no durable LDGR recovery sink is writable; worker execution was not started"
        ),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn agentctl(project: &Path, home: &Path) -> Command {
    let mut command = Command::cargo_bin("agentctl").expect("agentctl binary");
    command.current_dir(project).env("HOME", home);
    command
}

fn only_recovery(project: &Path) -> Value {
    let inbox = project.join(".ldgr/recovery/inbox");
    let records = fs::read_dir(&inbox)
        .expect("recovery inbox")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1, "{records:?}");
    serde_json::from_str(&fs::read_to_string(&records[0]).expect("recovery record"))
        .expect("recovery json")
}

fn agent_config(command: &str) -> String {
    format!(
        r#"
[summary]
max_output_bytes = 16384
tail_bytes = 4096
max_preview_lines = 12

[agents.fixture]
command = {command}
prompt_stdin = false
"#
    )
}

#[cfg(windows)]
fn nonzero_command() -> &'static str {
    r#"["cmd", "/C", "exit 7"]"#
}

#[cfg(windows)]
fn spawnable_command() -> &'static str {
    r#"["cmd", "/C", "echo worker-must-not-start"]"#
}

#[cfg(unix)]
fn spawnable_command() -> &'static str {
    r#"["sh", "-c", "echo worker-must-not-start"]"#
}

#[cfg(unix)]
fn nonzero_command() -> &'static str {
    r#"["sh", "-c", "exit 7"]"#
}
