use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::Deserialize;
use std::path::{Path, PathBuf};

const CONFIG_FILE: &str = "kamui.toml";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

const TEMPLATE: &str = "\
# Kamui configuration (global). This file may contain your API key.
#
# A project-level kamui.toml in a repository root may override `model`,
# `context_window`, and `provider.base_url`, but must NOT contain an api_key.

# Required: the model identifier your provider expects.
model = \"gpt-4o\"

# Optional: context window size. Enables context-percentage reporting.
# context_window = 128000

[provider]
# OpenAI-compatible base URL. Defaults to https://api.openai.com/v1 if omitted.
base_url = \"https://api.openai.com/v1\"

# Required: your provider API key.
api_key = \"\"
";

/// Fully resolved runtime configuration.
#[derive(Debug)]
pub struct Config {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub context_window: Option<u64>,
}

/// The result of loading configuration: either usable settings, or a signal that the user still
/// needs to fill in the freshly scaffolded (or key-less) global config.
pub enum Loaded {
    Ready(Config),
    NeedsSetup(PathBuf),
}

/// The on-disk shape of a `kamui.toml`. Every field is optional so global and project files can be
/// partial and layer over one another.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    model: Option<String>,
    context_window: Option<u64>,
    provider: Option<ProviderSection>,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderSection {
    base_url: Option<String>,
    api_key: Option<String>,
}

impl Config {
    /// Load configuration from the global `kamui.toml`, layering an optional project `kamui.toml`
    /// from the working directory on top. On first run the global file is scaffolded and the caller
    /// is asked to fill it in. No environment variables participate in provider or model settings.
    pub fn load() -> Result<Loaded> {
        let global_path = global_config_path()?;
        if !global_path.exists() {
            scaffold_global(&global_path)?;
            return Ok(Loaded::NeedsSetup(global_path));
        }
        let global = read_config_file(&global_path)?;

        // The template ships with an empty api_key, so an unfilled global file is still "needs
        // setup" rather than a hard error.
        let has_key = global
            .provider
            .as_ref()
            .and_then(|provider| provider.api_key.as_ref())
            .is_some_and(|key| !key.trim().is_empty());
        if !has_key {
            return Ok(Loaded::NeedsSetup(global_path));
        }

        let project_path = std::env::current_dir()
            .context("could not determine the working directory")?
            .join(CONFIG_FILE);
        let project = if project_path.is_file() {
            Some(read_config_file(&project_path)?)
        } else {
            None
        };

        resolve(global, project).map(Loaded::Ready)
    }
}

/// Merge a global file with an optional project file into a resolved `Config`. Kept separate from
/// disk access so the precedence and safety rules can be tested directly.
fn resolve(global: ConfigFile, project: Option<ConfigFile>) -> Result<Config> {
    if project
        .as_ref()
        .and_then(|file| file.provider.as_ref())
        .and_then(|provider| provider.api_key.as_ref())
        .is_some()
    {
        anyhow::bail!(
            "a project kamui.toml must not contain an api_key; keep secrets in the global config"
        );
    }

    let project_provider = project.as_ref().and_then(|file| file.provider.as_ref());
    let global_provider = global.provider.as_ref();

    let model = project
        .as_ref()
        .and_then(|file| file.model.clone())
        .or_else(|| global.model.clone())
        .context("model is not configured; set `model` in your kamui.toml")?;

    let context_window = project
        .as_ref()
        .and_then(|file| file.context_window)
        .or(global.context_window);

    let base_url = project_provider
        .and_then(|provider| provider.base_url.clone())
        .or_else(|| global_provider.and_then(|provider| provider.base_url.clone()))
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

    let api_key = global_provider
        .and_then(|provider| provider.api_key.clone())
        .filter(|key| !key.trim().is_empty())
        .context("api_key is not set; add it under [provider] in the global kamui.toml")?;

    Ok(Config {
        model,
        base_url,
        api_key,
        context_window,
    })
}

fn global_config_path() -> Result<PathBuf> {
    BaseDirs::new()
        .map(|dirs| dirs.config_dir().join("kamui").join(CONFIG_FILE))
        .context("could not determine the operating system config directory")
}

fn read_config_file(path: &Path) -> Result<ConfigFile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn scaffold_global(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, TEMPLATE).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(toml: &str) -> ConfigFile {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn parses_a_full_config_file() {
        let parsed = file(
            "model = \"gpt-5\"\ncontext_window = 128000\n[provider]\nbase_url = \"https://x/v1\"\napi_key = \"sk-1\"",
        );
        assert_eq!(parsed.model.as_deref(), Some("gpt-5"));
        assert_eq!(parsed.context_window, Some(128000));
        let provider = parsed.provider.unwrap();
        assert_eq!(provider.base_url.as_deref(), Some("https://x/v1"));
        assert_eq!(provider.api_key.as_deref(), Some("sk-1"));
    }

    #[test]
    fn resolves_from_the_global_file_with_defaults() {
        let config = resolve(
            file("model = \"gpt-5\"\n[provider]\napi_key = \"sk-1\""),
            None,
        )
        .unwrap();

        assert_eq!(config.model, "gpt-5");
        assert_eq!(config.api_key, "sk-1");
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.context_window, None);
    }

    #[test]
    fn project_overrides_non_secret_fields() {
        let global = file(
            "model = \"gpt-5\"\ncontext_window = 8000\n[provider]\nbase_url = \"https://global/v1\"\napi_key = \"sk-1\"",
        );
        let project = file("model = \"gpt-5-mini\"\n[provider]\nbase_url = \"https://project/v1\"");

        let config = resolve(global, Some(project)).unwrap();

        assert_eq!(config.model, "gpt-5-mini");
        assert_eq!(config.base_url, "https://project/v1");
        assert_eq!(config.context_window, Some(8000)); // inherited from global
        assert_eq!(config.api_key, "sk-1"); // always from global
    }

    #[test]
    fn rejects_an_api_key_in_a_project_file() {
        let global = file("model = \"gpt-5\"\n[provider]\napi_key = \"sk-1\"");
        let project = file("[provider]\napi_key = \"sk-leak\"");

        let error = resolve(global, Some(project)).unwrap_err();
        assert!(error.to_string().contains("must not contain an api_key"));
    }

    #[test]
    fn requires_a_model() {
        let error = resolve(file("[provider]\napi_key = \"sk-1\""), None).unwrap_err();
        assert!(error.to_string().contains("model is not configured"));
    }

    #[test]
    fn requires_a_non_empty_api_key() {
        let missing = resolve(file("model = \"gpt-5\""), None).unwrap_err();
        assert!(missing.to_string().contains("api_key is not set"));

        let blank = resolve(
            file("model = \"gpt-5\"\n[provider]\napi_key = \"   \""),
            None,
        )
        .unwrap_err();
        assert!(blank.to_string().contains("api_key is not set"));
    }
}
