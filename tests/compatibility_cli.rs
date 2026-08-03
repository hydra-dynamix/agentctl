use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

#[test]
fn older_core_is_rejected_and_spooled_before_worker_startup() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let bin = tempfile::tempdir().expect("bin");
    write_old_ldgr(bin.path());
    fs::write(
        project.path().join("agentctl.toml"),
        runnable_ldgr_profile(),
    )
    .expect("agentctl config");

    let path = prepend_path(bin.path());
    let output = Command::cargo_bin("agentctl")
        .expect("agentctl")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("PATH", path)
        .args(["run", "ldgr-loop", "--prompt", "must not start"])
        .output()
        .expect("run");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr");
    assert!(
        stderr.contains("requires LDGR Core >=0.1.13, <0.2.0"),
        "{stderr}"
    );
    assert!(stderr.contains("found ldgr 0.1.12"), "{stderr}");
    assert!(
        stderr.contains("install a release bundle containing both binaries"),
        "{stderr}"
    );

    let recovery = only_recovery(project.path());
    assert_eq!(recovery["error"]["domain"], "agentctl.compatibility");
    assert_eq!(recovery["error"]["code"], "core-incompatible");
    assert_eq!(
        recovery["error"]["details"]["detected_core_version"],
        "0.1.12"
    );
}

fn only_recovery(project: &Path) -> Value {
    let inbox = project.join(".ldgr/recovery/inbox");
    let records = fs::read_dir(inbox)
        .expect("recovery inbox")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("core-incompatible.json"))
        })
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1, "{records:?}");
    serde_json::from_str(&fs::read_to_string(&records[0]).expect("recovery"))
        .expect("recovery JSON")
}

fn prepend_path(directory: &Path) -> std::ffi::OsString {
    let mut paths = vec![directory.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).expect("PATH")
}

#[cfg(windows)]
fn write_old_ldgr(directory: &Path) {
    fs::write(
        directory.join("ldgr.cmd"),
        "@echo off\r\nif \"%1\"==\"--version\" (echo ldgr 0.1.12& exit /b 0)\r\necho unknown compatibility command 1>&2\r\nexit /b 2\r\n",
    )
    .expect("fake old ldgr");
}

#[cfg(unix)]
fn write_old_ldgr(directory: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join("ldgr");
    fs::write(
        &path,
        "#!/bin/sh\nif [ \"${1:-}\" = \"--version\" ]; then echo 'ldgr 0.1.12'; exit 0; fi\necho 'unknown compatibility command' >&2\nexit 2\n",
    )
    .expect("fake old ldgr");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("permissions");
}

#[cfg(windows)]
fn runnable_ldgr_profile() -> &'static str {
    r#"
[summary]
max_output_bytes = 1024
tail_bytes = 256
max_preview_lines = 4

[agents.ldgr-loop]
command = ["cmd", "/C", "echo worker-started"]
prompt_stdin = true
"#
}

#[cfg(unix)]
fn runnable_ldgr_profile() -> &'static str {
    r#"
[summary]
max_output_bytes = 1024
tail_bytes = 256
max_preview_lines = 4

[agents.ldgr-loop]
command = ["sh", "-c", "echo worker-started"]
prompt_stdin = true
"#
}
