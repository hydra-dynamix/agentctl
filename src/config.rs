use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::platform;

pub const PROJECT_CONFIG_FILE: &str = "agentctl.toml";
pub const CONFIG_DIR: &str = ".agentctl";
pub const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub summary: SummaryConfig,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryConfig {
    pub max_output_bytes: usize,
    pub tail_bytes: usize,
    pub max_preview_lines: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarizer_command: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub command: Vec<String>,
    pub prompt_stdin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: Config,
}

impl Default for Config {
    fn default() -> Self {
        let mut agents = BTreeMap::new();
        agents.insert(
            "pi".to_owned(),
            AgentConfig {
                command: vec!["pi".to_owned(), "-p".to_owned()],
                prompt_stdin: false,
                cwd: None,
            },
        );
        agents.insert(
            "codex".to_owned(),
            AgentConfig {
                command: vec![
                    "codex".to_owned(),
                    "exec".to_owned(),
                    "--sandbox".to_owned(),
                    "workspace-write".to_owned(),
                ],
                prompt_stdin: true,
                cwd: None,
            },
        );
        agents.insert(
            "claude-code".to_owned(),
            AgentConfig {
                command: vec!["claude".to_owned(), "-p".to_owned()],
                prompt_stdin: false,
                cwd: None,
            },
        );
        agents.insert(
            "claude".to_owned(),
            AgentConfig {
                command: vec!["claude".to_owned(), "-p".to_owned()],
                prompt_stdin: false,
                cwd: None,
            },
        );
        agents.insert(
            "openai-rest".to_owned(),
            AgentConfig {
                command: vec!["openai-rest-agent".to_owned()],
                prompt_stdin: true,
                cwd: None,
            },
        );
        agents.insert(
            "openai-websocket".to_owned(),
            AgentConfig {
                command: vec!["openai-websocket-agent".to_owned()],
                prompt_stdin: true,
                cwd: None,
            },
        );
        agents.insert(
            "openclaw".to_owned(),
            AgentConfig {
                command: vec!["openclaw".to_owned(), "run".to_owned()],
                prompt_stdin: false,
                cwd: None,
            },
        );
        agents.insert(
            "opencode".to_owned(),
            AgentConfig {
                command: vec!["opencode".to_owned(), "run".to_owned()],
                prompt_stdin: false,
                cwd: None,
            },
        );
        agents.insert(
            "ollama".to_owned(),
            AgentConfig {
                command: vec!["ollama".to_owned(), "run".to_owned(), "llama3".to_owned()],
                prompt_stdin: true,
                cwd: None,
            },
        );

        Self {
            summary: SummaryConfig::default(),
            agents,
        }
    }
}

impl Default for SummaryConfig {
    fn default() -> Self {
        Self {
            max_output_bytes: 16_384,
            tail_bytes: 4_096,
            max_preview_lines: 12,
            summarizer_command: None,
        }
    }
}

pub fn load() -> Result<LoadedConfig> {
    let current_dir = std::env::current_dir().context("determining current directory")?;
    load_from_dir(&current_dir)
}

pub fn load_from_dir(current_dir: &Path) -> Result<LoadedConfig> {
    let home = platform::home_dir()?;
    let selected_path = preferred_config_path(current_dir, &home);
    if selected_path.exists() {
        return load_path(&selected_path);
    }

    Ok(LoadedConfig {
        path: selected_path,
        config: Config::default(),
    })
}

pub fn preferred_config_path(current_dir: &Path, home: &Path) -> PathBuf {
    let project_config = current_dir.join(PROJECT_CONFIG_FILE);
    if project_config.exists() {
        return project_config;
    }

    home.join(CONFIG_DIR).join(CONFIG_FILE)
}

pub fn generate() -> Result<PathBuf> {
    generate_at(&home_config_path()?)
}

pub fn generate_at(path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {}", parent.display()))?;
    }

    let text = to_toml_string(&Config::default())?;
    fs::write(path, text).with_context(|| format!("writing config {}", path.display()))?;
    Ok(path.to_path_buf())
}

pub fn check() -> Result<LoadedConfig> {
    let loaded = load()?;
    validate(&loaded.config).with_context(|| format!("checking {}", loaded.path.display()))?;
    Ok(loaded)
}

pub fn check_path(path: &Path) -> Result<Config> {
    let config = read_config(path)?;
    validate(&config).with_context(|| format!("checking {}", path.display()))?;
    Ok(config)
}

pub fn show() -> Result<String> {
    let loaded = load()?;
    to_toml_string(&loaded.config)
}

pub fn validate(config: &Config) -> Result<()> {
    if config.summary.max_output_bytes == 0 {
        bail!("summary.max_output_bytes must be greater than zero");
    }
    if config.summary.tail_bytes > config.summary.max_output_bytes {
        bail!("summary.tail_bytes must be less than or equal to summary.max_output_bytes");
    }
    if config.summary.max_preview_lines == 0 {
        bail!("summary.max_preview_lines must be greater than zero");
    }
    if let Some(command) = &config.summary.summarizer_command
        && command.is_empty()
    {
        bail!("summary.summarizer_command must not be empty when set");
    }

    for (name, agent) in &config.agents {
        if agent.command.is_empty() {
            bail!("agents.{name}.command must not be empty");
        }
        if agent.command.iter().any(|part| part.trim().is_empty()) {
            bail!("agents.{name}.command must not contain empty arguments");
        }
    }

    Ok(())
}

pub fn home_config_path() -> Result<PathBuf> {
    Ok(platform::home_dir()?.join(CONFIG_DIR).join(CONFIG_FILE))
}

fn load_path(path: &Path) -> Result<LoadedConfig> {
    let config = check_path(path)?;
    Ok(LoadedConfig {
        path: path.to_path_buf(),
        config,
    })
}

fn read_config(path: &Path) -> Result<Config> {
    let text =
        fs::read_to_string(path).with_context(|| format!("reading config {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
}

fn to_toml_string(config: &Config) -> Result<String> {
    toml::to_string_pretty(config).context("serializing default config")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_check_round_trips() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join(CONFIG_DIR).join(CONFIG_FILE);

        generate_at(&path).expect("generate config");
        let loaded = check_path(&path).expect("check generated config");

        assert_eq!(loaded, Config::default());
    }

    #[test]
    fn project_config_wins_over_home_config() {
        let project_dir = tempfile::tempdir().expect("project tempdir");
        let home_dir = tempfile::tempdir().expect("home tempdir");
        let project_path = project_dir.path().join(PROJECT_CONFIG_FILE);
        let home_path = home_dir.path().join(CONFIG_DIR).join(CONFIG_FILE);
        let mut project_config = Config::default();
        project_config.summary.max_preview_lines = 3;

        generate_at(&home_path).expect("generate home config");
        fs::write(
            &project_path,
            to_toml_string(&project_config).expect("toml"),
        )
        .expect("write project config");

        let loaded = load_from_dir(project_dir.path()).expect("load project config");

        assert_eq!(loaded.path, project_path);
        assert_eq!(loaded.config.summary.max_preview_lines, 3);
    }

    #[test]
    fn invalid_config_is_rejected() {
        let config = Config {
            summary: SummaryConfig {
                max_output_bytes: 100,
                tail_bytes: 101,
                max_preview_lines: 12,
                summarizer_command: None,
            },
            agents: BTreeMap::new(),
        };

        let error = validate(&config).expect_err("invalid config");

        assert!(
            error
                .to_string()
                .contains("tail_bytes must be less than or equal")
        );
    }
}
