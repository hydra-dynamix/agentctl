#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use assert_cmd::Command;

fn executable(path: &std::path::Path, contents: &str) {
    std::fs::write(path, contents).expect("write fake executable");
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn pr_review_resolves_exact_head_and_launches_policy_prompt() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let requirements = project.path().join("requirements.md");
    std::fs::write(&requirements, "REQ-7 preserve history").unwrap();
    std::fs::write(
        project.path().join("agentctl.toml"),
        r#"
[agents.reviewer]
command = ["python", "-c", "import pathlib,sys; pathlib.Path('review-prompt.txt').write_text(sys.stdin.read()); print('review launched')"]
prompt_stdin = true
"#,
    )
    .unwrap();
    executable(
        &bin.path().join("git"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in\n  'rev-parse --show-toplevel') printf '%s\\n' '{}' ;;\n  'rev-parse HEAD') echo abc123 ;;\n  *) exit 2 ;;\nesac\n",
            project.path().display()
        ),
    );
    executable(
        &bin.path().join("gh"),
        r#"#!/bin/sh
if [ "$1 $2 $3 $4" = "repo view --json nameWithOwner" ]; then
  echo '{"nameWithOwner":"owner/project"}'
elif [ "$1" = "api" ]; then
  echo '[{"number":42,"html_url":"https://example.test/pull/42","title":"Feature","state":"open","head":{"sha":"abc123"}}]'
else
  exit 2
fi
"#,
    );
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::cargo_bin("agentctl")
        .unwrap()
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("PATH", path)
        .args([
            "task",
            "pr-review",
            "--agent",
            "reviewer",
            "--requirements",
            requirements.to_str().unwrap(),
            "--confidence-threshold",
            "93",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let prompt = std::fs::read_to_string(project.path().join("review-prompt.txt")).unwrap();
    assert!(prompt.contains("PR: #42"));
    assert!(prompt.contains("reviewed head SHA: abc123"));
    assert!(prompt.contains("confidence is at least 93%"));
    assert!(prompt.contains("REQ-7 preserve history"));
    assert!(prompt.contains("--auto --merge --match-head-commit abc123"));
}

#[test]
fn pr_review_fails_when_no_open_pr_exactly_matches_head() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("agentctl.toml"),
        r#"
[agents.reviewer]
command = ["python", "-c", "import pathlib; pathlib.Path('should-not-run').write_text('ran')"]
prompt_stdin = true
"#,
    )
    .unwrap();
    executable(
        &bin.path().join("git"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in\n  'rev-parse --show-toplevel') printf '%s\\n' '{}' ;;\n  'rev-parse HEAD') echo abc123 ;;\n  *) exit 2 ;;\nesac\n",
            project.path().display()
        ),
    );
    executable(
        &bin.path().join("gh"),
        r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo '{"nameWithOwner":"owner/project"}'
elif [ "$1" = "api" ]; then
  echo '[]'
else
  exit 2
fi
"#,
    );
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::cargo_bin("agentctl")
        .unwrap()
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("PATH", path)
        .args(["task", "pr-review", "--agent", "reviewer"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("no open pull request has exact head commit abc123")
    );
    assert!(!project.path().join("should-not-run").exists());
}
