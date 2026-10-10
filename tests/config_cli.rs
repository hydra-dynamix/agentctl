use assert_cmd::Command;
use serde_json::Value;

#[test]
fn config_generate_and_check_round_trips_through_cli() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");

    Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["config", "generate"])
        .assert()
        .success();

    Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["config", "check"])
        .assert()
        .success();

    let show_output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["config", "show"])
        .output()
        .expect("show config");

    assert!(show_output.status.success());
    let show = String::from_utf8(show_output.stdout).expect("show utf8");
    assert!(show.contains("[summary]"));
    assert!(show.contains("danger-full-access"));
    assert!(!show.contains("workspace-write"));
    for profile in [
        "[agents.codex]",
        "[agents.claude-code]",
        "[agents.claude]",
        "[agents.openai-rest]",
        "[agents.openai-websocket]",
        "[agents.ollama]",
        "[agents.openrouter]",
        "[agents.openrouter-api]",
        "[providers.openrouter]",
    ] {
        assert!(
            show.contains(profile),
            "generated config should include {profile}"
        );
    }
}

#[test]
fn config_check_uses_builtin_defaults_when_no_file_exists() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["config", "check"])
        .output()
        .expect("config check");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let normalized = stdout.replace('\\', "/");
    assert!(normalized.contains(".agentctl/config.toml"));
}

#[test]
fn top_level_help_lists_public_cli_surface() {
    let output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .arg("--help")
        .output()
        .expect("help output");

    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("help utf8");
    for command in [
        "exec", "run", "status", "list", "logs", "config", "discover",
    ] {
        assert!(help.contains(command), "help should list {command}");
    }
}

#[test]
fn version_is_available_for_compatibility_checks() {
    let output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .arg("--version")
        .output()
        .expect("version output");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("version utf8");
    assert!(stdout.starts_with("agentctl "));
}

#[test]
fn list_json_schema_is_empty_array_when_no_jobs_exist() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let project_dir = tempfile::tempdir().expect("project tempdir");

    Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["config", "generate"])
        .assert()
        .success();

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl binary")
        .current_dir(project_dir.path())
        .env("HOME", home_dir.path())
        .args(["list", "--json"])
        .output()
        .expect("list json");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("list json");
    assert_eq!(json.as_array().expect("array").len(), 0);
}

#[test]
#[cfg(windows)]
fn windows_uses_userprofile_when_home_is_missing() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    Command::cargo_bin("agentctl")
        .unwrap()
        .current_dir(project.path())
        .env_remove("HOME")
        .env("USERPROFILE", home.path())
        .args(["config", "generate"])
        .assert()
        .success();
    assert!(home.path().join(".agentctl/config.toml").exists());
    Command::cargo_bin("agentctl")
        .unwrap()
        .current_dir(project.path())
        .env_remove("HOME")
        .env("USERPROFILE", home.path())
        .args(["exec", "--json", "--", "python", "-c", "print('ok')"])
        .assert()
        .success();
}
