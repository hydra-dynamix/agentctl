use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::platform;

pub const COMPATIBILITY_SCHEMA: &str = "ldgr.launcher-compatibility.v1";
pub const REQUIRED_CORE: &str = ">=0.1.13, <0.2.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreCompatibility {
    pub schema: String,
    pub compatible: bool,
    pub core_version: String,
    pub core_executable: PathBuf,
    pub agentctl_version: String,
    pub agentctl_requirement: String,
    pub error_recovery_schema: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreCompatibilityStatus {
    pub executable: Option<PathBuf>,
    pub core_version: Option<String>,
    pub agentctl_version: String,
    pub required_core: String,
    pub compatible: bool,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompatibilityFailure {
    pub detected_core_version: Option<String>,
    message: String,
}

impl fmt::Display for CompatibilityFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CompatibilityFailure {}

pub fn negotiate(project_root: &Path) -> Result<CoreCompatibility, CompatibilityFailure> {
    let executable = platform::command_path("ldgr");
    let Some(executable) = executable else {
        return Err(failure(None, None, "LDGR Core is not available on PATH"));
    };

    let output = Command::new(&executable)
        .current_dir(project_root)
        .args([
            "compatibility",
            "--agentctl-version",
            env!("CARGO_PKG_VERSION"),
            "--json",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            failure(
                Some(&executable),
                None,
                &format!("could not start LDGR Core compatibility negotiation: {error}"),
            )
        })?;

    if !output.status.success() {
        let detected = probe_version(&executable, project_root).ok();
        return Err(failure(
            Some(&executable),
            detected.as_deref(),
            "LDGR Core does not implement the required launcher compatibility protocol",
        ));
    }

    let report: CoreCompatibility = serde_json::from_slice(&output.stdout).map_err(|error| {
        let detected = probe_version(&executable, project_root).ok();
        failure(
            Some(&executable),
            detected.as_deref(),
            &format!("LDGR Core returned an invalid compatibility report: {error}"),
        )
    })?;
    let expected = VersionReq::parse(REQUIRED_CORE).expect("valid Core requirement");
    let core_version = Version::parse(&report.core_version).map_err(|error| {
        failure(
            Some(&executable),
            Some(&report.core_version),
            &format!("LDGR Core returned a non-semantic version: {error}"),
        )
    })?;
    if report.schema != COMPATIBILITY_SCHEMA
        || !report.compatible
        || !expected.matches(&core_version)
        || report.error_recovery_schema != 1
    {
        return Err(failure(
            Some(&executable),
            Some(&report.core_version),
            "LDGR Core rejected this agentctl release or reported an unsupported recovery contract",
        ));
    }
    Ok(report)
}

pub fn status(project_root: &Path) -> CoreCompatibilityStatus {
    match negotiate(project_root) {
        Ok(report) => CoreCompatibilityStatus {
            executable: Some(report.core_executable),
            core_version: Some(report.core_version),
            agentctl_version: env!("CARGO_PKG_VERSION").to_owned(),
            required_core: REQUIRED_CORE.to_owned(),
            compatible: true,
            diagnostic: None,
        },
        Err(error) => CoreCompatibilityStatus {
            executable: platform::command_path("ldgr"),
            core_version: error.detected_core_version.clone(),
            agentctl_version: env!("CARGO_PKG_VERSION").to_owned(),
            required_core: REQUIRED_CORE.to_owned(),
            compatible: false,
            diagnostic: Some(error.to_string()),
        },
    }
}

fn probe_version(executable: &Path, project_root: &Path) -> Result<String> {
    let output = Command::new(executable)
        .current_dir(project_root)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("starting {}", executable.display()))?;
    if !output.status.success() {
        bail!("{} --version failed", executable.display());
    }
    let text = String::from_utf8(output.stdout).context("LDGR version output was not UTF-8")?;
    text.split_whitespace()
        .nth(1)
        .map(str::to_owned)
        .context("LDGR version output did not contain a version")
}

fn failure(
    executable: Option<&Path>,
    detected_core_version: Option<&str>,
    reason: &str,
) -> CompatibilityFailure {
    let found = match (executable, detected_core_version) {
        (Some(path), Some(version)) => format!("found ldgr {version} at {}", path.display()),
        (Some(path), None) => format!("found an incompatible ldgr at {}", path.display()),
        (None, _) => "no ldgr executable was found on PATH".to_owned(),
    };
    CompatibilityFailure {
        detected_core_version: detected_core_version.map(str::to_owned),
        message: format!(
            "{reason}; agentctl {} requires LDGR Core {REQUIRED_CORE} with compatibility schema {COMPATIBILITY_SCHEMA}, but {found}.\n\
             Upgrade both binaries from the same LDGR Core release bundle, then verify:\n\
             - `agentctl --version`\n\
             - `ldgr --version`\n\
             - `agentctl discover --json`\n\
             On Windows, rerun `irm https://ldgr.run/install.ps1 | iex`; the installer updates the currently resolved user binary directory. \
             To roll back, install a release bundle containing both binaries rather than mixing versions.",
            env!("CARGO_PKG_VERSION")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_core_range_is_valid_and_excludes_old_core() {
        let requirement = VersionReq::parse(REQUIRED_CORE).expect("requirement");
        assert!(!requirement.matches(&Version::parse("0.1.12").expect("old")));
        assert!(requirement.matches(&Version::parse("0.1.13").expect("current")));
        assert!(!requirement.matches(&Version::parse("0.2.0").expect("future major")));
    }
}
