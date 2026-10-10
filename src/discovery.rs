use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::compatibility::{self, CoreCompatibilityStatus};
use crate::config::{self, AgentConfig, Config, LoadedConfig};
use crate::platform::{self, HomeResolution, PlatformInfo};

pub const DISCOVERY_SCHEMA: &str = "agentctl.discovery.v1";
pub const LDGR_CONFIG_TOML: &str = ".ldgr/config.toml";
pub const LDGR_CONFIG_JSON: &str = ".ldgr/config.json";

#[derive(Debug, Clone, Default, Deserialize)]
struct LdgrHarnessDocument {
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    default_harness: Option<String>,
    #[serde(default)]
    selected_harnesses: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LdgrSelection {
    pub path: Option<PathBuf>,
    pub format: Option<String>,
    pub schema_version: Option<u32>,
    pub default_harness: Option<String>,
    pub selected_harnesses: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HarnessStatus {
    pub name: String,
    pub selected: bool,
    pub default: bool,
    pub command_candidates: Vec<String>,
    pub executable: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentStatus {
    pub name: String,
    pub command: Vec<String>,
    pub available: bool,
    pub executable: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveryReport {
    pub schema: String,
    pub platform: PlatformInfo,
    pub home: HomeResolution,
    pub agentctl_config: PathBuf,
    pub ldgr: LdgrSelection,
    pub core_compatibility: CoreCompatibilityStatus,
    pub harnesses: Vec<HarnessStatus>,
    pub agents: Vec<AgentStatus>,
    pub recommended_agent: Option<String>,
    pub warnings: Vec<String>,
    pub options: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    pub name: String,
    pub agent: AgentConfig,
    pub warnings: Vec<String>,
}

pub fn diagnose() -> Result<DiscoveryReport> {
    let platform = platform::detect();
    let home = platform::home_resolution()?;
    let current_dir = std::env::current_dir().context("determining current directory")?;
    let mut warnings = Vec::new();
    let loaded = match config::check() {
        Ok(loaded) => loaded,
        Err(error) => {
            warnings.push(format!(
                "Agentctl config could not be loaded: {error:#}. Built-in profiles are shown for recovery only."
            ));
            LoadedConfig {
                path: config::preferred_config_path(&current_dir, &home.path),
                config: Config::default(),
            }
        }
    };
    let ldgr = load_ldgr_selection(&home.path);
    let core_compatibility = compatibility::status(&current_dir);
    warnings.extend(ldgr.warnings.iter().cloned());
    let harnesses = harness_statuses(&ldgr);
    let agents = agent_statuses(&loaded.config);
    let recommended_agent = resolve_agent_with(&loaded, None, true, &ldgr, command_lookup)
        .ok()
        .map(|resolved| resolved.name);
    let options = recovery_options(&agents, &harnesses);

    Ok(DiscoveryReport {
        schema: DISCOVERY_SCHEMA.to_owned(),
        platform,
        home,
        agentctl_config: loaded.path,
        ldgr,
        core_compatibility,
        harnesses,
        agents,
        recommended_agent,
        warnings,
        options,
    })
}

pub fn resolve_agent(
    loaded: &LoadedConfig,
    requested: Option<&str>,
    allow_fallback: bool,
) -> Result<ResolvedAgent> {
    let home = platform::home_dir()?;
    let ldgr = load_ldgr_selection(&home);
    resolve_agent_with(loaded, requested, allow_fallback, &ldgr, command_lookup)
}

pub fn load_ldgr_selection(home: &Path) -> LdgrSelection {
    let toml_path = home.join(LDGR_CONFIG_TOML);
    let json_path = home.join(LDGR_CONFIG_JSON);
    let mut warnings = Vec::new();

    if toml_path.is_file() {
        match read_ldgr_toml(&toml_path) {
            Ok(document) => {
                return selection_from_document(toml_path, "toml", document, warnings);
            }
            Err(error) => warnings.push(format!(
                "Could not parse {}: {error:#}.",
                toml_path.display()
            )),
        }
    }

    if json_path.is_file() {
        match read_ldgr_json(&json_path) {
            Ok(document) => {
                if toml_path.is_file() {
                    warnings.push(format!(
                        "Using legacy {} because the preferred TOML config is invalid.",
                        json_path.display()
                    ));
                } else {
                    warnings.push(format!(
                        "Using legacy {}; rerun `ldgr install` to create {}.",
                        json_path.display(),
                        toml_path.display()
                    ));
                }
                return selection_from_document(json_path, "json", document, warnings);
            }
            Err(error) => warnings.push(format!(
                "Could not parse {}: {error:#}.",
                json_path.display()
            )),
        }
    }

    if !toml_path.is_file() && !json_path.is_file() {
        warnings.push(format!(
            "No LDGR harness selection found. Run `ldgr install` to create {}.",
            toml_path.display()
        ));
    }
    LdgrSelection {
        warnings,
        ..LdgrSelection::default()
    }
}

pub fn print_human(report: &DiscoveryReport) {
    println!("Platform");
    println!(
        "- {} {} ({}) runtime={} ci={} container={}",
        report.platform.os,
        report.platform.arch,
        report.platform.family,
        report.platform.runtime,
        report.platform.ci,
        report.platform.container
    );
    println!(
        "- home: {} (source={})",
        report.home.path.display(),
        report.home.source
    );
    println!("- agentctl config: {}", report.agentctl_config.display());
    match &report.ldgr.path {
        Some(path) => println!(
            "- LDGR selection: {} ({})",
            path.display(),
            report.ldgr.format.as_deref().unwrap_or("unknown")
        ),
        None => println!("- LDGR selection: <not available>"),
    }
    println!(
        "- Core compatibility: {} (Core={}, agentctl={}, required={})",
        if report.core_compatibility.compatible {
            "compatible"
        } else {
            "incompatible"
        },
        report
            .core_compatibility
            .core_version
            .as_deref()
            .unwrap_or("<unknown>"),
        report.core_compatibility.agentctl_version,
        report.core_compatibility.required_core,
    );

    println!("Harnesses");
    for harness in &report.harnesses {
        let marker = if harness.default {
            "default"
        } else if harness.selected {
            "selected"
        } else {
            "not selected"
        };
        let executable = harness
            .executable
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<not found>".to_owned());
        println!("- {}: {} ({marker})", harness.name, executable);
    }

    println!("Configured agents");
    for agent in &report.agents {
        let executable = agent
            .executable
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<not found>".to_owned());
        println!("- {}: {}", agent.name, executable);
    }
    println!(
        "Recommended agent: {}",
        report.recommended_agent.as_deref().unwrap_or("<none>")
    );

    if !report.warnings.is_empty() {
        println!("Warnings");
        for warning in &report.warnings {
            println!("- {warning}");
        }
    }
    println!("Recovery options");
    for option in &report.options {
        println!("- {option}");
    }
}

fn read_ldgr_toml(path: &Path) -> Result<LdgrHarnessDocument> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn read_ldgr_json(path: &Path) -> Result<LdgrHarnessDocument> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn selection_from_document(
    path: PathBuf,
    format: &str,
    document: LdgrHarnessDocument,
    mut warnings: Vec<String>,
) -> LdgrSelection {
    let mut selected = Vec::new();
    for harness in document
        .selected_harnesses
        .into_iter()
        .map(|value| normalize_harness(&value))
    {
        if !harness.is_empty() && !selected.contains(&harness) {
            selected.push(harness);
        }
    }
    let default_harness = document
        .default_harness
        .map(|value| normalize_harness(&value))
        .filter(|value| !value.is_empty());
    if let Some(default) = &default_harness
        && !selected.contains(default)
    {
        warnings.push(format!(
            "LDGR default harness `{default}` is not present in selected_harnesses."
        ));
        selected.insert(0, default.clone());
    }
    LdgrSelection {
        path: Some(path),
        format: Some(format.to_owned()),
        schema_version: document.schema_version,
        default_harness,
        selected_harnesses: selected,
        warnings,
    }
}

fn normalize_harness(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "claude-code" | "claude_code" => "claude".to_owned(),
        "open-claw" | "open_claw" | "opencode" => "openclaw".to_owned(),
        value => value.to_owned(),
    }
}

fn harness_specs() -> [(&'static str, &'static [&'static str]); 4] {
    [
        ("pi", &["pi"]),
        ("codex", &["codex"]),
        ("claude", &["claude"]),
        ("openclaw", &["openclaw", "opencode"]),
    ]
}

fn harness_statuses(selection: &LdgrSelection) -> Vec<HarnessStatus> {
    harness_specs()
        .into_iter()
        .map(|(name, commands)| HarnessStatus {
            name: name.to_owned(),
            selected: selection
                .selected_harnesses
                .iter()
                .any(|value| value == name),
            default: selection.default_harness.as_deref() == Some(name),
            command_candidates: commands.iter().map(|value| (*value).to_owned()).collect(),
            executable: commands
                .iter()
                .find_map(|command| platform::command_path(command)),
        })
        .collect()
}

fn agent_statuses(config: &Config) -> Vec<AgentStatus> {
    config
        .agents
        .iter()
        .map(|(name, agent)| {
            let executable = agent
                .command
                .first()
                .and_then(|command| platform::command_path(command));
            AgentStatus {
                name: name.clone(),
                command: agent.command.clone(),
                available: agent.provider.is_some() || executable.is_some(),
                executable,
            }
        })
        .collect()
}

fn resolve_agent_with<F>(
    loaded: &LoadedConfig,
    requested: Option<&str>,
    allow_fallback: bool,
    selection: &LdgrSelection,
    lookup: F,
) -> Result<ResolvedAgent>
where
    F: Fn(&str) -> Option<PathBuf>,
{
    let mut warnings = selection.warnings.clone();
    let requested = requested.map(str::trim).filter(|value| !value.is_empty());
    let preferred = requested.unwrap_or_else(|| {
        if selection.path.is_some() {
            "ldgr-loop"
        } else {
            "codex"
        }
    });

    if let Some(agent) = loaded.config.agents.get(preferred) {
        if agent_available(agent, &lookup) {
            return Ok(ResolvedAgent {
                name: preferred.to_owned(),
                agent: agent.clone(),
                warnings,
            });
        }
        warnings.push(unavailable_agent_warning(preferred, agent));
    } else if requested.is_some() && !is_ldgr_generic(preferred) {
        return Err(resolution_error(
            format!("unknown agent `{preferred}` in {}", loaded.path.display()),
            loaded,
            selection,
            &lookup,
        ));
    }

    let may_fallback = allow_fallback && (requested.is_none() || is_ldgr_generic(preferred));
    if may_fallback {
        for harness in ordered_harnesses(selection) {
            for profile in profile_candidates(preferred, &harness) {
                let Some(agent) = loaded.config.agents.get(&profile) else {
                    continue;
                };
                if agent_available(agent, &lookup) {
                    if profile != preferred {
                        warnings.push(format!(
                            "Using available selected harness `{harness}` through agent profile `{profile}`."
                        ));
                    }
                    return Ok(ResolvedAgent {
                        name: profile,
                        agent: agent.clone(),
                        warnings,
                    });
                }
            }
        }

        if requested.is_none() {
            for (name, agent) in &loaded.config.agents {
                if agent_available(agent, &lookup) {
                    warnings.push(format!(
                        "No selected LDGR harness was runnable; using available configured agent `{name}`."
                    ));
                    return Ok(ResolvedAgent {
                        name: name.clone(),
                        agent: agent.clone(),
                        warnings,
                    });
                }
            }
        }
    }

    Err(resolution_error(
        format!("agent `{preferred}` is not runnable"),
        loaded,
        selection,
        &lookup,
    ))
}

fn agent_available<F>(agent: &AgentConfig, lookup: &F) -> bool
where
    F: Fn(&str) -> Option<PathBuf>,
{
    agent.provider.is_some()
        || agent
            .command
            .first()
            .and_then(|command| lookup(command))
            .is_some()
}

fn unavailable_agent_warning(name: &str, agent: &AgentConfig) -> String {
    let executable = agent
        .command
        .first()
        .map(String::as_str)
        .unwrap_or("<empty command>");
    format!("Configured agent `{name}` requires `{executable}`, which was not found.")
}

fn is_ldgr_generic(name: &str) -> bool {
    matches!(name, "ldgr-loop" | "ldgr-summary")
}

fn ordered_harnesses(selection: &LdgrSelection) -> Vec<String> {
    let mut harnesses = Vec::new();
    if let Some(default) = &selection.default_harness {
        harnesses.push(default.clone());
    }
    for harness in &selection.selected_harnesses {
        if !harnesses.contains(harness) {
            harnesses.push(harness.clone());
        }
    }
    harnesses
}

fn profile_candidates(generic: &str, harness: &str) -> Vec<String> {
    let mut profiles = Vec::new();
    if is_ldgr_generic(generic) {
        profiles.push(format!("ldgr-loop-{harness}"));
    }
    match harness {
        "pi" => profiles.push("pi".to_owned()),
        "codex" => profiles.push("codex".to_owned()),
        "claude" => {
            profiles.push("claude-code".to_owned());
            profiles.push("claude".to_owned());
        }
        "openclaw" => {
            profiles.push("openclaw".to_owned());
            profiles.push("opencode".to_owned());
        }
        other => profiles.push(other.to_owned()),
    }
    profiles
}

fn resolution_error<F>(
    headline: String,
    loaded: &LoadedConfig,
    selection: &LdgrSelection,
    lookup: &F,
) -> anyhow::Error
where
    F: Fn(&str) -> Option<PathBuf>,
{
    let available = loaded
        .config
        .agents
        .iter()
        .filter(|(_, agent)| agent_available(agent, lookup))
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let selected = if selection.selected_harnesses.is_empty() {
        "<none>".to_owned()
    } else {
        selection.selected_harnesses.join(", ")
    };
    let available = if available.is_empty() {
        "<none>".to_owned()
    } else {
        available.join(", ")
    };
    anyhow!(
        "{headline}\n\
         detected platform: {} {} ({})\n\
         agentctl config: {}\n\
         LDGR selected harnesses: {selected}\n\
         runnable configured agents: {available}\n\
         recovery options:\n\
         - run `agentctl discover` for complete diagnostics\n\
         - run `ldgr install --harness <name>` to repair the LDGR selection\n\
         - fix PATH or the command in {}\n\
         - pass an available agent explicitly: `agentctl run <agent> ...`",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
        loaded.path.display(),
        loaded.path.display(),
    )
}

fn command_lookup(command: &str) -> Option<PathBuf> {
    platform::command_path(command)
}

fn recovery_options(agents: &[AgentStatus], harnesses: &[HarnessStatus]) -> Vec<String> {
    let mut options = BTreeSet::new();
    options.insert("Run `agentctl config check` to validate Agentctl configuration.".to_owned());
    options.insert(
        "Run `ldgr install --harness <name>` to update the preferred harnesses.".to_owned(),
    );
    options.insert(
        "Repair PATH or use an absolute executable path in the selected agent command.".to_owned(),
    );
    for agent in agents.iter().filter(|agent| agent.available) {
        options.insert(format!(
            "Use configured agent `{}` with `agentctl run {} ...`.",
            agent.name, agent.name
        ));
    }
    let available_harnesses = harnesses
        .iter()
        .filter(|harness| harness.executable.is_some())
        .map(|harness| harness.name.clone())
        .collect::<Vec<_>>();
    if !available_harnesses.is_empty() {
        options.insert(format!(
            "Available harness executables: {}.",
            available_harnesses.join(", ")
        ));
    }
    options.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn loaded(agents: &[(&str, &str)]) -> LoadedConfig {
        let agents = agents
            .iter()
            .map(|(name, command)| {
                (
                    (*name).to_owned(),
                    AgentConfig {
                        command: vec![(*command).to_owned()],
                        prompt_stdin: true,
                        cwd: None,
                        provider: None,
                        model: None,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        LoadedConfig {
            path: PathBuf::from("config.toml"),
            config: Config {
                agents,
                ..Config::default()
            },
        }
    }

    fn lookup(command: &str) -> Option<PathBuf> {
        (command == "codex").then(|| PathBuf::from("/bin/codex"))
    }

    #[test]
    fn invalid_toml_falls_back_to_legacy_json() {
        let home = tempfile::tempdir().expect("home");
        fs::create_dir_all(home.path().join(".ldgr")).expect("ldgr dir");
        fs::write(home.path().join(LDGR_CONFIG_TOML), "not = [valid").expect("toml");
        fs::write(
            home.path().join(LDGR_CONFIG_JSON),
            r#"{"schema_version":1,"default_harness":"codex","selected_harnesses":["codex"]}"#,
        )
        .expect("json");

        let selection = load_ldgr_selection(home.path());

        assert_eq!(selection.format.as_deref(), Some("json"));
        assert_eq!(selection.default_harness.as_deref(), Some("codex"));
        assert_eq!(selection.warnings.len(), 2);
    }

    #[test]
    fn unavailable_primary_ldgr_agent_uses_selected_available_fallback() {
        let loaded = loaded(&[
            ("ldgr-loop", "pi"),
            ("ldgr-loop-pi", "pi"),
            ("ldgr-loop-codex", "codex"),
        ]);
        let selection = LdgrSelection {
            default_harness: Some("pi".to_owned()),
            selected_harnesses: vec!["pi".to_owned(), "codex".to_owned()],
            ..LdgrSelection::default()
        };

        let resolved = resolve_agent_with(&loaded, Some("ldgr-loop"), true, &selection, lookup)
            .expect("fallback");

        assert_eq!(resolved.name, "ldgr-loop-codex");
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.contains("pi"))
        );
    }

    #[test]
    fn explicit_non_ldgr_agent_never_silently_falls_back() {
        let loaded = loaded(&[("claude", "claude"), ("codex", "codex")]);
        let selection = LdgrSelection {
            default_harness: Some("codex".to_owned()),
            selected_harnesses: vec!["codex".to_owned()],
            ..LdgrSelection::default()
        };

        let error = resolve_agent_with(&loaded, Some("claude"), true, &selection, lookup)
            .expect_err("explicit unavailable agent");

        assert!(error.to_string().contains("not runnable"));
        assert!(
            error
                .to_string()
                .contains("runnable configured agents: codex")
        );
    }
}
