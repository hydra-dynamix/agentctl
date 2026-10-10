use assert_cmd::Command;
use serde_json::Value;

fn generate_config(home_dir: &tempfile::TempDir, project_dir: &tempfile::TempDir) {
    Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["config", "generate"])
        .assert()
        .success();
}

#[test]
fn exec_json_reports_success_and_writes_raw_log() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");
    generate_config(&home_dir, &project_dir);

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args([
            "exec",
            "--json",
            "--label",
            "version",
            "--",
            "cargo",
            "--version",
        ])
        .output()
        .expect("run agentctl exec");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("json summary");
    assert_summary_schema(&json);
    assert_eq!(json.get("exit_code").and_then(Value::as_i64), Some(0));
    assert_eq!(
        json.get("command").and_then(Value::as_str),
        Some("cargo --version")
    );
    let raw_log_path = json["raw_log_path"].as_str().expect("raw log path");
    let raw_log = std::fs::read_to_string(raw_log_path).expect("raw log");
    assert!(raw_log.contains("cargo"));
}

#[test]
fn exec_passthrough_failing_exit_code_and_summary() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");
    generate_config(&home_dir, &project_dir);

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args([
            "exec",
            "--json",
            "--",
            "python",
            "-c",
            "import sys; print('warning: careful', file=sys.stderr); sys.exit(7)",
        ])
        .output()
        .expect("run failing agentctl exec");

    assert_eq!(output.status.code(), Some(7));
    let json: Value = serde_json::from_slice(&output.stdout).expect("json summary");
    assert_summary_schema(&json);
    assert_eq!(json.get("exit_code").and_then(Value::as_i64), Some(7));
    assert_eq!(
        json.pointer("/preview_lines/0").and_then(Value::as_str),
        Some("warning: careful")
    );
}

#[test]
fn exec_raw_streams_child_output_without_summary_json() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");
    generate_config(&home_dir, &project_dir);

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args([
            "exec",
            "--raw",
            "--json",
            "--",
            "python",
            "-c",
            "import sys; sys.stdout.write('raw-out'); sys.stderr.write('raw-err')",
        ])
        .output()
        .expect("run raw agentctl exec");

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).expect("stdout"), "raw-out");
    assert_eq!(String::from_utf8(output.stderr).expect("stderr"), "raw-err");
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

#[test]
fn external_summarizer_respects_output_limits() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("agentctl.toml"), r#"
[summary]
max_output_bytes = 256
tail_bytes = 64
max_preview_lines = 2
summarizer_command = ["python", "-c", "import sys; sys.stdin.read(); print('summary-line\\n'*100000)"]
"#).unwrap();
    let output = Command::cargo_bin("agentctl")
        .unwrap()
        .current_dir(project.path())
        .env("HOME", home.path())
        .args(["exec", "--json", "--", "python", "-c", "print('hello')"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    let lines = summary["preview_lines"].as_array().unwrap();
    assert_eq!(lines[0], "summary-line");
    assert!(lines.len() <= 2);
    assert!(
        lines
            .iter()
            .map(|line| line.as_str().unwrap().len())
            .sum::<usize>()
            <= 256
    );
}
