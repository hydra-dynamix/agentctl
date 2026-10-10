use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

pub const PROJECT_CONFIG_FILE: &str = "agentctl.toml";
pub const CONFIG_DIR: &str = ".agentctl";
pub const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub summary: SummaryConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default)]
    pub prompt_stdin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_command: Option<Vec<String>>,
    pub model: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    Openrouter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: Config,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "openrouter".to_owned(),
            ProviderConfig {
                kind: ProviderKind::Openrouter,
                base_url: "https://openrouter.ai/api/v1".to_owned(),
                api_key_env: Some("OPENROUTER_API_KEY".to_owned()),
                api_key_command: None,
                model: "openrouter/auto".to_owned(),
            },
        );

        let command_agent = |command: Vec<String>, prompt_stdin| AgentConfig {
            command,
            prompt_stdin,
            cwd: None,
            provider: None,
            model: None,
        };
        let mut agents = BTreeMap::new();
        agents.insert(
            "codex".to_owned(),
            command_agent(
                vec![
                    "codex".to_owned(),
                    "exec".to_owned(),
                    "--sandbox".to_owned(),
                    "danger-full-access".to_owned(),
                ],
                true,
            ),
        );
        agents.insert(
            "claude-code".to_owned(),
            command_agent(vec!["claude".to_owned(), "-p".to_owned()], false),
        );
        agents.insert(
            "claude".to_owned(),
            command_agent(vec!["claude".to_owned(), "-p".to_owned()], false),
        );
        agents.insert(
            "openai-rest".to_owned(),
            command_agent(vec!["openai-rest-agent".to_owned()], true),
        );
        agents.insert(
            "openai-websocket".to_owned(),
            command_agent(vec!["openai-websocket-agent".to_owned()], true),
        );
        agents.insert(
            "ollama".to_owned(),
            command_agent(
                vec!["ollama".to_owned(), "run".to_owned(), "llama3".to_owned()],
                true,
            ),
        );
        agents.insert(
            "openrouter-api".to_owned(),
            AgentConfig {
                command: Vec::new(),
                prompt_stdin: true,
                cwd: None,
                provider: Some("openrouter".to_owned()),
                model: None,
            },
        );
        agents.insert(
            "openrouter".to_owned(),
            command_agent(
                vec![
                    "pi".to_owned(),
                    "--provider".to_owned(),
                    "openrouter".to_owned(),
                    "--model".to_owned(),
                    "openrouter/auto".to_owned(),
                    "--print".to_owned(),
                    "--no-session".to_owned(),
                ],
                false,
            ),
        );

        Self {
            summary: SummaryConfig::default(),
            providers,
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
    let project_config = current_dir.join(PROJECT_CONFIG_FILE);
    if project_config.exists() {
        return load_path(&project_config);
    }

    let home_config = home_config_path()?;
    if home_config.exists() {
        return load_path(&home_config);
    }

    Ok(LoadedConfig {
        path: home_config,
        config: Config::default(),
    })
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

    for (name, provider) in &config.providers {
        if provider.base_url.trim().is_empty() {
            bail!("providers.{name}.base_url must not be empty");
        }
        if provider.model.trim().is_empty() {
            bail!("providers.{name}.model must not be empty");
        }
        match (&provider.api_key_env, &provider.api_key_command) {
            (Some(environment), None) if environment.trim().is_empty() => {
                bail!("providers.{name}.api_key_env must not be empty")
            }
            (None, Some(command)) if command.is_empty() => {
                bail!("providers.{name}.api_key_command must not be empty")
            }
            (None, Some(command)) if command.iter().any(|part| part.trim().is_empty()) => {
                bail!("providers.{name}.api_key_command must not contain empty arguments")
            }
            (Some(_), Some(_)) => {
                bail!("providers.{name} must set only one of api_key_env or api_key_command")
            }
            (None, None) => {
                bail!("providers.{name} must set one of api_key_env or api_key_command")
            }
            _ => {}
        }
    }

    for (name, agent) in &config.agents {
        match (agent.command.is_empty(), agent.provider.as_deref()) {
            (true, None) => bail!("agents.{name} must set command or provider"),
            (false, Some(_)) => bail!("agents.{name} must not set both command and provider"),
            _ => {}
        }
        if agent.command.iter().any(|part| part.trim().is_empty()) {
            bail!("agents.{name}.command must not contain empty arguments");
        }
        if let Some(provider) = &agent.provider
            && !config.providers.contains_key(provider)
        {
            bail!("agents.{name}.provider references unknown provider `{provider}`");
        }
        if agent.provider.is_none() && agent.model.is_some() {
            bail!("agents.{name}.model requires agents.{name}.provider");
        }
    }

    Ok(())
}

pub fn home_config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(CONFIG_DIR).join(CONFIG_FILE))
}

pub(crate) fn home_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    #[cfg(windows)]
    let home = home.or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()));
    home.map(PathBuf::from)
        .ok_or_else(|| anyhow!("home directory is not set"))
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
            providers: Config::default().providers,
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
