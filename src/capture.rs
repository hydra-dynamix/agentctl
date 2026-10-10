use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::platform;

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

const NO_SINK_DIAGNOSTIC: &str =
    "FATAL: no durable LDGR recovery sink is writable; worker execution was not started";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionAttempt {
    pub operation_id: String,
    pub attempt_id: String,
    observed_at: String,
    project_root: PathBuf,
    intent_path: PathBuf,
}

#[derive(Debug, Clone)]
pub enum FailureKind {
    Configuration,
    Environment,
    CoreIncompatible {
        detected_core_version: Option<String>,
    },
    Spawn,
    ExitCode(i32),
    Signal(i32),
    UnexpectedDisappearance,
}

#[derive(Debug, Clone, Serialize)]
struct ProjectRef {
    project_id: Option<String>,
    locator: String,
    database_identity: Option<String>,
}

#[derive(Debug, Serialize)]
struct IntentEnvelope<'a> {
    format: &'static str,
    schema_version: u32,
    project: &'a ProjectRef,
    producer: &'static str,
    operation_id: &'a str,
    attempt_id: &'a str,
    boundary: &'static str,
    subject: &'a str,
    environment: Value,
    accepted_at: String,
    process_id: u32,
}

#[derive(Debug, Serialize)]
struct Fingerprint {
    version: &'static str,
    value: String,
    inputs: FingerprintInputs,
}

#[derive(Debug, Serialize)]
struct FingerprintInputs {
    class: &'static str,
    domain: &'static str,
    code: &'static str,
    boundary: &'static str,
    component: &'static str,
    subject: &'static str,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    class: &'static str,
    domain: &'static str,
    code: &'static str,
    severity: &'static str,
    retryability: &'static str,
    source: &'static str,
    summary: &'static str,
    details: Value,
    environment: Value,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    format: &'static str,
    schema_version: u32,
    project: &'a ProjectRef,
    producer: &'static str,
    idempotency_key: String,
    operation_id: &'a str,
    attempt_id: &'a str,
    occurrence_id: String,
    fingerprint: Fingerprint,
    error: ErrorBody,
    observed_at: String,
}

impl ExecutionAttempt {
    /// Persist acceptance before configuration discovery or process creation.
    pub fn begin(project_hint: &Path, subject: &str) -> Result<Self> {
        let operation_id = uuid_v7()?;
        let attempt_id = uuid_v7()?;
        let project_root = discover_project_root(project_hint);
        let project = project_ref(&project_root);
        let environment = allowlisted_environment();
        let accepted_at = observed_at();
        let envelope = IntentEnvelope {
            format: "ldgr-execution-intent",
            schema_version: 1,
            project: &project,
            producer: "agentctl",
            operation_id: &operation_id,
            attempt_id: &attempt_id,
            boundary: "agent-launch",
            subject,
            environment,
            accepted_at: accepted_at.clone(),
            process_id: std::process::id(),
        };
        let bytes = serde_json::to_vec_pretty(&envelope)?;
        let filename = format!("{attempt_id}.intent.json");
        let intent_path = write_to_first_sink(&project_root, &filename, &bytes)
            .map_err(|error| anyhow!("{NO_SINK_DIAGNOSTIC}: {error:#}"))?;
        Ok(Self {
            operation_id,
            attempt_id,
            observed_at: accepted_at,
            project_root,
            intent_path,
        })
    }

    pub fn record_error(&self, failure: FailureKind) -> Result<()> {
        if !self.intent_path.is_file() {
            return Ok(());
        }
        let classification = classification(&failure);
        let occurrence_id = deterministic_occurrence_id(&self.attempt_id, classification.code);
        let inputs = FingerprintInputs {
            class: classification.class,
            domain: classification.domain,
            code: classification.code,
            boundary: classification.boundary,
            component: "agentctl",
            subject: "agent-worker",
        };
        let fingerprint = fingerprint(&inputs)?;
        let project = project_ref(&self.project_root);
        let environment = allowlisted_environment();
        let details = match &failure {
            FailureKind::CoreIncompatible {
                detected_core_version,
            } => json!({
                "agentctl_version": env!("CARGO_PKG_VERSION"),
                "detected_core_version": detected_core_version,
                "required_core": crate::compatibility::REQUIRED_CORE,
                "compatibility_schema": crate::compatibility::COMPATIBILITY_SCHEMA,
            }),
            FailureKind::ExitCode(code) => json!({ "exit_code": code }),
            FailureKind::Signal(signal) => json!({ "signal": signal }),
            _ => json!({}),
        };
        let observed_at = self.observed_at.clone();
        let idempotency_key = format!("{}:{}", self.attempt_id, classification.code);
        let envelope = ErrorEnvelope {
            format: "ldgr-error-recovery",
            schema_version: 1,
            project: &project,
            producer: "agentctl",
            idempotency_key: idempotency_key.clone(),
            operation_id: &self.operation_id,
            attempt_id: &self.attempt_id,
            occurrence_id: occurrence_id.clone(),
            fingerprint: Fingerprint {
                version: "structured-v1",
                value: fingerprint.clone(),
                inputs,
            },
            error: ErrorBody {
                class: classification.class,
                domain: classification.domain,
                code: classification.code,
                severity: "error",
                retryability: classification.retryability,
                source: classification.source,
                summary: classification.summary,
                details: details.clone(),
                environment: environment.clone(),
            },
            observed_at: observed_at.clone(),
        };

        if record_with_core(
            &self.project_root,
            CoreOccurrence {
                occurrence_id: &occurrence_id,
                idempotency_key: &idempotency_key,
                operation_id: &self.operation_id,
                attempt_id: &self.attempt_id,
                class: classification.class,
                domain: classification.domain,
                code: classification.code,
                boundary: classification.boundary,
                retryability: classification.retryability,
                source: classification.source,
                summary: classification.summary,
                details: &details,
                environment: &environment,
                observed_at: &observed_at,
            },
        ) {
            self.remove_intent();
            return Ok(());
        }

        let filename = format!("{}-{}.json", self.attempt_id, classification.code);
        let bytes = serde_json::to_vec_pretty(&envelope)?;
        write_to_first_sink(&self.project_root, &filename, &bytes).map_err(|error| {
            anyhow!(
                "FATAL: accepted worker failure could not be written to any durable LDGR sink: {error:#}"
            )
        })?;
        self.remove_intent();
        Ok(())
    }

    pub fn complete(&self) {
        self.remove_intent();
    }

    fn remove_intent(&self) {
        let _ = fs::remove_file(&self.intent_path);
    }
}

struct Classification {
    class: &'static str,
    domain: &'static str,
    code: &'static str,
    boundary: &'static str,
    retryability: &'static str,
    source: &'static str,
    summary: &'static str,
}

fn classification(failure: &FailureKind) -> Classification {
    match failure {
        FailureKind::Configuration => Classification {
            class: "infrastructure-error",
            domain: "agentctl.bootstrap",
            code: "configuration-invalid",
            boundary: "config-discovery",
            retryability: "after-change",
            source: "agentctl:config-discovery",
            summary: "Agent configuration could not be loaded before worker startup.",
        },
        FailureKind::Environment => Classification {
            class: "infrastructure-error",
            domain: "agentctl.bootstrap",
            code: "environment-invalid",
            boundary: "environment",
            retryability: "after-change",
            source: "agentctl:environment",
            summary: "The supervisor environment was invalid before worker startup.",
        },
        FailureKind::CoreIncompatible { .. } => Classification {
            class: "infrastructure-error",
            domain: "agentctl.compatibility",
            code: "core-incompatible",
            boundary: "core-negotiation",
            retryability: "after-change",
            source: "agentctl:core-negotiation",
            summary: "The installed LDGR Core is incompatible with this agentctl release.",
        },
        FailureKind::Spawn => Classification {
            class: "infrastructure-error",
            domain: "agentctl.spawn",
            code: "worker-spawn-failed",
            boundary: "worker-spawn",
            retryability: "after-change",
            source: "agentctl:worker-spawn",
            summary: "The supervisor could not start the configured worker.",
        },
        FailureKind::ExitCode(_) => Classification {
            class: "task-failure",
            domain: "agentctl.worker",
            code: "nonzero-exit",
            boundary: "worker-exit",
            retryability: "unknown",
            source: "agentctl:worker-exit",
            summary: "The worker returned a nonzero exit code.",
        },
        FailureKind::Signal(_) => Classification {
            class: "interruption",
            domain: "agentctl.worker",
            code: "signal",
            boundary: "worker-exit",
            retryability: "unknown",
            source: "agentctl:worker-exit",
            summary: "The worker was terminated by a signal.",
        },
        FailureKind::UnexpectedDisappearance => Classification {
            class: "interruption",
            domain: "agentctl.supervisor",
            code: "unexpected-disappearance",
            boundary: "supervisor-reconciliation",
            retryability: "after-change",
            source: "agentctl:status",
            summary: "A supervised process disappeared without a terminal record.",
        },
    }
}

struct CoreOccurrence<'a> {
    occurrence_id: &'a str,
    idempotency_key: &'a str,
    operation_id: &'a str,
    attempt_id: &'a str,
    class: &'a str,
    domain: &'a str,
    code: &'a str,
    boundary: &'a str,
    retryability: &'a str,
    source: &'a str,
    summary: &'a str,
    details: &'a Value,
    environment: &'a Value,
    observed_at: &'a str,
}

fn record_with_core(project_root: &Path, occurrence: CoreOccurrence<'_>) -> bool {
    let db = project_root.join(".ldgr/ldgr.db");
    if !db.is_file() || platform::command_path("ldgr").is_none() {
        return false;
    }
    let details = match serde_json::to_string(occurrence.details) {
        Ok(value) => value,
        Err(_) => return false,
    };
    let environment = match serde_json::to_string(occurrence.environment) {
        Ok(value) => value,
        Err(_) => return false,
    };
    Command::new("ldgr")
        .arg("--db")
        .arg(db)
        .args([
            "error",
            "record",
            "--occurrence-id",
            occurrence.occurrence_id,
            "--producer",
            "agentctl",
            "--idempotency-key",
            occurrence.idempotency_key,
            "--operation-id",
            occurrence.operation_id,
            "--attempt-id",
            occurrence.attempt_id,
            "--class",
            occurrence.class,
            "--domain",
            occurrence.domain,
            "--code",
            occurrence.code,
            "--boundary",
            occurrence.boundary,
            "--component",
            "agentctl",
            "--subject",
            "agent-worker",
            "--severity",
            "error",
            "--retryability",
            occurrence.retryability,
            "--source",
            occurrence.source,
            "--summary",
            occurrence.summary,
            "--details",
            &details,
            "--environment",
            &environment,
            "--observed-at",
            occurrence.observed_at,
            "--recovery-origin",
            "database",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn write_to_first_sink(project_root: &Path, filename: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut failures = Vec::new();
    for directory in recovery_directories(project_root) {
        match atomic_write(&directory, filename, bytes) {
            Ok(path) => return Ok(path),
            Err(error) => failures.push(format!("{}: {error:#}", directory.display())),
        }
    }
    bail!("{}", failures.join("; "))
}

fn recovery_directories(project_root: &Path) -> Vec<PathBuf> {
    let mut directories = vec![project_root.join(".ldgr/recovery/inbox")];
    if let Some(user) = user_recovery_directory()
        && !directories.contains(&user)
    {
        directories.push(user);
    }
    directories
}

fn user_recovery_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        nonempty_var("LOCALAPPDATA").map(|root| PathBuf::from(root).join("ldgr/recovery/inbox"))
    }
    #[cfg(not(windows))]
    {
        nonempty_var("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| nonempty_var("HOME").map(|home| PathBuf::from(home).join(".local/state")))
            .map(|root| root.join("ldgr/recovery/inbox"))
    }
}

fn atomic_write(directory: &Path, filename: &str, bytes: &[u8]) -> Result<PathBuf> {
    fs::create_dir_all(directory)
        .with_context(|| format!("creating recovery directory {}", directory.display()))?;
    let destination = directory.join(filename);
    if fs::read(&destination).is_ok_and(|existing| existing == bytes) {
        return Ok(destination);
    }
    let temporary = directory.join(format!(
        ".{filename}.{}.{}.tmp",
        std::process::id(),
        UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", temporary.display()))?;
    drop(file);
    if let Err(error) = fs::rename(&temporary, &destination) {
        if fs::read(&destination).is_ok_and(|existing| existing == bytes) {
            let _ = fs::remove_file(&temporary);
            return Ok(destination);
        }
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| {
            format!(
                "atomically publishing {} as {}",
                temporary.display(),
                destination.display()
            )
        });
    }
    #[cfg(unix)]
    {
        fs::File::open(directory)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing recovery directory {}", directory.display()))?;
    }
    Ok(destination)
}

fn discover_project_root(hint: &Path) -> PathBuf {
    if hint.is_absolute() {
        hint.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(hint)
    }
}

fn project_ref(project_root: &Path) -> ProjectRef {
    ProjectRef {
        project_id: fs::read_to_string(project_root.join(".ldgr/project-id"))
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        locator: redacted_locator(project_root),
        database_identity: None,
    }
}

fn redacted_locator(project_root: &Path) -> String {
    for name in ["HOME", "USERPROFILE"] {
        let Some(home) = nonempty_var(name).map(PathBuf::from) else {
            continue;
        };
        let Ok(relative) = project_root.strip_prefix(home) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            return "$HOME".to_owned();
        }
        return format!("$HOME/{}", relative.to_string_lossy().replace('\\', "/"));
    }
    project_root.to_string_lossy().replace('\\', "/")
}

fn allowlisted_environment() -> Value {
    json!({
        "os": env::consts::OS,
        "arch": env::consts::ARCH,
        "family": env::consts::FAMILY,
    })
}

fn fingerprint(inputs: &FingerprintInputs) -> Result<String> {
    let canonical = serde_json::to_vec(&serde_json::to_value(inputs)?)?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn uuid_v7() -> Result<String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis() as u64;
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow!("generating execution identity: {error}"))?;
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes(bytes[0..4].try_into().expect("four bytes")),
        u16::from_be_bytes(bytes[4..6].try_into().expect("two bytes")),
        u16::from_be_bytes(bytes[6..8].try_into().expect("two bytes")),
        u16::from_be_bytes(bytes[8..10].try_into().expect("two bytes")),
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ])
    ))
}

fn deterministic_occurrence_id(attempt_id: &str, code: &str) -> String {
    let digest = Sha256::digest(format!("agentctl:{attempt_id}:{code}"));
    let timestamp = attempt_id
        .chars()
        .filter(|character| *character != '-')
        .take(12)
        .collect::<String>();
    let random = format!("{digest:x}");
    let variant = match &random[3..4] {
        "0" | "1" | "2" | "3" => "8",
        "4" | "5" | "6" | "7" => "9",
        "8" | "9" | "a" | "b" => "a",
        _ => "b",
    };
    format!(
        "{}-{}-7{}-{}{}-{}",
        &timestamp[0..8],
        &timestamp[8..12],
        &random[0..3],
        variant,
        &random[4..7],
        &random[7..19],
    )
}

fn observed_at() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn nonempty_var(name: &str) -> Option<std::ffi::OsString> {
    env::var_os(OsStr::new(name)).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_and_error_payloads_exclude_prompt_text() -> Result<()> {
        let project = tempfile::tempdir()?;
        let attempt = ExecutionAttempt::begin(project.path(), "test-agent")?;
        let intent = fs::read_to_string(&attempt.intent_path)?;
        assert!(intent.contains("ldgr-execution-intent"));
        assert!(!intent.contains("secret prompt"));

        attempt.record_error(FailureKind::Spawn)?;
        let inbox = project.path().join(".ldgr/recovery/inbox");
        let recovery = fs::read_dir(inbox)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.extension() == Some(OsStr::new("json")))
            .context("recovery record")?;
        let payload = fs::read_to_string(recovery)?;
        assert!(payload.contains("worker-spawn-failed"));
        assert!(!payload.contains("secret prompt"));
        Ok(())
    }

    #[test]
    fn generated_identity_has_uuid_v7_shape() -> Result<()> {
        let id = uuid_v7()?;
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "7");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        Ok(())
    }
}
