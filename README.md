# agentctl

`agentctl` is a small Rust library and CLI for giving LLM coding agents compact,
bounded reports from noisy commands and long-running agent tasks.

The common use case is a tool loop around Claude Code, Codex, or another agent:
run the expensive command normally, keep the full raw log on disk, and return a
short structured summary that is cheap for the model to read.

## Install

```sh
cargo install --git https://github.com/hydra-dynamix/agentctl agentctl
```

Generate a starter config:

```sh
agentctl config generate
agentctl config check
```

`agentctl` reads `./agentctl.toml` first, then falls back to
`~/.agentctl/config.toml`. Raw command logs and detached job records live under
`~/.agentctl/jobs/`.

## Wrap a noisy command

Use `exec` when an agent wants to run a command but should receive only the
important lines:

```sh
agentctl exec --json --label cargo-test -- cargo test
```

The exit code is the child process exit code, so existing tool-loop error
handling still works. A failing `cargo test` returns a compact JSON summary:

```json
{
  "command": "cargo test",
  "exit_code": 101,
  "duration_ms": 1842,
  "preview_lines": [
    "failures:",
    "test result: FAILED. 12 passed; 1 failed"
  ],
  "raw_log_path": "/home/me/.agentctl/jobs/1780000000-1234-cargo-test/output.log"
}
```

The full stdout and stderr remain in `raw_log_path` for later inspection. Use
`--raw` when you deliberately want the untouched command output streamed back.

## Run a detached agent task

Configure agents in TOML:

```toml
[summary]
max_output_bytes = 16384
tail_bytes = 4096
max_preview_lines = 12

[agents.codex]
command = ["codex", "exec", "--sandbox", "workspace-write"]
prompt_stdin = true

[agents.claude-code]
command = ["claude", "-p"]
prompt_stdin = false

# Alias for Claude Code.
[agents.claude]
command = ["claude", "-p"]
prompt_stdin = false

# Provider-specific wrappers are external commands. Replace these placeholders
# with your chosen REST/websocket bridge scripts or binaries.
[agents.openai-rest]
command = ["openai-rest-agent"]
prompt_stdin = true

[agents.openai-websocket]
command = ["openai-websocket-agent"]
prompt_stdin = true

[agents.ollama]
command = ["ollama", "run", "llama3"]
prompt_stdin = true
```

Start a long task without keeping the caller attached:

```sh
agentctl run codex \
  --prompt "Run cargo test, fix failures, and report the files changed." \
  --cwd /path/to/project \
  --detach \
  --json

# codex is the default agent, and stdin is accepted as the prompt:
printf 'Run cargo test and summarize the result.' | agentctl run --json
```

That returns a job record:

```json
{
  "id": "1780000000-1234-codex",
  "agent": "codex",
  "state": "running",
  "pid": 12345,
  "started_at": 1780000000,
  "finished_at": null,
  "exit_code": null,
  "command": "codex exec --sandbox workspace-write",
  "cwd": "/path/to/project",
  "iterations": 1,
  "log_path": "/home/me/.agentctl/jobs/1780000000-1234-codex/output.log"
}
```

Poll and inspect it later:

```sh
agentctl status 1780000000-1234-codex --json
agentctl logs 1780000000-1234-codex
agentctl list --json
```

`logs` is intentionally the raw-output command. `status` and `list` stay bounded
and machine-readable for agent loops.

## JSON contract

Every public command that has `--json` uses stable field names.

Summary:

```json
{
  "command": "string",
  "exit_code": 0,
  "duration_ms": 0,
  "preview_lines": ["string"],
  "raw_log_path": "/path/to/output.log"
}
```

Foreground `run --json` returns an array of iteration reports:

```json
[
  {
    "iteration": 1,
    "iterations": 2,
    "summary": {}
  }
]
```

Detached `run --json` and `list --json` use job records:

```json
{
  "id": "string",
  "agent": "codex",
  "state": "running",
  "pid": 12345,
  "started_at": 1780000000,
  "finished_at": null,
  "exit_code": null,
  "command": "codex exec --sandbox workspace-write",
  "cwd": "/path/to/project",
  "iterations": 1,
  "log_path": "/path/to/output.log"
}
```

`state` is one of `running`, `succeeded`, `failed`, or `stale`.

Status wraps a job record with bounded log tail lines:

```json
{
  "record": {},
  "tail_lines": ["string"]
}
```

These shapes are semver-relevant.

## Library

The crate also exposes the capture, summary, config, and job modules used by the
CLI. The public API is intentionally small so other local tooling can build the
same bounded reports without shelling out.

## Provider boundary

`agentctl` is intentionally a wrapper, not a provider SDK. Codex, Claude Code,
OpenAI REST, OpenAI websocket/realtime, Ollama, or other provider-specific
clients live in their own CLIs/scripts. `agentctl` only decides how to pass the
prompt, capture output, summarize logs, and track detached jobs.
