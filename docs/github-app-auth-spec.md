# Automatic GitHub App authentication

Source: operator request for one CI command using an organization-wide GitHub App and credentials stored behind command-based secret retrieval.

## Configuration

```toml
[github_app]
app_id_command = ["op-vault", "get", "AGENTCTL_GH_APP_ID", "--account", "production"]
private_key_command = ["op-vault", "get", "AGENTCTL_GH_PRIVATE_KEY", "--account", "production"]
api_url = "https://api.github.com" # optional default
```

Both commands are required when the section exists and must contain no empty arguments. Their stdout is trimmed for the App ID and preserved except for surrounding whitespace for the PEM key. Command stderr may be used in a bounded error but secret stdout must never be logged, persisted, placed in process arguments, or included in job records.

## PR review behavior

When `[github_app]` is configured, `agentctl task pr-review`:

1. Resolves the Git repository root, exact `HEAD`, and `origin` owner/repository without GitHub authentication.
2. Executes the configured secret commands.
3. Signs an RS256 GitHub App JWT in memory.
4. Resolves the App installation for the current repository.
5. Creates a one-hour-or-shorter installation token scoped to only the current repository.
6. Supplies that token as `GH_TOKEN` only to `gh` discovery commands and the launched review agent, including detached supervisors.
7. Never writes the JWT, private key, or installation token to task prompts, summaries, logs, supervisor specifications, or job records.

If `[github_app]` is absent, existing ambient `gh` authentication behavior remains unchanged. Authentication failures stop before agent launch.

Repository parsing supports standard HTTPS and SSH GitHub origin URLs. Enterprise instances may set `api_url`; the origin host is not rewritten automatically.

## Validation

Tests cover configuration validation, origin parsing, JWT claims without exposing credentials, mocked installation/token HTTP responses, environment forwarding, and task CLI behavior. Formatting, lint, and all tests must pass.
