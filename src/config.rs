use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const CONFIG_FILE: &str = "kamui.toml";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_PROFILE_NAME: &str = "default";

const TEMPLATE: &str = "\
# Kamui configuration (global). This file may contain your API key.
#
# A project-level kamui.toml in a repository root may override `model`,
# `context_window`, `provider.base_url`, and `default_profile`, but must
# NOT contain an api_key.

# Required: the model identifier your provider expects.
model = \"gpt-4o\"

# Optional: context window size. Enables context-percentage reporting.
# context_window = 128000

[provider]
# OpenAI-compatible base URL. Defaults to https://api.openai.com/v1 if omitted.
base_url = \"https://api.openai.com/v1\"

# Required: your provider API key.
api_key = \"\"

# Alternatively, define several named profiles and switch between them at runtime
# with `/model <name>`. When profiles are present the flat settings above are ignored.
#
# default_profile = \"openai\"
#
# [profiles.openai]
# model = \"gpt-4o\"
# base_url = \"https://api.openai.com/v1\"
# api_key = \"sk-...\"
#
# [profiles.ollama]
# model = \"llama3.2\"
# base_url = \"http://localhost:11434/v1\"
# api_key = \"ollama\"
";

/// One provider+model configuration the user can run under.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub context_window: Option<u64>,
}

/// Fully resolved runtime configuration: every available profile plus the default choice.
#[derive(Debug)]
pub struct Config {
    pub profiles: Vec<Profile>,
    pub default_profile: String,
}

impl Config {
    pub fn find(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.name == name)
    }

    pub fn default(&self) -> &Profile {
        self.find(&self.default_profile)
            .expect("default profile always exists by construction")
    }
}

/// The result of loading configuration: either usable settings, or a signal that the user still
/// needs to fill in the freshly scaffolded (or key-less) global config.
pub enum Loaded {
    Ready(Config),
    NeedsSetup(PathBuf),
}

/// The on-disk shape of a `kamui.toml`. Every field is optional so global and project files can be
/// partial and layer over one another. The flat `model`/`provider` form and the `[profiles.*]` form
/// are both accepted; profiles win when present.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    model: Option<String>,
    context_window: Option<u64>,
    provider: Option<ProviderSection>,
    default_profile: Option<String>,
    #[serde(default)]
    profiles: HashMap<String, ProfileSection>,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderSection {
    base_url: Option<String>,
    api_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileSection {
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    context_window: Option<u64>,
}

impl Config {
    /// Load configuration from the global `kamui.toml`, layering an optional project `kamui.toml`
    /// from the working directory on top. On first run — or while the global file still lacks a key —
    /// the caller is asked to finish setup. No environment variables feed provider or model settings.
    pub fn load() -> Result<Loaded> {
        let global_path = global_config_path()?;
        if !global_path.exists() {
            scaffold_global(&global_path)?;
            return Ok(Loaded::NeedsSetup(global_path));
        }
        let global = read_config_file(&global_path)?;
        if !has_any_key(&global) {
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

/// Whether a global file carries at least one usable API key (flat or in any profile).
fn has_any_key(file: &ConfigFile) -> bool {
    let non_empty =
        |key: &Option<String>| key.as_ref().is_some_and(|value| !value.trim().is_empty());
    if !file.profiles.is_empty() {
        file.profiles
            .values()
            .any(|profile| non_empty(&profile.api_key))
    } else {
        file.provider
            .as_ref()
            .is_some_and(|provider| non_empty(&provider.api_key))
    }
}

/// Merge a global file with an optional project file into a resolved `Config`. Kept separate from
/// disk access so the precedence and safety rules can be tested directly.
fn resolve(global: ConfigFile, project: Option<ConfigFile>) -> Result<Config> {
    if let Some(project) = &project
        && declares_key(project)
    {
        anyhow::bail!(
            "a project kamui.toml must not contain an api_key; keep secrets in the global config"
        );
    }

    if global.profiles.is_empty() {
        resolve_flat(global, project)
    } else {
        resolve_profiles(global, project)
    }
}

/// A project file may not set an api_key anywhere, flat or per-profile.
fn declares_key(file: &ConfigFile) -> bool {
    file.provider
        .as_ref()
        .is_some_and(|provider| provider.api_key.is_some())
        || file
            .profiles
            .values()
            .any(|profile| profile.api_key.is_some())
}

/// The single-profile form: top-level `model`/`provider`, with project overrides for non-secrets.
fn resolve_flat(global: ConfigFile, project: Option<ConfigFile>) -> Result<Config> {
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

    let profile = Profile {
        name: DEFAULT_PROFILE_NAME.to_string(),
        model,
        base_url,
        api_key,
        context_window,
    };
    Ok(Config {
        default_profile: profile.name.clone(),
        profiles: vec![profile],
    })
}

/// The multi-profile form: one `[profiles.<name>]` per provider, chosen with `default_profile`.
fn resolve_profiles(global: ConfigFile, project: Option<ConfigFile>) -> Result<Config> {
    let mut profiles = Vec::with_capacity(global.profiles.len());
    for (name, section) in global.profiles {
        let model = section
            .model
            .with_context(|| format!("profile '{name}' is missing a model"))?;
        let base_url = section
            .base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let api_key = section
            .api_key
            .filter(|key| !key.trim().is_empty())
            .with_context(|| format!("profile '{name}' is missing an api_key"))?;
        profiles.push(Profile {
            name,
            model,
            base_url,
            api_key,
            context_window: section.context_window,
        });
    }
    // Stable ordering for listing, since the source is a hash map.
    profiles.sort_by(|a, b| a.name.cmp(&b.name));

    let default_profile = project
        .as_ref()
        .and_then(|file| file.default_profile.clone())
        .or(global.default_profile)
        .or_else(|| (profiles.len() == 1).then(|| profiles[0].name.clone()))
        .context("multiple profiles are defined; set `default_profile` to choose one")?;
    if !profiles
        .iter()
        .any(|profile| profile.name == default_profile)
    {
        anyhow::bail!("default_profile '{default_profile}' does not match any [profiles.*] entry");
    }

    Ok(Config {
        profiles,
        default_profile,
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
    fn resolves_a_flat_file_with_defaults() {
        let config = resolve(
            file("model = \"gpt-5\"\n[provider]\napi_key = \"sk-1\""),
            None,
        )
        .unwrap();

        assert_eq!(config.profiles.len(), 1);
        let profile = config.default();
        assert_eq!(profile.name, "default");
        assert_eq!(profile.model, "gpt-5");
        assert_eq!(profile.api_key, "sk-1");
        assert_eq!(profile.base_url, DEFAULT_BASE_URL);
        assert_eq!(profile.context_window, None);
    }

    #[test]
    fn project_overrides_non_secret_flat_fields() {
        let global = file(
            "model = \"gpt-5\"\ncontext_window = 8000\n[provider]\nbase_url = \"https://global/v1\"\napi_key = \"sk-1\"",
        );
        let project = file("model = \"gpt-5-mini\"\n[provider]\nbase_url = \"https://project/v1\"");

        let profile = resolve(global, Some(project)).unwrap().default().clone();

        assert_eq!(profile.model, "gpt-5-mini");
        assert_eq!(profile.base_url, "https://project/v1");
        assert_eq!(profile.context_window, Some(8000));
        assert_eq!(profile.api_key, "sk-1"); // always from global
    }

    #[test]
    fn resolves_named_profiles_and_the_default() {
        let global = file(
            "default_profile = \"ollama\"\n\
             [profiles.openai]\nmodel = \"gpt-5\"\nbase_url = \"https://api/v1\"\napi_key = \"sk-1\"\n\
             [profiles.ollama]\nmodel = \"llama3.2\"\nbase_url = \"http://localhost:11434/v1\"\napi_key = \"ollama\"\ncontext_window = 8000",
        );

        let config = resolve(global, None).unwrap();

        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.default_profile, "ollama");
        let active = config.default();
        assert_eq!(active.name, "ollama");
        assert_eq!(active.model, "llama3.2");
        assert_eq!(active.context_window, Some(8000));
        // Profiles are sorted by name for stable listing.
        assert_eq!(config.profiles[0].name, "ollama");
        assert_eq!(config.profiles[1].name, "openai");
    }

    #[test]
    fn a_single_profile_needs_no_default() {
        let config = resolve(
            file("[profiles.only]\nmodel = \"m\"\napi_key = \"k\""),
            None,
        )
        .unwrap();
        assert_eq!(config.default_profile, "only");
    }

    #[test]
    fn multiple_profiles_require_a_default() {
        let error = resolve(
            file("[profiles.a]\nmodel = \"m\"\napi_key = \"k\"\n[profiles.b]\nmodel = \"m\"\napi_key = \"k\""),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("set `default_profile`"));
    }

    #[test]
    fn a_project_may_pick_the_default_profile() {
        let global = file(
            "default_profile = \"a\"\n[profiles.a]\nmodel = \"m\"\napi_key = \"k\"\n[profiles.b]\nmodel = \"m\"\napi_key = \"k\"",
        );
        let project = file("default_profile = \"b\"");
        let config = resolve(global, Some(project)).unwrap();
        assert_eq!(config.default_profile, "b");
    }

    #[test]
    fn rejects_an_api_key_in_a_project_file() {
        let global = file("model = \"gpt-5\"\n[provider]\napi_key = \"sk-1\"");
        let project = file("[provider]\napi_key = \"sk-leak\"");

        let error = resolve(global, Some(project)).unwrap_err();
        assert!(error.to_string().contains("must not contain an api_key"));
    }

    #[test]
    fn rejects_a_profile_api_key_in_a_project_file() {
        let global = file("[profiles.a]\nmodel = \"m\"\napi_key = \"k\"");
        let project = file("[profiles.a]\napi_key = \"sk-leak\"");

        let error = resolve(global, Some(project)).unwrap_err();
        assert!(error.to_string().contains("must not contain an api_key"));
    }

    #[test]
    fn requires_a_model_and_key_in_flat_mode() {
        assert!(
            resolve(file("[provider]\napi_key = \"sk-1\""), None)
                .unwrap_err()
                .to_string()
                .contains("model is not configured")
        );
        assert!(
            resolve(file("model = \"gpt-5\""), None)
                .unwrap_err()
                .to_string()
                .contains("api_key is not set")
        );
    }
}
