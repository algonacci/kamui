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

# Optional: set to false if the model rejects the `tools` field (many small local
# models). Kamui then chats without offering tools. Defaults to true.
# tools = true

# Alternatively, define named profiles and switch between them at runtime with
# `/model <name>`. When profiles are present the flat settings above are ignored.
# Share one API key across many models by defining a [providers.*] block and
# referencing it from each profile with `provider = \"<name>\"`.
#
# default_profile = \"gpt4o\"
#
# [providers.openai]
# base_url = \"https://api.openai.com/v1\"
# api_key = \"sk-...\"
#
# [providers.ollama]
# base_url = \"http://localhost:11434/v1\"
# api_key = \"ollama\"
#
# [profiles.gpt4o]
# provider = \"openai\"
# model = \"gpt-4o\"
#
# [profiles.codeqwen]
# provider = \"ollama\"
# model = \"codeqwen:latest\"
# tools = false          # many small local models do not support tools

# MCP servers are launched as child processes and their tools are offered to the
# model alongside the built-in ones. Global-only: a project kamui.toml may not
# define these. Each call asks for approval unless the server is marked trusted.
#
# [mcp.filesystem]
# command = \"npx\"
# args = [\"-y\", \"@modelcontextprotocol/server-filesystem\", \".\"]
#
# [mcp.excel]
# command = \"uvx\"
# args = [\"mcp-excel\"]
# trusted = true         # skip the per-call approval for this server
";

/// One provider+model configuration the user can run under.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub context_window: Option<u64>,
    /// Whether to offer tools to this model. Disable for endpoints/models that reject the `tools`
    /// field (many small local models), so plain chat still works.
    pub tools: bool,
}

/// An MCP server Kamui launches and talks to over stdio.
#[derive(Debug, Clone)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// When true, this server's tools run without per-call approval.
    pub trusted: bool,
}

/// Fully resolved runtime configuration: every available profile plus the default choice.
#[derive(Debug)]
pub struct Config {
    pub profiles: Vec<Profile>,
    pub default_profile: String,
    pub mcp_servers: Vec<McpServer>,
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
    /// Named, shared provider credentials that profiles can reference by name.
    #[serde(default)]
    providers: HashMap<String, ProviderSection>,
    #[serde(default)]
    profiles: HashMap<String, ProfileSection>,
    /// MCP servers to launch. Global-only: a project file must not spawn processes.
    #[serde(default)]
    mcp: HashMap<String, McpSection>,
}

#[derive(Debug, Default, Deserialize)]
struct McpSection {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    trusted: bool,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderSection {
    base_url: Option<String>,
    api_key: Option<String>,
    tools: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileSection {
    /// Name of a `[providers.*]` block to inherit base_url/api_key/tools from.
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    context_window: Option<u64>,
    tools: Option<bool>,
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
        let project_path = std::env::current_dir()
            .context("could not determine the working directory")?
            .join(CONFIG_FILE);
        let project = if project_path.is_file() {
            Some(read_config_file(&project_path)?)
        } else {
            None
        };
        if !has_usable_configuration(&global, project.as_ref()) {
            return Ok(Loaded::NeedsSetup(global_path));
        }

        resolve(global, project).map(Loaded::Ready)
    }
}

/// Whether a global file carries at least one usable API key, in the flat provider, a shared
/// `[providers.*]` block, or inline on a profile.
fn has_usable_configuration(file: &ConfigFile, project: Option<&ConfigFile>) -> bool {
    let non_empty =
        |key: &Option<String>| key.as_ref().is_some_and(|value| !value.trim().is_empty());
    if file.profiles.is_empty() {
        (non_empty(&file.model) || project.is_some_and(|project| non_empty(&project.model)))
            && file
                .provider
                .as_ref()
                .is_some_and(|provider| non_empty(&provider.api_key))
    } else {
        file.providers.values().any(|p| non_empty(&p.api_key))
            || file.profiles.values().any(|p| non_empty(&p.api_key))
    }
}

/// Save the simple provider selected by first-run onboarding while preserving unrelated global
/// settings such as context limits and MCP servers.
pub fn save_onboarding(path: &Path, base_url: &str, api_key: &str, model: &str) -> Result<()> {
    ensure_onboarding_supported(path)?;
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut document: toml::Value =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    let table = document
        .as_table_mut()
        .context("global kamui.toml must contain a TOML table")?;
    table.insert("model".to_owned(), toml::Value::String(model.to_owned()));
    let provider = table
        .entry("provider")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("[provider] must be a TOML table")?;
    provider.insert(
        "base_url".to_owned(),
        toml::Value::String(base_url.trim_end_matches('/').to_owned()),
    );
    provider.insert(
        "api_key".to_owned(),
        toml::Value::String(api_key.to_owned()),
    );

    let content = toml::to_string_pretty(&document).context("failed to serialize configuration")?;
    std::fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    restrict_permissions(path)
}

pub fn ensure_onboarding_supported(path: &Path) -> Result<()> {
    let file = read_config_file(path)?;
    if !file.profiles.is_empty() || !file.providers.is_empty() {
        anyhow::bail!(
            "interactive setup cannot replace an advanced profiles configuration; edit {} manually",
            path.display()
        );
    }
    Ok(())
}

/// Merge a global file with an optional project file into a resolved `Config`. Kept separate from
/// disk access so the precedence and safety rules can be tested directly.
fn resolve(mut global: ConfigFile, project: Option<ConfigFile>) -> Result<Config> {
    if let Some(project) = &project {
        if declares_key(project) {
            anyhow::bail!(
                "a project kamui.toml must not contain an api_key; keep secrets in the global config"
            );
        }
        // Launching a server is arbitrary code execution, so a checked-in project file may not do it.
        if !project.mcp.is_empty() {
            anyhow::bail!(
                "a project kamui.toml must not define [mcp.*] servers; declare them in the global config"
            );
        }
    }

    let mcp_servers = resolve_mcp_servers(std::mem::take(&mut global.mcp))?;
    let mut config = if global.profiles.is_empty() {
        resolve_flat(global, project)?
    } else {
        resolve_profiles(global, project)?
    };
    config.mcp_servers = mcp_servers;
    Ok(config)
}

/// Turn `[mcp.<name>]` blocks into launchable server definitions, ordered by name.
fn resolve_mcp_servers(sections: HashMap<String, McpSection>) -> Result<Vec<McpServer>> {
    let mut servers = Vec::with_capacity(sections.len());
    for (name, section) in sections {
        let command = section
            .command
            .with_context(|| format!("mcp server '{name}' is missing a command"))?;
        servers.push(McpServer {
            name,
            command,
            args: section.args,
            trusted: section.trusted,
        });
    }
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(servers)
}

/// A project file may not set an api_key anywhere: flat, in a shared provider, or per-profile.
fn declares_key(file: &ConfigFile) -> bool {
    file.provider
        .as_ref()
        .is_some_and(|provider| provider.api_key.is_some())
        || file.providers.values().any(|p| p.api_key.is_some())
        || file.profiles.values().any(|p| p.api_key.is_some())
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

    let tools = project_provider
        .and_then(|provider| provider.tools)
        .or_else(|| global_provider.and_then(|provider| provider.tools))
        .unwrap_or(true);

    let profile = Profile {
        name: DEFAULT_PROFILE_NAME.to_string(),
        model,
        base_url,
        api_key,
        context_window,
        tools,
    };
    Ok(Config {
        default_profile: profile.name.clone(),
        profiles: vec![profile],
        mcp_servers: Vec::new(),
    })
}

/// The multi-profile form: one `[profiles.<name>]` per model, chosen with `default_profile`. A
/// profile may inherit base_url/api_key/tools from a shared `[providers.<name>]` block it references.
fn resolve_profiles(global: ConfigFile, project: Option<ConfigFile>) -> Result<Config> {
    let mut profiles = Vec::with_capacity(global.profiles.len());
    for (name, section) in &global.profiles {
        let shared = match &section.provider {
            Some(reference) => Some(global.providers.get(reference).with_context(|| {
                format!("profile '{name}' references unknown provider '{reference}'")
            })?),
            None => None,
        };

        let model = section
            .model
            .clone()
            .with_context(|| format!("profile '{name}' is missing a model"))?;
        let base_url = section
            .base_url
            .clone()
            .or_else(|| shared.and_then(|provider| provider.base_url.clone()))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let api_key = section
            .api_key
            .clone()
            .or_else(|| shared.and_then(|provider| provider.api_key.clone()))
            .filter(|key| !key.trim().is_empty())
            .with_context(|| {
                format!("profile '{name}' has no api_key (set it on the profile or its provider)")
            })?;
        let tools = section
            .tools
            .or_else(|| shared.and_then(|provider| provider.tools))
            .unwrap_or(true);
        profiles.push(Profile {
            name: name.clone(),
            model,
            base_url,
            api_key,
            context_window: section.context_window,
            tools,
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
        mcp_servers: Vec::new(),
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
    std::fs::write(path, TEMPLATE)
        .with_context(|| format!("failed to write {}", path.display()))?;
    restrict_permissions(path)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn file(toml: &str) -> ConfigFile {
        toml::from_str(toml).unwrap()
    }

    fn temporary_config(content: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("kamui-config-{}.toml", Uuid::new_v4()));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn setup_detection_requires_both_model_and_key() {
        assert!(!has_usable_configuration(&file("model = \"gpt-5\""), None));
        assert!(!has_usable_configuration(
            &file("[provider]\napi_key = \"sk-1\""),
            None
        ));
        assert!(has_usable_configuration(
            &file("model = \"gpt-5\"\n[provider]\napi_key = \"sk-1\""),
            None
        ));
    }

    #[test]
    fn project_model_completes_a_global_key_only_config() {
        let global = file("[provider]\napi_key = \"sk-1\"");
        let project = file("model = \"gpt-5\"");

        assert!(has_usable_configuration(&global, Some(&project)));
    }

    #[test]
    fn onboarding_preserves_unrelated_global_settings() {
        let path = temporary_config(
            "context_window = 8000\n[provider]\ntools = false\n[mcp.files]\ncommand = \"server\"",
        );

        save_onboarding(&path, "https://api.example.com/v1/", "sk-1", "gpt-5").unwrap();
        let saved = read_config_file(&path).unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(saved.model.as_deref(), Some("gpt-5"));
        assert_eq!(saved.context_window, Some(8000));
        assert_eq!(saved.provider.unwrap().tools, Some(false));
        assert_eq!(
            saved.mcp.get("files").unwrap().command.as_deref(),
            Some("server")
        );
    }

    #[test]
    fn onboarding_does_not_replace_advanced_profiles() {
        let path = temporary_config("[profiles.main]\nmodel = \"gpt-5\"");

        let error =
            save_onboarding(&path, "https://api.example.com/v1", "sk-1", "gpt-5").unwrap_err();
        std::fs::remove_file(path).unwrap();

        assert!(error.to_string().contains("advanced profiles"));
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
    fn profiles_inherit_shared_provider_credentials() {
        let global = file(
            "default_profile = \"sol\"\n\
             [providers.jatevo]\nbase_url = \"https://api.jatevo.ai/v1\"\napi_key = \"sk-j\"\n\
             [providers.ollama]\nbase_url = \"http://localhost:11434/v1\"\napi_key = \"ollama\"\ntools = false\n\
             [profiles.sol]\nprovider = \"jatevo\"\nmodel = \"gpt-5.6-sol\"\n\
             [profiles.codeqwen]\nprovider = \"ollama\"\nmodel = \"codeqwen:latest\"",
        );
        let config = resolve(global, None).unwrap();

        let sol = config.find("sol").unwrap();
        assert_eq!(sol.base_url, "https://api.jatevo.ai/v1");
        assert_eq!(sol.api_key, "sk-j");
        assert!(sol.tools);

        let codeqwen = config.find("codeqwen").unwrap();
        assert_eq!(codeqwen.api_key, "ollama");
        assert!(!codeqwen.tools); // inherited from the ollama provider
    }

    #[test]
    fn a_profile_referencing_an_unknown_provider_errors() {
        let error = resolve(
            file("[profiles.x]\nprovider = \"ghost\"\nmodel = \"m\""),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown provider 'ghost'"));
    }

    #[test]
    fn tools_default_on_and_can_be_disabled_per_profile() {
        let on = resolve(file("model = \"m\"\n[provider]\napi_key = \"k\""), None).unwrap();
        assert!(on.default().tools);

        let off = resolve(
            file("[profiles.local]\nmodel = \"m\"\napi_key = \"k\"\ntools = false"),
            None,
        )
        .unwrap();
        assert!(!off.default().tools);
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
    fn resolves_mcp_servers_from_the_global_file() {
        let config = resolve(
            file(
                "model = \"m\"\n[provider]\napi_key = \"k\"\n\
                 [mcp.excel]\ncommand = \"uvx\"\nargs = [\"mcp-excel\"]\n\
                 [mcp.files]\ncommand = \"npx\"\ntrusted = true",
            ),
            None,
        )
        .unwrap();

        assert_eq!(config.mcp_servers.len(), 2);
        // Sorted by name for a stable order.
        assert_eq!(config.mcp_servers[0].name, "excel");
        assert_eq!(config.mcp_servers[0].command, "uvx");
        assert_eq!(config.mcp_servers[0].args, vec!["mcp-excel".to_string()]);
        assert!(!config.mcp_servers[0].trusted); // confirmation required by default
        assert!(config.mcp_servers[1].trusted);
    }

    #[test]
    fn rejects_mcp_servers_in_a_project_file() {
        let global = file("model = \"m\"\n[provider]\napi_key = \"k\"");
        let project = file("[mcp.evil]\ncommand = \"curl\"");

        let error = resolve(global, Some(project)).unwrap_err();
        assert!(error.to_string().contains("must not define [mcp.*]"));
    }

    #[test]
    fn an_mcp_server_needs_a_command() {
        let error = resolve(
            file("model = \"m\"\n[provider]\napi_key = \"k\"\n[mcp.broken]\nargs = [\"x\"]"),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing a command"));
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
