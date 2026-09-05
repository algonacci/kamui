use crate::pricing::{ModelPrice, Prices};
use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const CONFIG_FILE: &str = "kamui.toml";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_PROFILE_NAME: &str = "default";
/// Default foreground `run_command` timeout, applied when `[commands].timeout_secs` is unset.
const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 30;
/// Default safety cap on a `background: true` job's total lifetime, applied when
/// `[commands].background_max_secs` is unset. A backstop against runaway/zombie processes, not a
/// limit meant to constrain legitimate long-running commands.
const DEFAULT_BACKGROUND_MAX_SECS: u64 = 30 * 60;

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

# Orvix Coding Plan (internal): POST /coding/completions with sticky session_id.
# Keep base_url on /v1 so /models still works; override only the chat path.
# completions_path = \"/coding/completions\"
# send_session_id = true

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
# enabled = false        # opt out without deleting (opencode-compatible)
# [mcp.excel.env]        # extra environment for the child process
# KEY = \"value\"

# Commands run_command may run without asking for approval. Exact match only (after
# trimming), so this never widens beyond exactly what is listed. Global-only, like
# api_key: a project kamui.toml may not define this.
#
# [permissions]
# allow_commands = [\"git status\", \"git diff\", \"cargo check\"]

# run_command timeout policy. Not security-relevant, so a project kamui.toml may
# override these. timeout_secs bounds a normal (foreground) command; background_max_secs
# is a safety cap on a background: true job's total lifetime, not a limit meant to
# constrain a legitimately long-running command.
#
# [commands]
# timeout_secs = 30
# background_max_secs = 1800

# Theme: default | catppuccin | ayu-dark | ayuppuccin (also per-project override)
# theme = \"ayuppuccin\"

# Optional: an embedding-capable model on this same provider, enabling /index and the
# search_code tool. Not every model/endpoint offers an embeddings API; omit this to
# leave semantic search unavailable.
# [provider]
# embedding_model = \"text-embedding-3-small\"

# Optional: what each model costs, so /stats and /usage can report spend as well as
# tokens. Prices are per MILLION tokens, the unit provider pricing pages use, and input
# and output are separate rates. Kamui does not know or convert currencies: the numbers
# are in whatever currency you typed, and `currency` is only the symbol printed with
# them. Models you do not price are reported as \"unpriced\", never as free. Prices are
# not secret, so a project kamui.toml may set them too.
#
# [pricing]
# currency = \"$\"
#
# [pricing.models.\"gpt-4o\"]
# input_per_million = 2.50
# output_per_million = 10.00
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
    /// The embedding-capable model to use for `/index`/`search_code`, on the same provider
    /// (base_url/api_key) as this profile. `None` means semantic search is unavailable for this
    /// profile — not every model/endpoint offers an embeddings API.
    pub embedding_model: Option<String>,
    /// Override the chat completions URL/path (see `OpenAIProvider::chat_url`).
    pub completions_path: Option<String>,
    /// Send Kamui's session id as top-level `session_id` (required by Orvix Coding Plan).
    pub send_session_id: bool,
}

/// An MCP server Kamui launches and talks to over stdio.
#[derive(Debug, Clone)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// When true, this server's tools run without per-call approval.
    pub trusted: bool,
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    pub headers: HashMap<String, String>,
    /// Whether the server participates in the current session. Disabled servers are kept
    /// (rather than dropped) so `/mcp` can list and re-enable them.
    pub enabled: bool,
}

/// Fully resolved runtime configuration: every available profile plus the default choice.
#[derive(Debug)]
pub struct Config {
    pub profiles: Vec<Profile>,
    pub default_profile: String,
    pub mcp_servers: Vec<McpServer>,
    /// Exact-match commands `run_command` runs without asking for approval. Global-only, like
    /// `api_key`: a project file could otherwise silently grant itself unattended execution.
    pub allow_commands: Vec<String>,
    /// Foreground `run_command` timeout. Not security-relevant, so (unlike `allow_commands`) a
    /// project file may override it, the same way it may override `context_window`.
    pub command_timeout_secs: u64,
    /// Safety cap on a `background: true` job's total lifetime.
    pub background_max_secs: u64,
    /// Per-model prices for optional cost reporting in `/stats` and `/usage`. Empty unless the
    /// user configured `[pricing.models]`, and cost is left out of both reports entirely when it is.
    pub prices: Prices,
    pub theme: crate::theme::Theme,
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
    theme: Option<String>,
    /// Named, shared provider credentials that profiles can reference by name.
    #[serde(default)]
    providers: HashMap<String, ProviderSection>,
    #[serde(default)]
    profiles: HashMap<String, ProfileSection>,
    /// MCP servers to launch. Global-only: a project file must not spawn processes.
    #[serde(default)]
    mcp: HashMap<String, McpSection>,
    /// Commands `run_command` may run without approval. Global-only: a checked-in project file
    /// could otherwise grant itself unattended execution.
    #[serde(default)]
    permissions: PermissionsSection,
    /// `run_command` timeout policy. Not security-relevant, so a project file may override it.
    #[serde(default)]
    commands: CommandsSection,
    /// Optional per-model prices. Like `[commands]`, and unlike `[permissions]`, a project file may
    /// set these: a price is not a secret and grants nothing — it only changes a displayed number.
    #[serde(default)]
    pricing: PricingSection,
}

#[derive(Debug, Default, Deserialize)]
struct PricingSection {
    /// Display label for the amounts. Kamui never converts currencies.
    currency: Option<String>,
    /// Model identifier -> price. Nested under `models` rather than sitting directly in
    /// `[pricing]` so `currency` cannot collide with a model that happens to be named after it.
    #[serde(default)]
    models: HashMap<String, ModelPrice>,
}

#[derive(Debug, Default, Deserialize)]
struct CommandsSection {
    timeout_secs: Option<u64>,
    background_max_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct McpSection {
    #[serde(default)]
    enabled: Option<bool>,
    /// opencode names this `type`; accept both spellings.
    #[serde(default, alias = "type")]
    kind: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    trusted: bool,
    #[serde(default)]
    env: HashMap<String, String>,
    /// opencode uses `environment`, accept both
    #[serde(default)]
    environment: HashMap<String, String>,
    /// Remote server (`type = "remote"`): streamable-http URL + optional headers.
    url: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct PermissionsSection {
    #[serde(default)]
    allow_commands: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderSection {
    base_url: Option<String>,
    api_key: Option<String>,
    tools: Option<bool>,
    embedding_model: Option<String>,
    /// See [`Profile::completions_path`].
    completions_path: Option<String>,
    /// See [`Profile::send_session_id`].
    #[serde(default)]
    send_session_id: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileSection {
    /// Name of a `[providers.*]` block to inherit base_url/api_key/tools/embedding_model from.
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    context_window: Option<u64>,
    tools: Option<bool>,
    embedding_model: Option<String>,
    completions_path: Option<String>,
    #[serde(default)]
    send_session_id: Option<bool>,
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
fn resolve(mut global: ConfigFile, mut project: Option<ConfigFile>) -> Result<Config> {
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
        // An allowlisted command skips the approval prompt entirely, so a checked-in project file
        // could otherwise grant itself unattended execution the same way an embedded api_key could
        // leak a credential.
        if !project.permissions.allow_commands.is_empty() {
            anyhow::bail!(
                "a project kamui.toml must not define [permissions]; declare allow_commands in the global config"
            );
        }
        // Only the global file's profiles and providers are resolved. Accepting these here and
        // then ignoring them left `default_profile` failing with "does not match any [profiles.*]
        // entry" while the file being read plainly contained one.
        if !project.profiles.is_empty() || !project.providers.is_empty() {
            anyhow::bail!(
                "a project kamui.toml must not define [profiles.*] or [providers.*]; they are resolved from the global config only. Use `default_profile` here to pin this project to one of the profiles defined there."
            );
        }
    }

    let command_timeout_secs = project
        .as_ref()
        .and_then(|file| file.commands.timeout_secs)
        .or(global.commands.timeout_secs)
        .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS);
    let background_max_secs = project
        .as_ref()
        .and_then(|file| file.commands.background_max_secs)
        .or(global.commands.background_max_secs)
        .unwrap_or(DEFAULT_BACKGROUND_MAX_SECS);

    let prices = resolve_prices(
        std::mem::take(&mut global.pricing),
        project
            .as_mut()
            .map(|file| std::mem::take(&mut file.pricing)),
    )?;

    let mcp_servers = resolve_mcp_servers(std::mem::take(&mut global.mcp))?;
    let allow_commands = std::mem::take(&mut global.permissions).allow_commands;
    let theme = project
        .as_ref()
        .and_then(|f| f.theme.clone())
        .or(global.theme.clone())
        .as_deref()
        .map(|s| s.parse::<crate::theme::Theme>())
        .transpose()
        .map_err(|e| anyhow::anyhow!(e))?
        .unwrap_or_default();
    // also allow DB override if toml had default and DB was set previously — toml wins when explicitly set

    let mut config = if global.profiles.is_empty() {
        resolve_flat(global, project)?
    } else {
        resolve_profiles(global, project)?
    };
    config.mcp_servers = mcp_servers;
    config.allow_commands = allow_commands;
    config.command_timeout_secs = command_timeout_secs;
    config.background_max_secs = background_max_secs;
    config.prices = prices;
    config.theme = theme;
    Ok(config)
}

/// Merge the global and project `[pricing]` sections into one price table. Merging is per model,
/// not whole-section: a project file can price a model the global file never mentioned without
/// having to restate the rest. A project file may set prices at all — unlike `[permissions]` or
/// `api_key` — because a price grants no capability and reveals no secret; the worst a wrong one
/// can do is print a wrong number in a report.
fn resolve_prices(global: PricingSection, project: Option<PricingSection>) -> Result<Prices> {
    let mut currency = global.currency;
    let mut models = global.models;
    if let Some(project) = project {
        currency = project.currency.or(currency);
        models.extend(project.models);
    }
    Prices::new(currency, models)
}

/// Turn `[mcp.<name>]` blocks into launchable server definitions, ordered by name. Disabled
/// servers are kept (with `enabled = false`) rather than dropped, so `/mcp` can list and
/// re-enable them; `mcp::connect_all` skips them.
fn resolve_mcp_servers(sections: HashMap<String, McpSection>) -> Result<Vec<McpServer>> {
    let mut servers = Vec::with_capacity(sections.len());
    for (name, section) in sections {
        let enabled = section.enabled.unwrap_or(true);
        let is_remote = section
            .kind
            .as_deref()
            .is_some_and(|k| k.eq_ignore_ascii_case("remote"));
        if is_remote {
            let url = section
                .url
                .clone()
                .with_context(|| format!("mcp server '{name}' is remote but has no url"))?;
            servers.push(McpServer {
                name,
                command: String::new(),
                args: Vec::new(),
                trusted: section.trusted,
                env: HashMap::new(),
                url: Some(url),
                headers: section.headers,
                enabled,
            });
            continue;
        }
        let command = section
            .command
            .with_context(|| format!("mcp server '{name}' is missing a command"))?;
        let mut env = section.env;
        env.extend(section.environment);
        servers.push(McpServer {
            name,
            command,
            args: section.args,
            trusted: section.trusted,
            env,
            url: None,
            headers: HashMap::new(),
            enabled,
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

    let embedding_model = project_provider
        .and_then(|provider| provider.embedding_model.clone())
        .or_else(|| global_provider.and_then(|provider| provider.embedding_model.clone()));

    let completions_path = project_provider
        .and_then(|provider| provider.completions_path.clone())
        .or_else(|| global_provider.and_then(|provider| provider.completions_path.clone()));

    let send_session_id = project_provider
        .and_then(|provider| provider.send_session_id)
        .or_else(|| global_provider.and_then(|provider| provider.send_session_id))
        .unwrap_or(false);

    let profile = Profile {
        name: DEFAULT_PROFILE_NAME.to_string(),
        model,
        base_url,
        api_key,
        context_window,
        tools,
        embedding_model,
        completions_path,
        send_session_id,
    };
    Ok(Config {
        default_profile: profile.name.clone(),
        profiles: vec![profile],
        mcp_servers: Vec::new(),
        allow_commands: Vec::new(),
        command_timeout_secs: DEFAULT_COMMAND_TIMEOUT_SECS,
        background_max_secs: DEFAULT_BACKGROUND_MAX_SECS,
        // Overwritten by `resolve`, which is the only place that sees both files.
        prices: Prices::default(),
        theme: crate::theme::Theme::default(),
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
        let embedding_model = section
            .embedding_model
            .clone()
            .or_else(|| shared.and_then(|provider| provider.embedding_model.clone()));
        let completions_path = section
            .completions_path
            .clone()
            .or_else(|| shared.and_then(|provider| provider.completions_path.clone()));
        let send_session_id = section
            .send_session_id
            .or_else(|| shared.and_then(|provider| provider.send_session_id))
            .unwrap_or(false);
        profiles.push(Profile {
            name: name.clone(),
            model,
            base_url,
            api_key,
            context_window: section.context_window,
            tools,
            embedding_model,
            completions_path,
            send_session_id,
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
        allow_commands: Vec::new(),
        command_timeout_secs: DEFAULT_COMMAND_TIMEOUT_SECS,
        background_max_secs: DEFAULT_BACKGROUND_MAX_SECS,
        // Overwritten by `resolve`, which is the only place that sees both files.
        prices: Prices::default(),
        theme: crate::theme::Theme::default(),
    })
}

/// The OS configuration directory Kamui owns (`.../kamui`). Public so other modules that keep
/// user-editable files beside `kamui.toml` — currently `commands::global_dir` — resolve the same
/// location instead of duplicating the platform lookup.
pub fn global_config_dir() -> Result<PathBuf> {
    BaseDirs::new()
        .map(|dirs| dirs.config_dir().join("kamui"))
        .context("could not determine the operating system config directory")
}

/// Appends a `[profiles.<name>]` block to the global `kamui.toml`, creating the file if
/// needed. Used by the TUI "Add provider" flow; the name is sanitized and de-duplicated so a
/// repeated append can never produce a TOML table collision.
pub fn save_theme(path: &Path, theme: &str) -> Result<()> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml::Value = if content.trim().is_empty() {
        toml::Value::Table(Default::default())
    } else {
        toml::from_str(&content).unwrap_or(toml::Value::Table(Default::default()))
    };
    let table = doc
        .as_table_mut()
        .context("global config must be a TOML table")?;
    table.insert("theme".to_string(), toml::Value::String(theme.to_string()));
    let rendered = toml::to_string_pretty(&doc).context("could not serialize theme config")?;
    std::fs::write(path, rendered)?;
    Ok(())
}

/// Toggle an `[mcp.<name>]` server's `enabled` flag in the global `kamui.toml`. The runtime
/// connection is made at startup, so the change applies on the next launch; `/mcp` reports that.
pub fn set_mcp_enabled(path: &Path, name: &str, enabled: bool) -> Result<()> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml::Value = if content.trim().is_empty() {
        toml::Value::Table(Default::default())
    } else {
        toml::from_str(&content).unwrap_or(toml::Value::Table(Default::default()))
    };
    let mcp = doc
        .as_table_mut()
        .context("global config must be a TOML table")?
        .entry("mcp")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("[mcp] must be a TOML table")?;
    let server = mcp
        .entry(name.to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("mcp server entry must be a TOML table")?;
    server.insert("enabled".to_string(), toml::Value::Boolean(enabled));
    let rendered = toml::to_string_pretty(&doc).context("could not serialize mcp config")?;
    std::fs::write(path, rendered)?;
    Ok(())
}

pub fn append_profile(path: &Path, base_url: &str, api_key: &str, model: &str) -> Result<String> {
    use std::io::Write;

    let mut name: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let name = name.as_mut_str();
    // De-duplicate: read existing names from the file text.
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let taken: Vec<String> = existing
        .lines()
        .filter_map(|l| l.strip_prefix("[profiles."))
        .map(|l| l.trim_end_matches(']').trim().to_string())
        .collect();
    let mut final_name = name.to_string();
    let mut counter = 2;
    while taken.iter().any(|t| t.eq_ignore_ascii_case(&final_name)) {
        final_name = format!("{name}-{counter}");
        counter += 1;
    }

    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::File::create(path)?;
    }
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    writeln!(file)?;
    writeln!(file, "[profiles.{final_name}]")?;
    writeln!(file, "model = \"{model}\"")?;
    writeln!(file, "base_url = \"{base_url}\"")?;
    writeln!(file, "api_key = \"{api_key}\"")?;
    Ok(final_name)
}

pub(crate) fn global_config_path() -> Result<PathBuf> {
    global_config_dir().map(|dir| dir.join(CONFIG_FILE))
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
    fn orvix_coding_provider_inherits_completions_path_and_session_flag() {
        let global = file(
            r#"
default_profile = "flash"
[providers.orvix-coding]
base_url = "https://api.orvix.id/v1"
api_key = "sk-test"
completions_path = "/coding/completions"
send_session_id = true
[profiles.flash]
provider = "orvix-coding"
model = "orvix/deepseek-v4-flash"
"#,
        );
        let profile = resolve(global, None).unwrap().default().clone();
        assert_eq!(profile.base_url, "https://api.orvix.id/v1");
        assert_eq!(
            profile.completions_path.as_deref(),
            Some("/coding/completions")
        );
        assert!(profile.send_session_id);
    }

    #[test]
    fn a_project_file_may_not_define_profiles_or_providers() {
        // Only the global file's profiles are resolved. Accepting these and ignoring them made
        // `default_profile` fail with "does not match any [profiles.*] entry" while the file the
        // user was staring at plainly contained one.
        let global = file(
            r#"
default_profile = "a"
[profiles.a]
model = "m"
api_key = "k"
"#,
        );
        let project = file(
            r#"
[profiles.dead]
model = "nothing"
"#,
        );

        let error = resolve(global, Some(project)).expect_err("project profiles are rejected");
        let text = format!("{error:#}");
        assert!(text.contains("[profiles.*]"), "{text}");
        assert!(
            text.contains("default_profile"),
            "it points at what does work here: {text}"
        );
    }

    #[test]
    fn a_project_file_may_still_pin_the_default_profile() {
        let global = file(
            r#"
default_profile = "a"
[profiles.a]
model = "m"
api_key = "k"
[profiles.b]
model = "n"
api_key = "k"
"#,
        );
        let project = file(r#"default_profile = "b""#);

        let config = resolve(global, Some(project)).expect("pinning is allowed");
        assert_eq!(config.default().name, "b");
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
    fn pricing_is_absent_unless_it_is_configured() {
        let config = resolve(
            file("model = \"gpt-5\"\n[provider]\napi_key = \"sk-1\""),
            None,
        )
        .unwrap();

        assert!(config.prices.is_empty());
    }

    #[test]
    fn a_project_file_may_price_models_and_override_one_globally_priced() {
        let global = file(
            "model = \"gpt-5\"\n[provider]\napi_key = \"sk-1\"\n\
             [pricing]\ncurrency = \"$\"\n\
             [pricing.models.\"gpt-5\"]\ninput_per_million = 1.0\noutput_per_million = 2.0",
        );
        let project = file(
            "[pricing.models.\"gpt-5\"]\ninput_per_million = 3.0\noutput_per_million = 4.0\n\
             [pricing.models.\"codeqwen:latest\"]\ninput_per_million = 0.0\noutput_per_million = 0.0",
        );

        let prices = resolve(global, Some(project)).unwrap().prices;

        // The project file wins for the model it restates, and adds one the global file never named.
        assert_eq!(
            prices.format(&prices.tally([(Some("gpt-5"), 1_000_000, 0)])),
            "$3.0000"
        );
        assert_eq!(
            prices.format(&prices.tally([(Some("codeqwen:latest"), 1_000_000, 0)])),
            "$0.0000"
        );
    }

    #[test]
    fn an_impossible_price_is_rejected_rather_than_reported() {
        let error = resolve(
            file(
                "model = \"gpt-5\"\n[provider]\napi_key = \"sk-1\"\n\
                 [pricing.models.\"gpt-5\"]\ninput_per_million = -1.0\noutput_per_million = 1.0",
            ),
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("non-negative"));
    }

    /// Input and output are billed at different rates, so half a price is not a price.
    #[test]
    fn a_price_must_state_both_directions() {
        let error =
            toml::from_str::<ConfigFile>("[pricing.models.\"gpt-5\"]\ninput_per_million = 1.0")
                .unwrap_err();

        assert!(error.to_string().contains("output_per_million"));
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
    fn an_mcp_server_can_be_disabled_and_forward_env() {
        let config = resolve(
            file(
                "model = \"m\"\n[provider]\napi_key = \"k\"\n\
                 [mcp.on]\ncommand = \"srv\"\nenabled = true\n\
                 [mcp.on.environment]\nTOKEN = \"t\"\n\
                 [mcp.off]\ncommand = \"srv\"\nenabled = false",
            ),
            None,
        )
        .unwrap();

        assert_eq!(config.mcp_servers.len(), 2, "disabled servers are kept");
        let on = config.mcp_servers.iter().find(|s| s.name == "on").unwrap();
        assert!(on.enabled);
        assert_eq!(on.env.get("TOKEN").map(String::as_str), Some("t"));
        let off = config.mcp_servers.iter().find(|s| s.name == "off").unwrap();
        assert!(!off.enabled);
    }

    #[test]
    fn a_remote_mcp_server_parses_without_a_command() {
        let config = resolve(
            file(
                "model = \"m\"\n[provider]\napi_key = \"k\"\n\
                 [mcp.remote]\ntype = \"remote\"\nurl = \"https://example.com/mcp\"\n\
                 [mcp.remote.headers]\nX-Api-Key = \"secret\"",
            ),
            None,
        )
        .unwrap();

        assert_eq!(config.mcp_servers.len(), 1);
        let remote = &config.mcp_servers[0];
        assert_eq!(remote.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(remote.command, "");
        assert_eq!(
            remote.headers.get("X-Api-Key").map(String::as_str),
            Some("secret")
        );
    }

    #[test]
    fn resolves_the_command_allowlist_from_the_global_file() {
        let config = resolve(
            file(
                "model = \"m\"\n[provider]\napi_key = \"k\"\n\
                 [permissions]\nallow_commands = [\"git status\", \"cargo check\"]",
            ),
            None,
        )
        .unwrap();

        assert_eq!(
            config.allow_commands,
            vec!["git status".to_string(), "cargo check".to_string()]
        );
    }

    #[test]
    fn rejects_an_allowlist_in_a_project_file() {
        let global = file("model = \"m\"\n[provider]\napi_key = \"k\"");
        let project = file("[permissions]\nallow_commands = [\"rm -rf /\"]");

        let error = resolve(global, Some(project)).unwrap_err();
        assert!(error.to_string().contains("must not define [permissions]"));
    }

    #[test]
    fn command_limits_default_when_unset() {
        let config = resolve(file("model = \"m\"\n[provider]\napi_key = \"k\""), None).unwrap();

        assert_eq!(config.command_timeout_secs, DEFAULT_COMMAND_TIMEOUT_SECS);
        assert_eq!(config.background_max_secs, DEFAULT_BACKGROUND_MAX_SECS);
    }

    #[test]
    fn a_project_may_override_command_limits() {
        let global = file(
            "model = \"m\"\n[provider]\napi_key = \"k\"\n[commands]\ntimeout_secs = 60\nbackground_max_secs = 600",
        );
        let project = file("[commands]\ntimeout_secs = 120");

        let config = resolve(global, Some(project)).unwrap();

        assert_eq!(config.command_timeout_secs, 120); // project overrides global
        assert_eq!(config.background_max_secs, 600); // falls back to global
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
