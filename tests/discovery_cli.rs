use std::fs;

use assert_cmd::Command;
use serde_json::Value;

#[test]
fn discover_reports_platform_ldgr_toml_and_available_agents() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    fs::create_dir_all(home.path().join(".ldgr")).expect("ldgr dir");
    fs::write(
        home.path().join(".ldgr/config.toml"),
        r#"
schema_version = 1
default_harness = "codex"
selected_harnesses = ["codex", "claude"]
"#,
    )
    .expect("ldgr config");
    fs::write(
        project.path().join("agentctl.toml"),
        platform_discovery_config(),
    )
    .expect("agentctl config");

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl")
        .current_dir(project.path())
        .env("HOME", home.path())
        .args(["discover", "--json"])
        .output()
        .expect("discover");

    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).expect("report");
    assert_eq!(report["schema"], "agentctl.discovery.v1");
    assert_eq!(report["platform"]["os"], std::env::consts::OS);
    assert_eq!(report["platform"]["arch"], std::env::consts::ARCH);
    assert_eq!(report["ldgr"]["format"], "toml");
    assert_eq!(report["ldgr"]["default_harness"], "codex");
    assert_eq!(
        report["core_compatibility"]["agentctl_version"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        report["core_compatibility"]["required_core"],
        ">=0.1.13, <0.2.0"
    );
    assert_eq!(report["recommended_agent"], "codex");
    let agents = report["agents"].as_array().expect("agents");
    assert!(
        agents
            .iter()
            .any(|agent| agent["name"] == "codex" && agent["available"] == true)
    );
}

#[test]
fn config_check_uses_userprofile_when_home_is_missing() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl")
        .current_dir(project.path())
        .env_remove("AGENTCTL_HOME")
        .env_remove("HOME")
        .env("USERPROFILE", home.path())
        .args(["config", "check"])
        .output()
        .expect("config check");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains(".agentctl"));
}

#[test]
fn unavailable_explicit_agent_returns_diagnostics_and_recovery_options() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    fs::write(
        project.path().join("agentctl.toml"),
        r#"
[summary]
max_output_bytes = 1024
tail_bytes = 256
max_preview_lines = 4

[agents.broken]
command = ["definitely-not-an-agentctl-harness"]
prompt_stdin = true
"#,
    )
    .expect("agentctl config");

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl")
        .current_dir(project.path())
        .env("HOME", home.path())
        .args(["run", "broken", "--prompt", "test"])
        .output()
        .expect("run");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr");
    for expected in [
        "detected platform:",
        "LDGR selected harnesses:",
        "runnable configured agents:",
        "recovery options:",
        "agentctl discover",
        "ldgr install --harness",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr should contain {expected:?}: {stderr}"
        );
    }
}

#[test]
fn generic_ldgr_profile_falls_back_to_next_selected_runnable_harness() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let fake_core = tempfile::tempdir().expect("fake Core");
    write_compatible_ldgr(fake_core.path());
    fs::create_dir_all(home.path().join(".ldgr")).expect("ldgr dir");
    fs::write(
        home.path().join(".ldgr/config.toml"),
        r#"
schema_version = 1
default_harness = "pi"
selected_harnesses = ["pi", "codex"]
"#,
    )
    .expect("ldgr config");
    fs::write(
        project.path().join("agentctl.toml"),
        platform_fallback_config(),
    )
    .expect("agentctl config");

    let output = Command::cargo_bin("agentctl")
        .expect("agentctl")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("PATH", prepend_path(fake_core.path()))
        .args(["run", "ldgr-loop", "--prompt", "test", "--json"])
        .output()
        .expect("run");

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr");
    assert!(stderr.contains("requires `missing-pi`"));
    assert!(stderr.contains("ldgr-loop-codex"));
    let report: Value = serde_json::from_slice(&output.stdout).expect("run JSON");
    assert!(
        report[0]["summary"]["preview_lines"]
            .to_string()
            .contains("fallback-codex")
    );
}

fn prepend_path(directory: &std::path::Path) -> std::ffi::OsString {
    let mut paths = vec![directory.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).expect("PATH")
}

#[cfg(windows)]
fn write_compatible_ldgr(directory: &std::path::Path) {
    fs::write(
        directory.join("compatibility.json"),
        r#"{"schema":"ldgr.launcher-compatibility.v1","compatible":true,"core_version":"0.1.13","core_executable":"ldgr.cmd","agentctl_version":"0.1.2","agentctl_requirement":">=0.1.2, <0.2.0","error_recovery_schema":1}"#,
    )
    .expect("compatibility JSON");
    fs::write(
        directory.join("ldgr.cmd"),
        "@echo off\r\nif \"%1\"==\"--version\" (echo ldgr 0.1.13& exit /b 0)\r\nif \"%1\"==\"compatibility\" (type \"%~dp0compatibility.json\"& exit /b 0)\r\nexit /b 2\r\n",
    )
    .expect("fake compatible ldgr");
}

#[cfg(unix)]
fn write_compatible_ldgr(directory: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join("ldgr");
    fs::write(
        &path,
        "#!/bin/sh\nif [ \"${1:-}\" = \"--version\" ]; then echo 'ldgr 0.1.13'; exit 0; fi\nif [ \"${1:-}\" = \"compatibility\" ]; then printf '%s\\n' '{\"schema\":\"ldgr.launcher-compatibility.v1\",\"compatible\":true,\"core_version\":\"0.1.13\",\"core_executable\":\"ldgr\",\"agentctl_version\":\"0.1.2\",\"agentctl_requirement\":\">=0.1.2, <0.2.0\",\"error_recovery_schema\":1}'; exit 0; fi\nexit 2\n",
    )
    .expect("fake compatible ldgr");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("permissions");
}

#[cfg(unix)]
fn platform_discovery_config() -> &'static str {
    r#"
[summary]
max_output_bytes = 1024
tail_bytes = 256
max_preview_lines = 4

[agents.codex]
command = ["sh", "-c", "echo codex"]
prompt_stdin = true
"#
}

#[cfg(windows)]
fn platform_discovery_config() -> &'static str {
    r#"
[summary]
max_output_bytes = 1024
tail_bytes = 256
max_preview_lines = 4

[agents.codex]
command = ["cmd", "/C", "echo codex"]
prompt_stdin = true
"#
}

#[cfg(unix)]
fn platform_fallback_config() -> &'static str {
    r#"
[summary]
max_output_bytes = 1024
tail_bytes = 256
max_preview_lines = 4

[agents.ldgr-loop]
command = ["missing-pi"]
prompt_stdin = true

[agents.ldgr-loop-codex]
command = ["sh", "-c", "echo fallback-codex"]
prompt_stdin = true
"#
}

#[cfg(windows)]
fn platform_fallback_config() -> &'static str {
    r#"
[summary]
max_output_bytes = 1024
tail_bytes = 256
max_preview_lines = 4

[agents.ldgr-loop]
command = ["missing-pi"]
prompt_stdin = true

[agents.ldgr-loop-codex]
command = ["cmd", "/C", "echo fallback-codex"]
prompt_stdin = true
"#
}
