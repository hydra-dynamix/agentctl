# Changelog

## 0.1.5 - 2026-10-10

- Add native OpenRouter-compatible Chat Completions profiles with environment or command-based API keys.
- Give each Linux capture a dedicated subreaper, preserving host process ownership while cleaning detached descendants and zombies.
- Assign Windows workers to Job Objects before execution and prevent detached supervisors from holding caller output pipes open.
- Preserve failed foreground reports and exit codes, bound external summaries and status tails, and prevent detached job ID collisions.
- Retain discovery, Core compatibility, and durable error capture; validate provider execution against a live compatible endpoint.

## 0.1.4 - 2026-09-17

- Reap successfully exited adopted descendants throughout active Linux agent runs instead of retaining them as zombies until the direct agent exits.
- Preserve direct-child exit status ownership while processing `SIGCHLD`, with a regression test for long-running agents that launch detached helpers.

## 0.1.3 - 2026-09-08

- Allow Codex workers to write Git metadata by default using full access.
- Add `agentctl run codex --sandbox` to opt into the restricted workspace sandbox for foreground and detached workers.

## 0.1.2 - 2026-07-30

- Persist supervisor-owned execution intents and first-class recovery envelopes before worker startup.
- Detect native Windows homes, executables, process state, and process trees without Unix shims.
- Add `agentctl discover` with LDGR harness and Core compatibility diagnostics.
- Negotiate `ldgr.launcher-compatibility.v1` before an LDGR loop worker starts; incompatible or older Core binaries now produce an actionable durable `agentctl.compatibility/core-incompatible` error.
- Publish checksum-covered binaries for Linux, macOS, and Windows for paired packaging with LDGR Core 0.1.13.
