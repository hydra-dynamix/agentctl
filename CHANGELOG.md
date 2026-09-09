# Changelog

## 0.1.3 - 2026-09-08

- Allow Codex workers to write Git metadata by default using full access.
- Add `agentctl run codex --sandbox` to opt into the restricted workspace sandbox for foreground and detached workers.

## 0.1.2 - 2026-07-30

- Persist supervisor-owned execution intents and first-class recovery envelopes before worker startup.
- Detect native Windows homes, executables, process state, and process trees without Unix shims.
- Add `agentctl discover` with LDGR harness and Core compatibility diagnostics.
- Negotiate `ldgr.launcher-compatibility.v1` before an LDGR loop worker starts; incompatible or older Core binaries now produce an actionable durable `agentctl.compatibility/core-incompatible` error.
- Publish checksum-covered binaries for Linux, macOS, and Windows for paired packaging with LDGR Core 0.1.13.
