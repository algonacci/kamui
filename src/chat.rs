use crate::cache;
use crate::commands;
use crate::compaction;
use crate::config::{Config, Profile};
use crate::context::ProjectContext;
use crate::markdown;
use crate::mcp::ConnectionStatus;
use crate::pricing::Prices;
use crate::prompt;
use crate::provider::{ChatRequest, Message, Provider, Role, StreamEvent, ToolCall, Usage};
use crate::render;
use crate::storage;
use crate::storage::{Database, Session};
use crate::terminal::{Style, Ui};
use crate::tools;
use crate::tools::ToolRegistry;
use crate::ui::{self, ChatUi, HubEvent, InputHub};
use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use dialoguer::console::Term;
use futures_util::future::join_all;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{path::Path, process::Command};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

const RESUME_PREVIEW_MESSAGES: usize = 6;
/// How many stored messages `/resume` replays into the transcript.
const RESUME_REPLAY_MESSAGES: usize = 10;
/// Upper bound on model/tool round-trips within a single user turn, to stop runaway tool loops.
/// Generous enough for multi-file edits while still bounding a stuck loop.
const MAX_TOOL_ROUNDS: usize = 25;
const MAX_CONCURRENT_SUB_AGENTS: usize = 4;
const EMBEDDING_BATCH_SIZE: usize = 64;
/// Settings key for the persisted active provider profile.
const ACTIVE_PROFILE_KEY: &str = "active_profile";

#[allow(clippy::too_many_arguments)]
pub async fn start_chat<F>(
    mut config: Config,
    tools: ToolRegistry,
    mut mcp_statuses: Vec<ConnectionStatus>,
    database: &Database,
    project: &ProjectContext,
    resume_id: Option<String>,
    auto_approve: bool,
    build_provider: F,
) -> Result<()>
where
    F: Fn(&Profile) -> Box<dyn Provider>,
{
    // Pick the active profile: a persisted choice if it still exists, otherwise the default.
    let active_name = database
        .get_setting(ACTIVE_PROFILE_KEY)?
        .filter(|name| config.find(name).is_some())
        .unwrap_or_else(|| config.default_profile.clone());
    let mut active = config
        .find(&active_name)
        .cloned()
        .unwrap_or_else(|| config.default().clone());
    if let Some(th) = database
        .get_setting("active_theme")?
        .and_then(|s| s.parse::<crate::theme::Theme>().ok())
    {
        config.theme = th;
    }
    let mut provider = build_provider(&active);
    let mut context_window = active.context_window;
    let job_registry = tools.jobs();
    let command_library = commands::CommandLibrary::load(project.root());
    let mut skill_library = crate::skills::SkillLibrary::load(project.root());
    // Warnings captured when `/warnings fix` was invoked, so the turn that repairs them can
    // be compared against a fresh load and reported instead of ending silently.
    let mut pending_skill_fix: Option<Vec<String>> = None;
    let use_tui = crate::tui::is_interactive();
    let mut chat_ui = crate::ui::ChatUi::new_with_theme(
        use_tui,
        format!(
            "Kamui v{} · {} · {}",
            env!("CARGO_PKG_VERSION"),
            active.model,
            display_path(project.root())
        ),
        config.theme.clone(),
    )?;
    // The keyboard hub owns all TUI input for the session: editor always live, Enter queues
    // while the agent runs, Esc interrupts.
    let mut hub = chat_ui.screen_handle().map(InputHub::spawn);
    let interrupt = hub.as_ref().map(|h| h.interrupt.clone());
    if let Some(hub) = hub.as_ref() {
        refresh_model_source(&config, hub);
        refresh_session_source(database, hub);
        if let Ok(candidates) = project.at_path_candidates() {
            hub.set_path_candidates(candidates);
        }
    }
    let ui = Ui::stdio();
    let mcp_sidebar = mcp_sidebar_value(&mcp_statuses);
    // `--auto-approve` is now the starting point of a cycle rather than a fixed property of the
    // session: Tab / Shift+Tab move between build, auto, and plan while it runs.
    let mut mode = if auto_approve {
        Mode::Auto
    } else {
        Mode::Build
    };
    let mut auto_approve = auto_approve;
    if let Some(note) = tools_disabled_note(&active) {
        chat_ui.warning(&note)?;
    }
    // One tidy startup line instead of a wall of per-skill warnings; /skills still lists every
    // individual reason.
    let skill_warning_count = skill_library.warnings().len();
    if skill_warning_count > 0 {
        if use_tui {
            chat_ui.warning(&format!(
                "{skill_warning_count} skill folder(s) skipped (invalid name or frontmatter) — /warnings details, /warnings fix"
            ))?;
            chat_ui.set_warning_details(
                skill_library
                    .warnings()
                    .iter()
                    .map(|w| w.to_string())
                    .collect(),
            )?;
        } else {
            for warning in skill_library.warnings() {
                eprintln!("warning: {warning}");
            }
        }
    }

    if !use_tui {
        print_status(
            project,
            &active,
            &tools,
            &mcp_statuses,
            &config.allow_commands,
        );
        println!("Data: {}", database.path().display());
    }
    // A hint, not an action: an out-of-date index is only worth mentioning to someone who already
    // runs `/index`, and refreshing it costs the user's own embedding budget, so Kamui reports the
    // drift and leaves the decision to them. Interactive chat only — `-p` output is script input.
    // A failed check is not worth interrupting startup over; semantic search still works without it.
    if active.embedding_model.is_some()
        && let Ok(Some(staleness)) = index_staleness(database, project)
        && !staleness.is_fresh()
    {
        chat_ui.notice(&format!(
            "Index: {} since last /index — run /index to refresh.",
            staleness.describe()
        ))?;
    }
    if auto_approve {
        chat_ui
            .warning("--auto-approve is active: commands and file edits will run without asking")?;
    }

    let (mut session, mut messages) = match resume_id {
        Some(id) => {
            let session = resolve_session(database, &id)?;
            if session.provider != provider.name() {
                anyhow::bail!(
                    "session uses provider '{}', but '{}' is active",
                    session.provider,
                    provider.name()
                );
            }
            let messages = database.load_messages(&session.id)?;
            chat_ui.notice(&format!(
                "Resuming: {} ({})",
                session.title,
                short_id(&session.id)
            ))?;
            if use_tui {
                replay_tui_history(&mut chat_ui, &messages)?;
            } else {
                print_history_preview(&messages);
            }
            (Some(session), messages)
        }
        None => (None, Vec::new()),
    };
    update_sidebar(
        &mut chat_ui,
        session.as_ref(),
        &model_label(&active),
        mode.label(),
        env!("CARGO_PKG_VERSION"),
        project,
        &mcp_sidebar,
        None,
        None,
        context_window,
        active.send_session_id,
        session
            .as_ref()
            .and_then(|s| session_cache_line(database, &s.id)),
        None,
        None,
    );
    let mut plan_mode: Option<PlanModeState> = session
        .as_ref()
        .and_then(|s| database.get_plan(&s.id).ok().flatten())
        .and_then(|(json, status)| {
            let status = match status.as_str() {
                "pending" => PlanStatus::Pending,
                "approved" => PlanStatus::Approved,
                _ => return None,
            };
            Some(PlanModeState {
                status,
                plan_json: Some(json),
            })
        });
    if let Some(state) = plan_mode.as_ref() {
        mode = if state.status == PlanStatus::Pending {
            Mode::Plan
        } else {
            Mode::Build
        };
        auto_approve = false;
        update_sidebar(
            &mut chat_ui,
            session.as_ref(),
            &model_label(&active),
            mode.label(),
            env!("CARGO_PKG_VERSION"),
            project,
            &mcp_sidebar,
            None,
            None,
            context_window,
            active.send_session_id,
            session
                .as_ref()
                .and_then(|s| session_cache_line(database, &s.id)),
            None,
            None,
        );
    }
    if let Some(state) = plan_mode.as_ref()
        && state.status == PlanStatus::Pending
        && let Some(json) = state.plan_json.as_deref()
        && let Some(rendered) = tools::render_plan(json)
    {
        chat_ui.notice(&format!("Plan Mode — pending plan\n{rendered}"))?;
    }
    let mut input_rx = if use_tui { None } else { Some(input_channel()) };
    let mut disabled_skills = crate::settings::load_disabled_skills(project.root());

    // Prompt-cache prefix watch. Only cache-pinned profiles (Orvix Coding Plan, `send_session_id`)
    // pay attention: everywhere else a changed prefix costs nothing worth a notice.
    let mut prefix_guard = cache::PrefixGuard::new(active.send_session_id);
    // Rolling context compaction: `summary` folds in messages before `summarized_upto`; the rest of
    // `messages` is sent verbatim. Both reset whenever a command replaces the loaded history.
    let mut summary: Option<String> = None;
    let mut summarized_upto: usize = 0;
    // The most recently completed turn's pre-edit file snapshot, if it touched any files, so
    // `/undo` can revert it. `None` once nothing is left to undo.
    let mut last_turn_snapshot: Option<HashMap<PathBuf, Option<String>>> = None;
    // Tool names granted a standing "always allow" for the rest of this session (ported from
    // Kumo's "Always allow" approval button). Session-scoped: cleared whenever a chat effectively
    // restarts (`/new`, or `/delete` of the active session), same as `last_turn_snapshot`.
    let mut always_allowed: HashSet<String> = HashSet::new();
    // `/plan` forces the next turn into Plan Mode even for a small task.
    let mut plan_requested = false;
    // `/warnings` flips this; the transcript only renders the warning rail when it is set.
    let mut show_warnings = true;
    // In-flight "Add provider" wizard: base URL + API key awaiting a picked model id.
    let mut pending_add: Option<(String, String)> = None;
    // Background jobs already reported as finished, so each is announced once.
    let mut announced_jobs: HashSet<String> = HashSet::new();
    // One fixed tool array per Session (prefix-cache stability): computed once from the
    // profile, never swapped per Turn. Plan Mode pending holds mutating tools at execution
    // time (`is_mutating_held`) instead of shrinking the roster, so the `tools` suffix of
    // the request prefix stays byte-identical across Turns. Reset on profile switch and
    // whenever a command replaces the session (same points that reset plan/compaction state).
    // ponytail: full roster in pending Plan Mode (model may call held tools, gets a hold
    // message); subset roster if the hold-message round-trips prove wasteful.
    let mut session_tools: Option<Vec<crate::provider::ToolDefinition>> = None;
    // Frozen request head per Session (prefix-cache stability): the base system prompt,
    // memory snapshot, and skill block are each their own message, computed once and
    // reused across Turns. Only the rolling Compaction summary (which grows by design)
    // is re-rendered per Turn, as the last head message before history.
    // `cached_memory_snapshot` holds the last rendered memory block; `memory_dirty`
    // flips when a memory Tool runs, so the next Turn re-reads the DB once instead of
    // every Turn — while a skill toggle rebuilds the head around the same snapshot.
    let mut head_messages: Option<Vec<Message>> = None;
    let mut cached_memory_snapshot = String::new();
    let mut memory_dirty = true;
    // Previous turn's cache + model for miss detection (`cache_miss_label`): a drop
    // beyond the noise floor means the prefix broke. `head_rebuilt_this_turn` marks
    // turns where we intentionally rebuilt the head (memory/skill change) so the
    // label names the cause. Model identity is tracked to label model-switch misses.
    let mut prev_cached: Option<u64> = None;
    let mut prev_model: Option<String> = None;
    let mut head_rebuilt_this_turn = false;

    'chat: loop {
        // A background job that ended could previously only be found by polling `/jobs`. Report
        // it on the way back to the prompt, which is when there is somewhere to put it.
        for line in tools::drain_finished_jobs(&job_registry, &mut announced_jobs) {
            chat_ui.notice(&line)?;
        }
        // Sticky Orvix Coding sessions need an id before the first provider call (including
        // /compact). Creating early is harmless for other providers: we only do it when asked.
        let coding_session_id = ensure_coding_session_id(
            &mut session,
            database,
            provider.name(),
            &active.model,
            active.send_session_id,
        )?;
        let input = if use_tui {
            let hub = hub.as_mut().expect("tui implies hub");
            let cmds: Vec<crate::commands::CustomCommand> = command_library.list().to_vec();
            let sks: Vec<crate::skills::Skill> = skill_library.list().to_vec();
            hub.set_candidates(crate::tui::slash_candidates(&cmds, &sks, &disabled_skills));
            chat_ui.prompt()?;
            // Queued lines typed while the agent ran are consumed first, in order.
            if let Some(queued) = hub.pop_queue() {
                queued
            } else {
                match hub.next().await {
                    Some(HubEvent::Line(line)) => line,
                    Some(HubEvent::Quit) | None => {
                        shutdown(
                            &mut chat_ui,
                            database,
                            session.as_ref(),
                            context_window,
                            &job_registry,
                            &config.prices,
                        )?;
                        break;
                    }
                }
            }
        } else {
            print!("{}", ui.style("\u{276f} ", &[Style::Cyan, Style::Bold]));
            io::stdout().flush()?;
            let rx = input_rx.as_mut().expect("plain mode has input channel");
            let line = tokio::select! {
                input = rx.recv() => match input {
                    Some(input) => input,
                    None => {
                        shutdown(
                        &mut chat_ui,
                        database,
                        session.as_ref(),
                        context_window,
                        &job_registry,
                        &config.prices,
                    )?;
                        break;
                    }
                },
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl+C")?;
                    println!();
                    shutdown(
                        &mut chat_ui,
                        database,
                        session.as_ref(),
                        context_window,
                        &job_registry,
                        &config.prices,
                    )?;
                    break;
                }
            };
            line
        };
        let input = input.trim();

        if input.eq_ignore_ascii_case("exit") || input == "/exit" {
            shutdown(
                &mut chat_ui,
                database,
                session.as_ref(),
                context_window,
                &job_registry,
                &config.prices,
            )?;
            break;
        }
        if input.is_empty() {
            continue;
        }
        // `!cmd` runs a shell command directly — no model, no approval, the human typed it.
        if let Some(direct) = input.strip_prefix('!') {
            let direct = direct.trim();
            if direct.is_empty() {
                chat_ui.notice("usage: !<command> runs it in the shell (e.g. !git status)")?;
                continue;
            }
            let started = Instant::now();
            let output = tools::run_direct_command(
                project.root(),
                direct,
                Duration::from_secs(config.command_timeout_secs),
            )
            .await;
            let (outcome, ok) = crate::terminal::tool_outcome_parts(&output, started.elapsed());
            chat_ui.tool_call("shell", direct)?;
            chat_ui.tool_finished(&outcome, ok, tool_body(&output))?;
            continue;
        }
        // Slash commands are UI operations, not conversation turns — opencode hides them
        // from the transcript too.
        if !input.starts_with('/') {
            chat_ui.user(input)?;
        }

        // A custom command (`/review`, ...) or skill (`/my-skill`, `/skill:my-skill`) expands
        // into this turn's prompt and then takes the ordinary path below. Built-in commands and
        // custom commands win over a same-named skill on bare `/<name>`; use `/skill:<name>` to
        // force the skill. The original line is kept for titling.
        let expanded_command = command_library.expand(input);
        let expanded_skill = if expanded_command.is_none() {
            skill_library.expand_filtered(input, &disabled_skills)
        } else {
            None
        };
        let expanded = expanded_command.as_deref().or(expanded_skill.as_deref());
        let title_source = input;
        let input: &str = expanded.unwrap_or(input);

        if expanded.is_none() && input.starts_with('/') && use_tui {
            chat_ui.leave_intro()?;
        }
        if expanded.is_none() && input.starts_with('/') {
            let (command, argument) = input.split_once(' ').unwrap_or((input, ""));
            let command = match resolve_builtin_command(command) {
                Ok(command) => command,
                Err(message) => {
                    chat_ui.notice(&format!("{message:#}"))?;
                    continue;
                }
            };
            let canonical_input = format!("{command} {}", argument.trim());
            // Open a cell headed by the command so its output is attributed to it rather than
            // merging into the flat run of status lines that used to sit below every card.
            chat_ui.command_echo(input)?;
            if command == "/help" && use_tui {
                chat_ui.toggle_help()?;
                continue;
            }
            // `/model` opens the picker; `__add__` runs the registry wizard; picking from
            // that wizard registers + switches; `<name>` switches directly.
            if command == "/model" && use_tui {
                let hub_ref = hub.as_mut().expect("tui implies hub");
                if argument == "__add__" {
                    chat_ui
                        .notice("Add provider — Base URL (Enter = https://api.openai.com/v1):")?;
                    let base = hub_ref.request_line().await.unwrap_or_default();
                    let base = base.trim().trim_end_matches('/');
                    let base = if base.is_empty() {
                        "https://api.openai.com/v1"
                    } else {
                        base
                    }
                    .to_string();
                    chat_ui.notice("API key (input is echoed):")?;
                    let key = hub_ref.request_line().await.unwrap_or_default();
                    let key = key.trim().to_string();
                    if key.is_empty() {
                        chat_ui.notice("Cancelled—empty API key.")?;
                        continue;
                    }
                    chat_ui.notice("Fetching models…")?;
                    match crate::provider::openai::OpenAIProvider::list_models(&key, &base).await {
                        Ok(models) if !models.is_empty() => {
                            chat_ui.notice(&format!("{} models found— pick one.", models.len()))?;
                            pending_add = Some((base, key));
                            hub_ref.open_dialog(
                                "Pick Model",
                                "/model __picked__ ",
                                models.iter().map(|m| (m.clone(), m.clone())).collect(),
                            );
                        }
                        Ok(_) => chat_ui.notice("Provider returned no models.")?,
                        Err(error) => {
                            chat_ui.error(&format!("Could not load models: {error:#}"))?
                        }
                    }
                    continue;
                }
                if let Some(rest) = argument.strip_prefix("__picked__ ") {
                    let Some((base, key)) = pending_add.as_ref() else {
                        chat_ui.error("No pending provider registration.")?;
                        continue;
                    };
                    let path = crate::config::global_config_path()?;
                    let name = crate::config::append_profile(&path, base, key, rest)?;
                    let profile = crate::config::Profile {
                        name: name.clone(),
                        model: rest.to_string(),
                        base_url: base.clone(),
                        api_key: key.clone(),
                        context_window: None,
                        tools: true,
                        embedding_model: None,
                        completions_path: None,
                        send_session_id: false,
                    };
                    active = profile.clone();
                    provider = build_provider(&active);
                    context_window = None;
                    database.set_setting(ACTIVE_PROFILE_KEY, &name)?;
                    config.profiles.push(profile);
                    pending_add = None;
                    refresh_model_source(&config, hub_ref);
                    update_sidebar(
                        &mut chat_ui,
                        session.as_ref(),
                        &model_label(&active),
                        mode.label(),
                        env!("CARGO_PKG_VERSION"),
                        project,
                        &mcp_sidebar,
                        None,
                        None,
                        context_window,
                        active.send_session_id,
                        session
                            .as_ref()
                            .and_then(|s| session_cache_line(database, &s.id)),
                        None,
                        None,
                    );
                    chat_ui.notice(&format!("Added & switched to {rest} (profile {name})."))?;
                    continue;
                }
                if argument.is_empty() {
                    let opened = hub_ref.open_models_dialog();
                    if !opened {
                        chat_ui.notice("No provider profiles configured. Use /model __add__.")?;
                    }
                    continue;
                }
                // A concrete name: fall through to handle_command's direct switch.
            }
            if command == "/sessions" && use_tui {
                let opened = hub
                    .as_ref()
                    .map(|h| h.open_sessions_dialog())
                    .unwrap_or(false);
                if !opened {
                    chat_ui.notice("No saved sessions yet.")?;
                }
                continue;
            }
            if command == "/resume" && argument.trim().is_empty() && use_tui {
                let opened = hub
                    .as_ref()
                    .map(|h| h.open_sessions_dialog())
                    .unwrap_or(false);
                if !opened {
                    chat_ui.notice("No saved sessions yet.")?;
                }
                continue;
            }
            if command == "/models" && use_tui {
                let opened = hub
                    .as_ref()
                    .map(|h| h.open_models_dialog())
                    .unwrap_or(false);
                if !opened {
                    chat_ui.notice("No provider profiles configured.")?;
                }
                continue;
            }
            if command == "/commands" {
                {
                    let mut buf = String::new();
                    if chat_ui.is_fullscreen() && command_library.is_empty() {
                        chat_ui.notice(
                            "Custom commands are listed in .kamui/commands and global kamui/commands.",
                        )?;
                        continue;
                    }
                    if chat_ui.is_fullscreen() {
                        // Fullscreen: list through the sink so it lands in the transcript.
                        let mut out_buf2 = String::new();
                        print_commands(&command_library, &mut out_buf2);
                        chat_ui.notice(out_buf2.trim_end())?;
                    } else if !command_library.is_empty() {
                        print_commands(&command_library, &mut buf);
                        print!("{buf}");
                    } else {
                        let mut out_buf2 = String::new();
                        print_commands(&command_library, &mut out_buf2);
                        print!("{out_buf2}");
                    }
                }
                continue;
            }
            if command == "/warnings" || command == "/warning" {
                match argument.to_ascii_lowercase().as_str() {
                    "" => {
                        show_warnings = !show_warnings;
                        chat_ui.set_warnings_visible(show_warnings)?;
                        chat_ui.notice(if show_warnings {
                            "Warnings shown."
                        } else {
                            "Warnings hidden. /warnings to show again."
                        })?;
                    }
                    "details" | "expand" => {
                        show_warnings = true;
                        chat_ui.set_warning_details(
                            skill_library
                                .warnings()
                                .iter()
                                .map(|w| w.to_string())
                                .collect(),
                        )?;
                        chat_ui.set_warnings_expanded(true)?;
                        chat_ui.set_warnings_visible(true)?;
                        chat_ui.notice("Warning details expanded.")?;
                    }
                    "collapse" => {
                        chat_ui.set_warnings_expanded(false)?;
                    }
                    "fix" => {
                        let details = skill_library.warnings().join("\n- ");
                        if details.is_empty() {
                            chat_ui.notice("No warnings to fix.")?;
                            continue;
                        }
                        let prompt = format!(
                            "Kamui's skill loader rejected these skill folders:\n- {details}\n\nInspect each path, repair the folder name and SKILL.md frontmatter (name + description required, lowercase-dash folder), and verify the loader would accept them. Do not delete skills unless they are clearly junk."
                        );
                        if let Some(h) = hub.as_ref() {
                            h.push_prompt(prompt);
                            pending_skill_fix = Some(skill_library.warnings().to_vec());
                            chat_ui.notice(&format!(
                                "Repairing {} flagged skill folder(s) \u{2014} the result is reported when the turn ends.",
                                skill_library.warnings().len()
                            ))?;
                        } else {
                            anyhow::bail!("/warnings fix requires interactive TUI mode");
                        }
                    }
                    other => {
                        chat_ui.notice(&format!(
                            "usage: /warnings [on|off|details|fix] (got \"{other}\")"
                        ))?;
                    }
                }
                continue;
            }
            if command == "/expand" {
                if !chat_ui.is_fullscreen() || !chat_ui.expand_last()? {
                    chat_ui.notice("Nothing to expand.")?;
                }
                continue;
            }
            if command == "/collapse" {
                if !chat_ui.is_fullscreen() || !chat_ui.collapse_last()? {
                    chat_ui.notice("Nothing to collapse.")?;
                }
                continue;
            }
            if command == "/skills" {
                let argument = argument.trim();
                // `/skills toggle <name>` is what the picker submits on Enter, and it works
                // typed directly too.
                if let Some(name) = argument.strip_prefix("toggle ") {
                    let name = name.trim();
                    match skill_library.list().iter().find(|skill| skill.name == name) {
                        Some(skill) => {
                            let now_disabled = !disabled_skills.contains(&skill.name);
                            match crate::settings::set_skill_disabled(
                                project.root(),
                                skill,
                                now_disabled,
                            ) {
                                Ok(()) => {
                                    disabled_skills =
                                        crate::settings::load_disabled_skills(project.root());
                                    // Skill block is part of the frozen head: refresh next turn.
                                    head_messages = None;
                                    head_rebuilt_this_turn = true;
                                    chat_ui.notice(&format!(
                                        "/{} is now {}",
                                        skill.name,
                                        if now_disabled { "disabled" } else { "enabled" }
                                    ))?;
                                }
                                Err(error) => {
                                    chat_ui.error(&format!("Could not save: {error:#}"))?;
                                }
                            }
                        }
                        None => chat_ui.notice(&format!("No skill named '{name}'."))?,
                    }
                    continue;
                }
                // A picker, through the same dialog machinery the model and session pickers use.
                // The previous popup drove `console::Term` on its own: it read keys from the
                // stdin the input thread was already blocked on, and painted raw ANSI into the
                // alternate screen ratatui owns and repaints over.
                let opened = if use_tui {
                    let items: Vec<(String, String)> = skill_library
                        .list()
                        .iter()
                        .map(|skill| {
                            let state = if disabled_skills.contains(&skill.name) {
                                "disabled"
                            } else {
                                "enabled"
                            };
                            (
                                skill.name.clone(),
                                format!("[{state}] {}", skill.description),
                            )
                        })
                        .collect();
                    hub.as_ref()
                        .map(|hub| hub.open_skills_dialog(items))
                        .unwrap_or(false)
                } else {
                    false
                };
                if !opened {
                    let mut buf = String::new();
                    print_skills(&skill_library, &disabled_skills, &mut buf);
                    if use_tui {
                        chat_ui.notice(buf.trim_end())?;
                    } else {
                        print!("{buf}");
                    }
                }
                continue;
            }
            if command == "/theme" || command == "/themes" {
                let arg = argument.trim();
                if arg.is_empty() && use_tui {
                    hub.as_ref().map(|h| h.open_themes_dialog());
                    continue;
                }
                if arg.is_empty() {
                    let cur = config.theme.clone();
                    let list: String = crate::theme::Theme::all()
                        .into_iter()
                        .map(|t| {
                            let star = if t == cur { "*" } else { " " };
                            format!("{star} {t}\n")
                        })
                        .collect();
                    chat_ui.notice(&format!(
                        "Themes (* = active):\n{list}\nUsage: /theme <name>"
                    ))?;
                } else {
                    match arg.parse::<crate::theme::Theme>() {
                        Ok(th) => {
                            let th2 = th.clone();
                            config.theme = th2.clone();
                            let _ = database.set_setting("active_theme", &th2.to_string());
                            let _ = crate::config::save_theme(
                                &crate::config::global_config_path().unwrap(),
                                &th2.to_string(),
                            );
                            chat_ui.set_theme(th2.clone());
                            chat_ui.notice(&format!(
                                "Theme switched to {th} — restart to fully re-theme chrome."
                            ))?;
                        }
                        Err(e) => chat_ui.error(&e)?,
                    }
                }
                continue;
            }
            if command == "/model" {
                let tui_sink = if use_tui { Some(&mut chat_ui) } else { None };
                if let Err(error) = switch_profile(
                    argument.trim(),
                    &config,
                    &mut active,
                    &mut provider,
                    &mut context_window,
                    database,
                    &build_provider,
                    tui_sink,
                ) {
                    if chat_ui.is_fullscreen() {
                        chat_ui.error(&format!("Command failed: {error:#}"))?;
                    } else {
                        eprintln!(
                            "{}",
                            ui.style(&format!("Command failed: {error:#}\n"), &[Style::Red])
                        );
                    }
                }
                if chat_ui.is_fullscreen() {
                    chat_ui.set_header(format!(
                        "Kamui v{} · {} · {}",
                        env!("CARGO_PKG_VERSION"),
                        active.model,
                        display_path(project.root())
                    ))?;
                }
                // Profile switch may flip tools/embedding_model/system: recompute fixed state.
                session_tools = None;
                head_messages = None;
                memory_dirty = true;
                continue;
            }
            if command == "/status" {
                if chat_ui.is_fullscreen() {
                    chat_ui.notice(&format!(
                        "Project: {} · Model: {} · Tools: {} · MCP: {}",
                        display_path(project.root()),
                        active.model,
                        tools.len(),
                        mcp_statuses.len()
                    ))?;
                } else {
                    print_status(
                        project,
                        &active,
                        &tools,
                        &mcp_statuses,
                        &config.allow_commands,
                    );
                }
                continue;
            }
            if command == "/mcp" {
                let arg = argument.trim();
                // `/mcp apply name:on|off ...` — the dialog submits one line covering every
                // mark, so the response is a single "config updated" notice.
                if let Some(rest) = arg.strip_prefix("apply ") {
                    let mut applied = 0usize;
                    let mut first_error = None;
                    for pair in rest.split_whitespace() {
                        let (name, want) = match pair.split_once(':') {
                            Some((n, w)) => (n, w),
                            None => continue,
                        };
                        let want_on = want.eq_ignore_ascii_case("on");
                        match set_mcp_enabled(&config, &mut mcp_statuses, name, want_on) {
                            Ok(()) => applied += 1,
                            Err(error) => {
                                if first_error.is_none() {
                                    first_error = Some(format!("{error:#}"));
                                }
                            }
                        }
                    }
                    match first_error {
                        Some(error) => chat_ui.error(&format!(
                            "MCP config updated ({applied} changed), but: {error}"
                        ))?,
                        None => chat_ui.notice("MCP config updated — restart kamui to apply.")?,
                    }
                    continue;
                }
                if let Some(name) = arg.strip_prefix("toggle ").map(str::trim) {
                    let name = name.trim();
                    if name.is_empty() {
                        chat_ui.notice("usage: /mcp toggle <server>")?;
                        continue;
                    }
                    match toggle_mcp(&config, &mut mcp_statuses, name) {
                        Ok(now_enabled) => {
                            let text = print_mcp(&mcp_statuses, &tools);
                            chat_ui.notice(&format!(
                                "{} is now {}.\nRestart kamui to apply.\n\n{text}",
                                name,
                                if now_enabled { "enabled" } else { "disabled" }
                            ))?;
                        }
                        Err(error) => chat_ui.error(&format!("Could not toggle: {error:#}"))?,
                    }
                    continue;
                }
                if arg.is_empty() && use_tui {
                    let items: Vec<(String, String)> = mcp_statuses
                        .iter()
                        .map(|s| {
                            let state = if s.disabled {
                                "disabled"
                            } else if s.error.is_some() {
                                "unavailable"
                            } else {
                                "enabled"
                            };
                            (
                                s.name.clone(),
                                format!("{state} · {} tool(s)", s.tool_count),
                            )
                        })
                        .collect();
                    let marks: Vec<bool> = mcp_statuses.iter().map(|s| !s.disabled).collect();
                    if !hub
                        .as_ref()
                        .map(|h| h.open_mcp_dialog(items, marks))
                        .unwrap_or(false)
                    {
                        chat_ui.notice("No MCP servers configured.")?;
                    }
                    continue;
                }
                let text = print_mcp(&mcp_statuses, &tools);
                chat_ui.notice(&text)?;
                continue;
            }
            if command == "/compact" {
                let outcome = tokio::select! {
                    result = run_compaction(
                        provider.as_ref(),
                        &active.model,
                        &messages,
                        summary.as_deref(),
                        summarized_upto,
                        cache::side_session_id(
                            coding_session_id.as_deref(),
                            cache::SideRequest::Compact,
                        ),
                    ) => result,
                    signal = tokio::signal::ctrl_c() => {
                        signal.context("failed to listen for Ctrl+C")?;
                        chat_ui.notice("interrupted — back to prompt")?;
                        continue;
                    }
                };
                match outcome {
                    Ok(Some((new_summary, new_upto, count))) => {
                        summary = Some(new_summary);
                        summarized_upto = new_upto;
                        chat_ui.notice(&format!(
                            "Compacted {count} earlier messages into the summary.{}",
                            cache_reset_note(active.send_session_id)
                        ))?;
                        prefix_guard.reset();
                    }
                    Ok(None) => chat_ui.notice("Not enough history to compact yet.")?,
                    Err(error) => {
                        if chat_ui.is_fullscreen() {
                            chat_ui.error(&format!("Compaction failed: {error:#}"))?;
                        } else {
                            eprintln!(
                                "{}",
                                ui.style(&format!("Compaction failed: {error:#}\n"), &[Style::Red])
                            );
                        }
                    }
                }
                continue;
            }
            // Tab / Shift+Tab in the editor submit `/mode next` and `/mode prev`, so cycling
            // reuses the ordinary command path instead of needing its own event.
            if command == "/mode" {
                let requested = match argument.trim() {
                    "" => Some(mode),
                    "next" => Some(mode.next()),
                    "prev" | "previous" => Some(mode.prev()),
                    other => Mode::parse(other),
                };
                let Some(requested) = requested else {
                    chat_ui.notice(&format!(
                        "usage: /mode [build|auto|plan|next|prev] (got \"{argument}\")"
                    ))?;
                    continue;
                };
                if requested != mode {
                    mode = requested;
                    match mode {
                        Mode::Build | Mode::Auto => {
                            auto_approve = mode == Mode::Auto;
                            plan_requested = false;
                            // Leaving Plan Mode has to clear the stored plan too, or resuming
                            // this session would drop straight back into it.
                            if plan_mode.take().is_some()
                                && let Some(session) = session.as_ref()
                            {
                                let _ = database.set_plan(&session.id, "{}", "approved");
                            }
                        }
                        Mode::Plan => {
                            auto_approve = false;
                            plan_requested = true;
                        }
                    }
                }
                chat_ui.notice(&format!("Mode: {}", mode.describe()))?;
                update_sidebar(
                    &mut chat_ui,
                    session.as_ref(),
                    &model_label(&active),
                    mode.label(),
                    env!("CARGO_PKG_VERSION"),
                    project,
                    &mcp_sidebar,
                    None,
                    None,
                    context_window,
                    active.send_session_id,
                    session
                        .as_ref()
                        .and_then(|s| session_cache_line(database, &s.id)),
                    None,
                    None,
                );
                continue;
            }
            if command == "/plan" {
                plan_requested = true;
                mode = Mode::Plan;
                auto_approve = false;
                update_sidebar(
                    &mut chat_ui,
                    session.as_ref(),
                    &model_label(&active),
                    mode.label(),
                    env!("CARGO_PKG_VERSION"),
                    project,
                    &mcp_sidebar,
                    None,
                    None,
                    context_window,
                    active.send_session_id,
                    session
                        .as_ref()
                        .and_then(|s| session_cache_line(database, &s.id)),
                    None,
                    None,
                );
                chat_ui.notice("Plan Mode requested — next turn will require a plan.")?;
                continue;
            }
            if command == "/undo" {
                match last_turn_snapshot.take() {
                    Some(snapshot) => {
                        chat_ui
                            .notice(&revert_snapshot(&snapshot).summary("from the last turn"))?;
                    }
                    None => chat_ui.notice("Nothing to undo.")?,
                }
                continue;
            }
            if command == "/jobs" {
                let text = format!(
                    "Session jobs:\n{}\n\nScheduled jobs:\n{}",
                    tools::describe_jobs(&job_registry),
                    crate::jobs::format_jobs(&database.list_scheduled_jobs()?)
                );
                chat_ui.notice(&text)?;
                continue;
            }
            if command == "/index" {
                // Indexing walks a whole project over the network; without this it looks frozen.
                let mut spinner = start_spinner("Indexing...", ui, &mut chat_ui);
                let outcome = tokio::select! {
                    result = run_index(provider.as_ref(), &active, database, project) => result,
                    signal = tokio::signal::ctrl_c() => {
                        signal.context("failed to listen for Ctrl+C")?;
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        chat_ui.notice("interrupted — back to prompt")?;
                        continue;
                    }
                };
                stop_spinner(&mut spinner, &mut chat_ui).await;
                match outcome {
                    Ok(summary) => chat_ui.notice(&summary)?,
                    Err(error) => {
                        if chat_ui.is_fullscreen() {
                            chat_ui.error(&format!("Index failed: {error:#}"))?;
                        } else {
                            eprintln!(
                                "{}",
                                ui.style(&format!("Index failed: {error:#}\n"), &[Style::Red])
                            );
                        }
                    }
                }
                continue;
            }
            let prev_session_id = session.as_ref().map(|s| s.id.clone());
            let messages_before = messages.len();
            if use_tui {
                chat_ui.leave_intro()?;
            }
            let tui_sink = if use_tui { Some(&mut chat_ui) } else { None };
            if let Err(error) = handle_command(
                &canonical_input,
                provider.as_ref(),
                context_window,
                database,
                &mut session,
                &mut messages,
                &mut always_allowed,
                &mut last_turn_snapshot,
                &config.prices,
                tui_sink,
            ) {
                if chat_ui.is_fullscreen() {
                    chat_ui.error(&format!("Command failed: {error:#}"))?;
                } else {
                    eprintln!(
                        "{}",
                        ui.style(&format!("Command failed: {error:#}\n"), &[Style::Red])
                    );
                }
            }
            // Sidebar follows /new //resume: session title, id, and context reset.
            if use_tui {
                update_sidebar(
                    &mut chat_ui,
                    session.as_ref(),
                    &model_label(&active),
                    mode.label(),
                    env!("CARGO_PKG_VERSION"),
                    project,
                    &mcp_sidebar,
                    None,
                    None,
                    context_window,
                    active.send_session_id,
                    session
                        .as_ref()
                        .and_then(|s| session_cache_line(database, &s.id)),
                    None,
                    None,
                );
            }
            // Sync Plan Mode with session changes from /new /resume /delete.
            let new_session_id = session.as_ref().map(|s| s.id.clone());
            if prev_session_id != new_session_id {
                if let Some(id) = new_session_id {
                    plan_mode = database
                        .get_plan(&id)
                        .ok()
                        .flatten()
                        .and_then(|(json, status)| {
                            let status = match status.as_str() {
                                "pending" => PlanStatus::Pending,
                                "approved" => PlanStatus::Approved,
                                _ => return None,
                            };
                            Some(PlanModeState {
                                status,
                                plan_json: Some(json),
                            })
                        });
                    if let Some(state) = plan_mode.as_ref()
                        && state.status == PlanStatus::Pending
                        && let Some(json) = state.plan_json.as_deref()
                        && let Some(rendered) = tools::render_plan(json)
                    {
                        chat_ui.notice(&format!("Plan Mode — pending plan\n{rendered}"))?;
                    }
                    mode = if plan_mode
                        .as_ref()
                        .is_some_and(|state| state.status == PlanStatus::Pending)
                    {
                        Mode::Plan
                    } else {
                        Mode::Build
                    };
                    auto_approve = false;
                    update_sidebar(
                        &mut chat_ui,
                        session.as_ref(),
                        &model_label(&active),
                        mode.label(),
                        env!("CARGO_PKG_VERSION"),
                        project,
                        &mcp_sidebar,
                        None,
                        None,
                        context_window,
                        active.send_session_id,
                        session
                            .as_ref()
                            .and_then(|s| session_cache_line(database, &s.id)),
                        None,
                        None,
                    );
                } else {
                    plan_mode = None;
                    plan_requested = false;
                }
            }
            // Compaction state is tied to the current history; reset it if a command replaced it.
            if messages.len() != messages_before {
                summary = None;
                summarized_upto = 0;
                // A different conversation gets a different prefix by design, so the first turn
                // after it is a warm-up, not drift worth reporting.
                prefix_guard.reset();
            }
            // The fixed tool array + frozen head are session-scoped: a new/resumed session
            // may carry a different plan state or profile, so recompute on the next turn.
            if prev_session_id != session.as_ref().map(|s| s.id.clone()) {
                session_tools = None;
                head_messages = None;
                memory_dirty = true;
            }
            continue;
        }

        let user_message = Message::user(input);
        let expanded = match project.expand_file_references(input) {
            Ok(expanded) => expanded,
            Err(error) => {
                if chat_ui.is_fullscreen() {
                    chat_ui.error(&format!("Could not attach file: {error:#}"))?;
                } else {
                    eprintln!(
                        "{}",
                        ui.style(
                            &format!("\nCould not attach file: {error:#}\n"),
                            &[Style::Red]
                        )
                    );
                }
                continue;
            }
        };

        let model = active.model.clone();
        // Plan Mode: auto-enter for ≥3-step tasks (heuristic on prompt) or manual /plan.
        // Also auto-enter when model first calls update_plan with ≥3 steps (prompt heuristic
        // is not the only signal — Q1=B covers both).
        let should_enter_plan =
            plan_requested || (plan_mode.is_none() && looks_like_multi_step(input));
        if should_enter_plan && active.tools {
            mode = Mode::Plan;
            auto_approve = false;
            plan_mode = Some(PlanModeState {
                status: PlanStatus::Pending,
                plan_json: None,
            });
            plan_requested = false;
            if let Some(session) = session.as_ref() {
                let _ = database.set_plan(&session.id, "{}", "pending");
            }
            chat_ui.notice("Plan Mode — only read-only tools + update_plan until approved")?;
            update_sidebar(
                &mut chat_ui,
                session.as_ref(),
                &model_label(&active),
                mode.label(),
                env!("CARGO_PKG_VERSION"),
                project,
                &mcp_sidebar,
                None,
                None,
                context_window,
                active.send_session_id,
                session
                    .as_ref()
                    .and_then(|s| session_cache_line(database, &s.id)),
                None,
                None,
            );
        } else if plan_requested {
            plan_requested = false;
        }
        // Some models/endpoints reject the `tools` field; a profile can opt out so plain chat works.
        // Fixed per Session (prefix-cache stability): computed once, reused every turn.
        // Pending Plan Mode does NOT shrink the roster — mutating calls are held at execution
        // time with an explanatory message (`is_mutating_held`), so the wire `tools` array
        // never flips shape mid-session.
        if session_tools.is_none() {
            session_tools = Some(if active.tools {
                let mut defs = tools.definitions();
                if active.embedding_model.is_some() {
                    defs.push(tools::search_code_definition());
                }
                defs
            } else {
                Vec::new()
            });
        }
        let tool_definitions = session_tools.as_ref().expect("computed above").clone();

        // Auto-compact older history once the recent portion grows past the threshold.
        summarized_upto = summarized_upto.min(messages.len());
        if compaction::total_bytes(&messages[summarized_upto..])
            > compaction::threshold(active.context_window, active.send_session_id)
        {
            let outcome = tokio::select! {
                result = run_compaction(
                    provider.as_ref(),
                    &active.model,
                    &messages,
                    summary.as_deref(),
                    summarized_upto,
                    cache::side_session_id(
                        coding_session_id.as_deref(),
                        cache::SideRequest::Compact,
                    ),
                ) => result,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl+C")?;
                    chat_ui.notice("interrupted — back to prompt")?;
                    continue 'chat;
                }
            };
            match outcome {
                Ok(Some((new_summary, new_upto, count))) => {
                    summary = Some(new_summary);
                    summarized_upto = new_upto;
                    chat_ui.notice(&format!(
                        "Compacted {count} earlier messages into a running summary.{}",
                        cache_reset_note(active.send_session_id)
                    ))?;
                    prefix_guard.reset();
                }
                Ok(None) => {}
                Err(error) => {
                    if chat_ui.is_fullscreen() {
                        chat_ui.error(&format!("Could not compact history: {error:#}"))?;
                    } else {
                        eprintln!(
                            "{}",
                            ui.style(
                                &format!("(could not compact history: {error:#})\n"),
                                &[Style::Red]
                            )
                        );
                    }
                }
            }
        }

        // Working conversation for this turn (prompt-cache contract, see `src/cache.rs`):
        //
        //     [head]     base system + project instructions, then the eager skill list
        //     [history]  the un-summarized conversation, unchanged
        //     [tail]     memory snapshot + rolling summary
        //     [user]     the new turn
        //
        // The head is computed once per Session and cloned per Turn: its inputs are fixed at
        // startup or change only on an explicit user action (`/model`, a skill toggle), which
        // resets `head_messages`.
        //
        // Memory and the summary sit *behind* the history rather than in the head. Both are read
        // fresh -- a fact remembered this turn is visible on the next one -- so putting them in
        // front would mean one `remember` call moved byte zero and re-read the whole conversation
        // at full price. Behind it, the divergence point is only ever the previous turn's tail.
        // Intermediate tool messages live here only; they are not persisted.
        if head_messages.is_none() {
            let skills_eager = skill_library.eager_block_filtered(&disabled_skills);
            let base = prompt::build(active.tools, project.system_message().as_deref(), None);
            head_messages = Some(build_head_messages(base, skills_eager.as_deref()));
        }
        let head = head_messages.as_ref().expect("built above");
        // The tail re-reads the database only after a memory tool ran; between those turns the
        // previous snapshot is reused, which saves a round trip per turn rather than protecting
        // the prefix -- the tail is free to change without costing a cache hit.
        if memory_dirty {
            cached_memory_snapshot = render_memory_snapshot(&database.list_memory()?);
            memory_dirty = false;
        }
        // Guard the frozen part. Nothing above should ever move without an explicit user action;
        // when it does, say so, because otherwise a collapsed hit rate has no visible cause.
        if let Some(notice) = prefix_guard.check(&cache::head_text(head), &tool_definitions) {
            chat_ui.notice(&notice)?;
        }
        // Say what was attached. The prompt text only ever shows what was typed (`@clipboard`,
        // `@shot.png`), so without this there is nothing to tell you whether an image was
        // actually picked up, how big it was, or that it needs a vision model to be read.
        if let Some(note) = describe_attachments(&expanded) {
            chat_ui.notice(&note)?;
        }
        let mut turn_messages = cache::turn_messages(
            head,
            &messages[summarized_upto..],
            cache::volatile_tail(&cached_memory_snapshot, summary.as_deref()),
            Message::user_with_images(expanded.text, expanded.images),
        );

        // Agent loop: stream a turn, run any tools it requests, and repeat until a plain answer.
        // `tool_trail` collects this turn's intermediate tool-request and tool-result messages so
        // they can be persisted alongside the prompt and final answer.
        let mut final_usage = Usage::default();
        let mut final_finish = String::new();
        let mut last_content = String::new();
        let mut tool_trail: Vec<Message> = Vec::new();
        let mut total_tool_calls: usize = 0;
        // Pre-edit snapshot of every file an approved patch_file call touches this turn, so an
        // interrupted multi-file edit can be reverted instead of left half-applied (see
        // `snapshot_patch_target`/`revert_on_cancel`).
        let mut turn_snapshot: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut round = 0usize;
        // Esc / Ctrl+C now interrupt; the editor stays live for queueing.
        let _busy = hub.as_ref().map(|h| h.busy_guard());
        let assistant_message = 'agent: loop {
            round += 1;
            // Steering. Anything typed while the turn runs is folded into it here, between two
            // rounds, so a correction reaches the model while it can still act on it instead of
            // waiting for the whole turn to finish. Round 1 is skipped: nothing has run yet, so
            // there is nothing to steer, and the prompt itself is already in `turn_messages`.
            if let Some(hub) = hub.as_ref().filter(|_| round > 1) {
                // Drain fully first: a slash command is a UI action, not something to say to
                // the model, so it goes back on the queue and runs once the turn is over.
                let mut queued = Vec::new();
                while let Some(line) = hub.pop_queue() {
                    queued.push(line);
                }
                for line in queued {
                    if plan_mode
                        .as_ref()
                        .is_some_and(|state| state.status == PlanStatus::Pending)
                        && is_plan_approval(&line)
                    {
                        if let Some(state) = plan_mode.as_mut() {
                            state.status = PlanStatus::Approved;
                            if let Some(session) = session.as_ref()
                                && let Some(json) = state.plan_json.as_deref()
                            {
                                let _ = database.set_plan(&session.id, json, "approved");
                            }
                        }
                        mode = Mode::Build;
                        auto_approve = false;
                        chat_ui.notice("plan approved — gate open for this session")?;
                        update_sidebar(
                            &mut chat_ui,
                            session.as_ref(),
                            &model_label(&active),
                            mode.label(),
                            env!("CARGO_PKG_VERSION"),
                            project,
                            &mcp_sidebar,
                            None,
                            None,
                            context_window,
                            active.send_session_id,
                            session
                                .as_ref()
                                .and_then(|s| session_cache_line(database, &s.id)),
                            None,
                            None,
                        );
                        continue;
                    }
                    if line.trim_start().starts_with('/') {
                        hub.push_prompt(line);
                        continue;
                    }
                    chat_ui.user_steering(&line)?;
                    let message = Message::user(&line);
                    turn_messages.push(message.clone());
                    tool_trail.push(message);
                }
            }
            if round > MAX_TOOL_ROUNDS {
                chat_ui.notice(&format!(
                    "Stopped after {MAX_TOOL_ROUNDS} tool rounds without a final answer."
                ))?;
                break 'agent Message::assistant(if last_content.is_empty() {
                    "(stopped: reached the tool-call round limit)".to_string()
                } else {
                    last_content.clone()
                });
            }

            let started = Instant::now();
            let request = provider.chat_stream(ChatRequest {
                model: model.clone(),
                messages: turn_messages.clone(),
                tools: tool_definitions.clone(),
                session_id: coding_session_id.clone(),
            });
            if !chat_ui.is_fullscreen() {
                println!();
            }
            // Animate a spinner from the moment the request is sent until the first token (or a
            // terminal event) arrives, so the wait for the model does not look frozen.
            let mut spinner = start_spinner("Thinking...", ui, &mut chat_ui);
            let mut stream = tokio::select! {
                response = request => match response {
                    Ok(stream) => stream,
                    Err(error) => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        chat_ui.error(&format!("Request failed: {error:#}"))?;
                        revert_on_cancel(&mut chat_ui, &turn_snapshot);
                        continue 'chat;
                    }
                },
                signal = tokio::signal::ctrl_c() => {
                    stop_spinner(&mut spinner, &mut chat_ui).await;
                    signal.context("failed to listen for Ctrl+C")?;
                    revert_on_cancel(&mut chat_ui, &turn_snapshot);
                    chat_ui.notice("interrupted — back to prompt")?;
                    continue 'chat;
                },
                () = wait_interrupt(&interrupt) => {
                    stop_spinner(&mut spinner, &mut chat_ui).await;
                    revert_on_cancel(&mut chat_ui, &turn_snapshot);
                    chat_ui.notice("interrupted — back to prompt")?;
                    continue 'chat;
                }
            };

            let mut content = String::new();
            let mut ttft: Option<Duration> = None;
            // Styles the streamed text a line at a time. `content` keeps the raw markdown, since
            // that is what gets persisted and re-sent to the model.
            let mut renderer = markdown::Renderer::for_stdout();
            let (usage, finish_reason, tool_calls) = loop {
                let event = tokio::select! {
                    event = stream.recv() => event,
                    signal = tokio::signal::ctrl_c() => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        signal.context("failed to listen for Ctrl+C")?;
                        // Show the partial line still held by the line buffer before leaving.
                        if chat_ui.is_fullscreen() {
                            chat_ui.assistant_update(&content)?;
                        } else {
                            print!("{}", renderer.finish());
                        }
                        revert_on_cancel(&mut chat_ui, &turn_snapshot);
                        chat_ui.notice("interrupted — back to prompt")?;
                        continue 'chat;
                    }
                    () = wait_interrupt(&interrupt) => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        if chat_ui.is_fullscreen() {
                            chat_ui.assistant_update(&content)?;
                        } else {
                            print!("{}", renderer.finish());
                        }
                        revert_on_cancel(&mut chat_ui, &turn_snapshot);
                        chat_ui.notice("interrupted — back to prompt")?;
                        continue 'chat;
                    }
                };
                match event {
                    Some(Ok(StreamEvent::Delta(delta))) => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        if ttft.is_none() {
                            ttft = Some(started.elapsed());
                        }
                        let rendered = renderer.push(&delta);
                        if chat_ui.is_fullscreen() {
                            content.push_str(&delta);
                            chat_ui.assistant_update(&content)?;
                        } else {
                            print!("{rendered}");
                            io::stdout().flush()?;
                            content.push_str(&delta);
                        }
                    }
                    Some(Ok(StreamEvent::Done {
                        usage,
                        finish_reason,
                        tool_calls,
                    })) => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        if chat_ui.is_fullscreen() {
                            chat_ui.assistant_update(&content)?;
                            chat_ui.assistant_done()?;
                        } else {
                            print!("{}", renderer.finish());
                            println!();
                        }
                        break (usage, finish_reason, tool_calls);
                    }
                    Some(Err(error)) => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        if chat_ui.is_fullscreen() {
                            chat_ui.error(&format!("Request failed: {error:#}"))?;
                        } else {
                            print!("{}", renderer.finish());
                            eprintln!(
                                "\n{}",
                                render::render_error(&format!("Request failed: {error:#}"), ui)
                            );
                        }
                        revert_on_cancel(&mut chat_ui, &turn_snapshot);
                        continue 'chat;
                    }
                    None => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        if chat_ui.is_fullscreen() {
                            chat_ui.error("Request failed: provider stream closed unexpectedly")?;
                        } else {
                            print!("{}", renderer.finish());
                            eprintln!(
                                "\n{}",
                                render::render_error(
                                    "Request failed: provider stream closed unexpectedly",
                                    ui
                                )
                            );
                        }
                        revert_on_cancel(&mut chat_ui, &turn_snapshot);
                        continue 'chat;
                    }
                }
            };
            let miss_label = cache_miss_label(
                prev_cached,
                usage.cached_tokens,
                usage.prompt_tokens,
                prev_model.as_deref() != Some(active.model.as_str()),
                head_rebuilt_this_turn,
            );
            let usage_line = format_usage(
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens,
                usage.cached_tokens,
                active.send_session_id,
                &finish_reason,
                ttft,
                started.elapsed(),
                context_window,
                miss_label,
            );
            if chat_ui.is_fullscreen() {
                // Structured per-metric lines read better in the narrow rail. Tab separates
                // the fixed-width label from the value so the sidebar can style them apart.
                let mut lt = format!("tok\t{}", usage.total_tokens);
                lt.push_str(&format!("\nin\t{}", usage.prompt_tokens));
                if usage.cached_tokens > 0 && usage.prompt_tokens > 0 {
                    let hit = (usage.cached_tokens as f64 / usage.prompt_tokens as f64 * 100.0)
                        .min(100.0);
                    lt.push_str(&format!("\ncache\t{} ({hit:.0}%)", usage.cached_tokens));
                } else if let Some(miss) = miss_label {
                    lt.push_str(&format!("\ncache\t{miss}"));
                } else if active.send_session_id {
                    lt.push_str("\ncache\t0 (warm-up)");
                }
                if let Some(t) = ttft {
                    lt.push_str(&format!("\nlat\t{}", crate::terminal::format_duration(t)));
                }
                lt.push_str(&format!(
                    "\ntime\t{}",
                    crate::terminal::format_duration(started.elapsed())
                ));
                let activity = {
                    let mut a = format!("tools\t{}", total_tool_calls);
                    if let Some(t) = ttft {
                        a.push_str(&format!("\nlat\t{}", crate::terminal::format_duration(t)));
                    }
                    a.push_str(&format!(
                        "\ntime\t{}",
                        crate::terminal::format_duration(started.elapsed())
                    ));
                    a
                };
                update_sidebar(
                    &mut chat_ui,
                    session.as_ref(),
                    &model_label(&active),
                    mode.label(),
                    env!("CARGO_PKG_VERSION"),
                    project,
                    &mcp_sidebar,
                    Some(usage.prompt_tokens),
                    Some(usage.cached_tokens),
                    context_window,
                    active.send_session_id,
                    session
                        .as_ref()
                        .and_then(|s| session_cache_line(database, &s.id)),
                    Some(lt),
                    Some(activity),
                );
            } else {
                println!("{usage_line}");
                update_sidebar(
                    &mut chat_ui,
                    session.as_ref(),
                    &model_label(&active),
                    mode.label(),
                    env!("CARGO_PKG_VERSION"),
                    project,
                    &mcp_sidebar,
                    Some(usage.prompt_tokens),
                    Some(usage.cached_tokens),
                    context_window,
                    active.send_session_id,
                    session
                        .as_ref()
                        .and_then(|s| session_cache_line(database, &s.id)),
                    None,
                    None,
                );
            }
            total_tool_calls += tool_calls.len();
            accumulate_usage(&mut final_usage, &usage);
            final_finish = finish_reason;
            last_content = content.clone();
            // Feed miss detection for the next round: the last round's counts are what
            // the next request's prefix is measured against. Reset the rebuild flag —
            // it only names the turn where the rebuild happened.
            prev_cached = Some(usage.cached_tokens);
            prev_model = Some(active.model.clone());
            head_rebuilt_this_turn = false;

            if tool_calls.is_empty() {
                break 'agent Message::assistant(content);
            }

            // The model requested tools. Record the request, run each tool, feed the results back.
            let request_message = Message::tool_request(content, tool_calls.clone());
            turn_messages.push(request_message.clone());
            tool_trail.push(request_message);
            let spawn_calls: Vec<&ToolCall> = tool_calls
                .iter()
                .filter(|call| call.name == tools::SPAWN_AGENT_TOOL)
                .collect();
            let spawned_outputs = if spawn_calls.is_empty() {
                HashMap::new()
            } else {
                chat_ui.notice(&format!(
                    "running {} sub-agent(s), up to {MAX_CONCURRENT_SUB_AGENTS} concurrently",
                    spawn_calls.len()
                ))?;
                tokio::select! {
                    output = dispatch_spawn_agents(
                        provider.as_ref(),
                        &active.model,
                        project,
                        &spawn_calls,
                        coding_session_id.clone(),
                    ) => output,
                    signal = tokio::signal::ctrl_c() => {
                        signal.context("failed to listen for Ctrl+C")?;
                        revert_on_cancel(&mut chat_ui, &turn_snapshot);
                        chat_ui.notice("interrupted — back to prompt")?;
                        continue 'chat;
                    }
                }
            };
            // Plan Mode: auto-enter on first update_plan with ≥3 steps (Q1=B).
            if plan_mode.is_none() {
                for call in &tool_calls {
                    if call.name == tools::UPDATE_PLAN_TOOL
                        && let Some(count) = tools::plan_step_count(&call.arguments)
                        && count >= 3
                    {
                        plan_mode = Some(PlanModeState {
                            status: PlanStatus::Pending,
                            plan_json: None,
                        });
                        mode = Mode::Plan;
                        auto_approve = false;
                        if let Some(session) = session.as_ref() {
                            let _ = database.set_plan(&session.id, "{}", "pending");
                        }
                        chat_ui.notice(
                            "Plan Mode — only read-only tools + update_plan until approved",
                        )?;
                        update_sidebar(
                            &mut chat_ui,
                            session.as_ref(),
                            &model_label(&active),
                            mode.label(),
                            env!("CARGO_PKG_VERSION"),
                            project,
                            &mcp_sidebar,
                            None,
                            None,
                            context_window,
                            active.send_session_id,
                            session
                                .as_ref()
                                .and_then(|s| session_cache_line(database, &s.id)),
                            None,
                            None,
                        );
                        break;
                    }
                }
            }
            let mut pending_visuals = Vec::new();
            for call in &tool_calls {
                let tool_started = Instant::now();
                if call.name == tools::UPDATE_PLAN_TOOL
                    && let Some(rendered) = tools::render_plan(&call.arguments)
                {
                    chat_ui.tool_call(&call.name, &call.arguments)?;
                    if chat_ui.is_fullscreen() {
                        chat_ui.notice(&rendered)?;
                    } else {
                        println!("{rendered}");
                    }
                    let _ = chat_ui.set_plan(tools::plan_view(&call.arguments));
                    // Persist plan: pending stays pending, approved stays tracker.
                    if let Some(state) = plan_mode.as_mut() {
                        state.plan_json = Some(call.arguments.clone());
                        let status = match state.status {
                            PlanStatus::Pending => "pending",
                            PlanStatus::Approved => "approved",
                        };
                        if let Some(session) = session.as_ref() {
                            let _ = database.set_plan(&session.id, &call.arguments, status);
                        }
                    }
                    // Inline approval prompt when a plan is pending and this is the plan call.
                    if plan_mode
                        .as_ref()
                        .is_some_and(|s| s.status == PlanStatus::Pending)
                    {
                        let plan_title = "Approve plan?";
                        let plan_body = tools::render_plan(
                            plan_mode
                                .as_ref()
                                .and_then(|state| state.plan_json.as_deref())
                                .unwrap_or(""),
                        )
                        .unwrap_or_default();
                        let answer = tokio::select! {
                             answer = read_plan_approval_line(&mut input_rx, use_tui, hub.as_mut(), plan_title, plan_body) => answer,
                        () = wait_interrupt(&interrupt) => None,
                            signal = tokio::signal::ctrl_c() => {
                                signal.context("failed to listen for Ctrl+C")?;
                                revert_on_cancel(&mut chat_ui, &turn_snapshot);
                                chat_ui.notice("interrupted — back to prompt")?;
                                continue 'chat;
                            }
                        };
                        let approved = answer.as_deref().is_some_and(is_plan_approval);
                        if approved {
                            if let Some(state) = plan_mode.as_mut() {
                                state.status = PlanStatus::Approved;
                                if let Some(session) = session.as_ref()
                                    && let Some(json) = state.plan_json.clone()
                                {
                                    let _ = database.set_plan(&session.id, &json, "approved");
                                }
                            }
                            chat_ui.notice("plan approved — gate open for this session")?;
                            mode = Mode::Build;
                            auto_approve = false;
                            update_sidebar(
                                &mut chat_ui,
                                session.as_ref(),
                                &model_label(&active),
                                mode.label(),
                                env!("CARGO_PKG_VERSION"),
                                project,
                                &mcp_sidebar,
                                None,
                                None,
                                context_window,
                                active.send_session_id,
                                session
                                    .as_ref()
                                    .and_then(|s| session_cache_line(database, &s.id)),
                                None,
                                None,
                            );
                        } else {
                            chat_ui.notice(
                                "plan not approved — still in Plan Mode; propose a revised plan",
                            )?;
                        }
                    }
                } else {
                    chat_ui.tool_call(&call.name, &call.arguments)?;
                }
                // In pending Plan Mode, hold mutating tools.
                let is_mutating_held = plan_mode
                    .as_ref()
                    .is_some_and(|s| s.status == PlanStatus::Pending)
                    && is_mutating_tool(&call.name);
                let output = if is_mutating_held {
                    "Plan Mode is active — propose a plan with update_plan and wait for approval before mutating tools.".to_string()
                } else if call.name == tools::ASK_USER_TOOL {
                    tokio::select! {
                        output = ask_user(&mut input_rx, use_tui, &call.arguments, hub.as_mut()) => output?,
                        signal = tokio::signal::ctrl_c() => {
                            signal.context("failed to listen for Ctrl+C")?;
                            revert_on_cancel(&mut chat_ui, &turn_snapshot);
                            chat_ui.notice("interrupted — back to prompt")?;
                            continue 'chat;
                        }
                        () = wait_interrupt(&interrupt) => {
                            revert_on_cancel(&mut chat_ui, &turn_snapshot);
                            chat_ui.notice("interrupted — back to prompt")?;
                            continue 'chat;
                        }
                    }
                } else if call.name == tools::SPAWN_AGENT_TOOL {
                    spawned_outputs
                        .get(&call.id)
                        .map(|(output, _)| output.clone())
                        .unwrap_or_else(|| "Error: sub-agent result was missing".to_string())
                } else if call.name == tools::SEARCH_CODE_TOOL {
                    match active.embedding_model.as_deref() {
                        Some(embedding_model) => {
                            tokio::select! {
                                output = dispatch_search_code(
                                    provider.as_ref(),
                                    embedding_model,
                                    database,
                                    project,
                                    &call.arguments,
                                ) => output,
                                signal = tokio::signal::ctrl_c() => {
                                    signal.context("failed to listen for Ctrl+C")?;
                                    revert_on_cancel(&mut chat_ui, &turn_snapshot);
                                    chat_ui.notice("interrupted — back to prompt")?;
                                    continue 'chat;
                                }
                            }
                        }
                        None => "Error: this profile has no embedding_model configured; \
                                 search_code is unavailable"
                            .to_string(),
                    }
                } else if is_memory_tool(&call.name) {
                    // Memory changed: re-read it for the next turn's tail, so the new fact is
                    // visible immediately (same guarantee as the old read-fresh-every-turn).
                    // The head is untouched — memory does not live there, which is exactly why a
                    // `remember` call no longer costs the session its cached prefix.
                    memory_dirty = true;
                    dispatch_memory_tool(database, &call.name, &call.arguments)
                } else if tools.requires_confirmation_for(&call.name, &call.arguments)
                    && !auto_approve
                    && !always_allowed.contains(&call.name)
                {
                    let preview = tools.preview(call);
                    if !use_tui {
                        if let Some(preview) = &preview {
                            chat_ui.notice(preview)?;
                        }
                        chat_ui.notice("approve? [y/N/a]")?;
                    }
                    let modal_title = format!("Allow {}?", call.name);
                    let modal_body =
                        preview.unwrap_or_else(|| format!("{} {}", call.name, call.arguments));
                    let answer = tokio::select! {
                        answer = read_approval_line(&mut input_rx, use_tui, hub.as_mut(), &modal_title, modal_body) => answer,
                        () = wait_interrupt(&interrupt) => None,
                        signal = tokio::signal::ctrl_c() => {
                            signal.context("failed to listen for Ctrl+C")?;
                            revert_on_cancel(&mut chat_ui, &turn_snapshot);
                            chat_ui.notice("interrupted — back to prompt")?;
                            continue 'chat;
                        }
                    };
                    let trimmed = answer.as_deref().map(str::trim);
                    let always = matches!(trimmed, Some("a" | "A" | "always" | "Always"));
                    let approved = always || matches!(trimmed, Some("y" | "Y" | "yes" | "Yes"));
                    if always {
                        always_allowed.insert(call.name.clone());
                        chat_ui.notice(&format!(
                            "always allowing {} for the rest of this session — /new clears this",
                            call.name
                        ))?;
                    }
                    if approved {
                        if call.name == tools::PATCH_FILE_TOOL {
                            snapshot_patch_target(
                                project.root(),
                                &call.arguments,
                                &mut turn_snapshot,
                            );
                        }
                        tokio::select! {
                            output = tools.dispatch(call) => output,
                            signal = tokio::signal::ctrl_c() => {
                                signal.context("failed to listen for Ctrl+C")?;
                                revert_on_cancel(&mut chat_ui, &turn_snapshot);
                                chat_ui.notice("interrupted — back to prompt")?;
                                continue 'chat;
                            }
                        }
                    } else {
                        chat_ui.notice("skipped")?;
                        "The user declined to run this command.".to_string()
                    }
                } else {
                    // Reached because the tool never needs confirmation, --auto-approve overrode
                    // one that normally would, or it was granted a standing "always allow" this
                    // session; either way, still snapshot a patch so it can be reverted like an
                    // approved one.
                    if call.name == tools::PATCH_FILE_TOOL {
                        snapshot_patch_target(project.root(), &call.arguments, &mut turn_snapshot);
                    }
                    tokio::select! {
                        output = tools.dispatch(call) => output,
                        signal = tokio::signal::ctrl_c() => {
                            signal.context("failed to listen for Ctrl+C")?;
                            revert_on_cancel(&mut chat_ui, &turn_snapshot);
                            chat_ui.notice("interrupted — back to prompt")?;
                            continue 'chat;
                        }
                    }
                };
                let elapsed = spawned_outputs
                    .get(&call.id)
                    .map(|(_, elapsed)| *elapsed)
                    .unwrap_or_else(|| tool_started.elapsed());
                let (outcome, ok) = crate::terminal::tool_outcome_parts(&output, elapsed);
                let body = if ok {
                    preview_output(&output)
                } else {
                    tool_body(&output).to_string()
                };
                chat_ui.tool_finished(&outcome, ok, &body)?;
                let result_message = Message::tool_result(&call.id, output.clone());
                turn_messages.push(result_message.clone());
                tool_trail.push(result_message);
                // Defer vision attachments until every tool result in this batch is
                // on the wire. A `user` message mid-batch breaks OpenAI's rule that
                // every tool_call_id must be answered contiguously after the
                // assistant tool_calls message (Orvix returns 400 otherwise).
                if call.name == "read_image" && !output.starts_with("Error: ") {
                    pending_visuals.push(match tools.read_image(&call.arguments) {
                        Ok((metadata, image)) => Message::user_with_images(
                            format!(
                                "Visual input returned by read_image tool call {}:\n{metadata}",
                                call.id
                            ),
                            vec![image],
                        ),
                        Err(error) => Message::user(format!("Image attachment failed: {error:#}")),
                    });
                }
            }
            for visual in pending_visuals {
                turn_messages.push(visual.clone());
                tool_trail.push(visual);
            }
        };

        // The turn completed normally (not cancelled): keep its file snapshot around for /undo.
        last_turn_snapshot = (!turn_snapshot.is_empty()).then_some(turn_snapshot);

        // Assemble the full turn: the original prompt, any tool trail, then the final answer.
        let final_answer = assistant_message.content.clone();
        let mut turn_record = Vec::with_capacity(tool_trail.len() + 2);
        turn_record.push(user_message);
        turn_record.append(&mut tool_trail);
        turn_record.push(assistant_message);

        let is_first_exchange = session.is_none();
        let active_session = match session.as_mut() {
            Some(session) => session,
            None => session.insert(database.create_session(provider.name(), &active.model)?),
        };
        database.save_turn(
            &active_session.id,
            &turn_record,
            &final_usage,
            &active.model,
            &final_finish,
        )?;
        // Persist plan state after save (session now exists). Approved clears pending.
        if let Some(state) = plan_mode.as_ref() {
            match state.status {
                PlanStatus::Approved => {
                    if let Some(json) = state.plan_json.as_deref() {
                        let _ = database.set_plan(&active_session.id, json, "approved");
                    }
                }
                PlanStatus::Pending => {
                    if let Some(json) = state.plan_json.as_deref() {
                        let _ = database.set_plan(&active_session.id, json, "pending");
                    }
                }
            }
        }
        if active_session.title == "New chat" {
            active_session.title = make_title(title_source);
        }
        messages.extend(turn_record);

        // A `/warnings fix` turn is only finished once the loader has been asked again: the
        // agent's own summary is a claim, a fresh load is the evidence.
        if let Some(before) = pending_skill_fix.take() {
            skill_library = crate::skills::SkillLibrary::load(project.root());
            let report = skill_fix_report(&before, skill_library.warnings());
            chat_ui.notice(&report.summary)?;
            chat_ui.set_warnings(report.banner.into_iter().collect())?;
            chat_ui.set_warning_details(
                skill_library
                    .warnings()
                    .iter()
                    .map(|warning| warning.to_string())
                    .collect(),
            )?;
        }

        // Only after the turn is safely persisted: a refresh failure must never cost the exchange.
        if let Some(snapshot) = last_turn_snapshot.as_ref() {
            let edited: Vec<PathBuf> = snapshot.keys().cloned().collect();
            let mut interrupted = false;
            let outcome = tokio::select! {
                result = refresh_index_for_paths(
                    provider.as_ref(), &active, database, project, edited,
                ) => result,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl+C")?;
                    interrupted = true;
                    Ok(0)
                }
            };
            if interrupted {
                chat_ui.notice("interrupted — the index may be stale; /index rebuilds it")?;
            } else {
                report_index_refresh(&mut chat_ui, outcome);
            }
        }

        if is_first_exchange {
            let title_request = provider.chat(ChatRequest {
                model: active.model.clone(),
                messages: vec![
                    Message::system(
                        "Create a concise title of at most 6 words for this conversation. Return only the title without quotes or punctuation.",
                    ),
                    Message::user(title_source),
                    Message::assistant(final_answer),
                ],
                tools: Vec::new(),
                // Sibling sticky id: title must not evict the conversation's warm prefix.
                session_id: cache::side_session_id(
                    coding_session_id.as_deref(),
                    cache::SideRequest::Title,
                ),
            });
            let title_response = tokio::select! {
                response = title_request => response,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl+C")?;
                    println!();
                    shutdown(
                        &mut chat_ui,
                        database,
                        session.as_ref(),
                        context_window,
                        &job_registry,
                        &config.prices,
                    )?;
                    break;
                }
            };
            match title_response {
                Ok(response) => {
                    let title = clean_title(&response.content);
                    if !title.is_empty() {
                        let session = session.as_mut().expect("session was just persisted");
                        database.save_generated_title(
                            &session.id,
                            &title,
                            &response.usage,
                            &active.model,
                            &response.finish_reason,
                        )?;
                        session.title = title;
                    }
                }
                Err(error) => eprintln!(
                    "{}",
                    ui.style(
                        &format!("Could not generate session title: {error:#}\n"),
                        &[Style::Red]
                    )
                ),
            }
        }
    }

    Ok(())
}

/// Run a single prompt non-interactively and exit: no REPL loop, no stdin reader, no spinner.
/// Reuses the same profile selection, tool registry, agent loop, and persistence as interactive
/// chat, so a `-p` session can later be resumed with `-r` like any other.
pub async fn run_once<F>(
    config: Config,
    tools: ToolRegistry,
    database: &Database,
    project: &ProjectContext,
    prompt: &str,
    auto_approve: bool,
    build_provider: F,
) -> Result<()>
where
    F: Fn(&Profile) -> Box<dyn Provider>,
{
    let ui = Ui::stdio();
    let active_name = database
        .get_setting(ACTIVE_PROFILE_KEY)?
        .filter(|name| config.find(name).is_some())
        .unwrap_or_else(|| config.default_profile.clone());
    let active = config
        .find(&active_name)
        .cloned()
        .unwrap_or_else(|| config.default().clone());
    let provider = build_provider(&active);
    let session = if active.send_session_id {
        Some(database.create_session(provider.name(), &active.model)?)
    } else {
        None
    };
    let coding_session_id = session.as_ref().map(|s| s.id.clone());

    // `kamui -p /review` or `/skill:my-skill` expands the same way interactive chat does.
    let command_library = commands::CommandLibrary::load(project.root());
    let skill_library = crate::skills::SkillLibrary::load(project.root());
    for warning in skill_library.warnings() {
        eprintln!("warning: {warning}");
    }
    let disabled_skills = crate::settings::load_disabled_skills(project.root());
    let expanded_command = command_library.expand(prompt);
    let expanded_skill = if expanded_command.is_none() {
        skill_library.expand_filtered(prompt, &disabled_skills)
    } else {
        None
    };
    let expanded = expanded_command.as_deref().or(expanded_skill.as_deref());
    let title_source = prompt;
    let prompt: &str = expanded.unwrap_or(prompt);

    let expanded = project
        .expand_file_references(prompt)
        .context("could not attach file")?;
    let mut tool_definitions = if active.tools {
        tools.definitions()
    } else {
        Vec::new()
    };
    if active.tools && active.embedding_model.is_some() {
        tool_definitions.push(tools::search_code_definition());
    }

    let skills_eager = skill_library.eager_block_filtered(&disabled_skills);
    // Same shape as an interactive turn (see `src/cache.rs`): frozen head first, volatile memory
    // tail last, so a session driven through `-p` builds the prefix an interactive turn of the
    // same session would.
    let base = prompt::build(active.tools, project.system_message().as_deref(), None);
    let head = build_head_messages(base, skills_eager.as_deref());
    if let Some(note) = describe_attachments(&expanded) {
        println!("{note}");
    }
    let memory_snapshot = render_memory_snapshot(&database.list_memory()?);
    let mut turn_messages = cache::turn_messages(
        &head,
        &[],
        cache::volatile_tail(&memory_snapshot, None),
        Message::user_with_images(expanded.text, expanded.images),
    );

    let user_message = Message::user(prompt);
    let mut tool_trail: Vec<Message> = Vec::new();
    // Files `patch_file` targeted this turn, so the code index can be refreshed once at the end.
    // Interactive chat reads the same set out of its revert snapshot, which `-p` has no use for.
    let mut edited: Vec<PathBuf> = Vec::new();
    let mut round = 0usize;
    let (assistant_message, final_usage, final_finish) = loop {
        round += 1;
        if round > MAX_TOOL_ROUNDS {
            anyhow::bail!("stopped after {MAX_TOOL_ROUNDS} tool rounds without a final answer");
        }

        let response = provider
            .chat(ChatRequest {
                model: active.model.clone(),
                messages: turn_messages.clone(),
                tools: tool_definitions.clone(),
                session_id: coding_session_id.clone(),
            })
            .await
            .context("request failed")?;

        if response.tool_calls.is_empty() {
            break (
                Message::assistant(response.content),
                response.usage,
                response.finish_reason,
            );
        }

        let request_message = Message::tool_request(response.content, response.tool_calls.clone());
        turn_messages.push(request_message.clone());
        tool_trail.push(request_message);
        let spawn_calls: Vec<&ToolCall> = response
            .tool_calls
            .iter()
            .filter(|call| call.name == tools::SPAWN_AGENT_TOOL)
            .collect();
        let spawned_outputs = dispatch_spawn_agents(
            provider.as_ref(),
            &active.model,
            project,
            &spawn_calls,
            coding_session_id.clone(),
        )
        .await;
        let mut pending_visuals = Vec::new();
        for call in &response.tool_calls {
            let tool_started = Instant::now();
            if call.name == tools::UPDATE_PLAN_TOOL
                && let Some(rendered) = tools::render_plan(&call.arguments)
            {
                print!(
                    "{}",
                    render::render_tool_call(&call.name, &call.arguments, ui)
                );
                println!("{rendered}");
            } else {
                print!(
                    "{}",
                    render::render_tool_call(&call.name, &call.arguments, ui)
                );
            }
            let output = if call.name == tools::ASK_USER_TOOL {
                println!("    skipped: ask_user is not available in non-interactive mode");
                "There is no user to ask in non-interactive mode. Proceed using your best \
                 judgment, or state your assumption in the final answer."
                    .to_string()
            } else if call.name == tools::SPAWN_AGENT_TOOL {
                spawned_outputs
                    .get(&call.id)
                    .map(|(output, _)| output.clone())
                    .unwrap_or_else(|| "Error: sub-agent result was missing".to_string())
            } else if call.name == tools::SEARCH_CODE_TOOL {
                match active.embedding_model.as_deref() {
                    Some(embedding_model) => {
                        dispatch_search_code(
                            provider.as_ref(),
                            embedding_model,
                            database,
                            project,
                            &call.arguments,
                        )
                        .await
                    }
                    None => "Error: this profile has no embedding_model configured; search_code \
                             is unavailable"
                        .to_string(),
                }
            } else if is_memory_tool(&call.name) {
                dispatch_memory_tool(database, &call.name, &call.arguments)
            } else if tools.requires_confirmation_for(&call.name, &call.arguments) && !auto_approve
            {
                println!("    denied: non-interactive mode (pass --auto-approve to allow)");
                "The user declined to run this command (non-interactive mode).".to_string()
            } else {
                if call.name == tools::PATCH_FILE_TOOL
                    && let Some(target) = tools::patch_target(project.root(), &call.arguments)
                    && !edited.contains(&target)
                {
                    edited.push(target);
                }
                tools.dispatch(call).await
            };
            let elapsed = spawned_outputs
                .get(&call.id)
                .map(|(_, elapsed)| *elapsed)
                .unwrap_or_else(|| tool_started.elapsed());
            if output.starts_with("Error: ") {
                print!("{}", render::render_error(tool_body(&output), ui));
            } else if !output.is_empty() {
                print!(
                    "{}",
                    render::render_tool_output(&preview_output(&output), ui)
                );
            }
            println!("{}", ui.tool_outcome(&output, elapsed));
            let result_message = Message::tool_result(&call.id, output.clone());
            turn_messages.push(result_message.clone());
            tool_trail.push(result_message);
            // Defer vision attachments until every tool result in this batch is
            // on the wire. A `user` message mid-batch breaks OpenAI's rule that
            // every tool_call_id must be answered contiguously after the
            // assistant tool_calls message (Orvix returns 400 otherwise).
            if call.name == "read_image" && !output.starts_with("Error: ") {
                pending_visuals.push(match tools.read_image(&call.arguments) {
                    Ok((metadata, image)) => Message::user_with_images(
                        format!(
                            "Visual input returned by read_image tool call {}:\n{metadata}",
                            call.id
                        ),
                        vec![image],
                    ),
                    Err(error) => Message::user(format!("Image attachment failed: {error:#}")),
                });
            }
        }
        for visual in pending_visuals {
            turn_messages.push(visual.clone());
            tool_trail.push(visual);
        }
    };

    println!(
        "\n{}",
        markdown::Renderer::for_stdout().render_block(&assistant_message.content)
    );

    let final_answer = assistant_message.content.clone();
    let mut turn_record = Vec::with_capacity(tool_trail.len() + 2);
    turn_record.push(user_message);
    turn_record.append(&mut tool_trail);
    turn_record.push(assistant_message);

    let mut session = match session {
        Some(existing) => existing,
        None => database.create_session(provider.name(), &active.model)?,
    };
    database.save_turn(
        &session.id,
        &turn_record,
        &final_usage,
        &active.model,
        &final_finish,
    )?;
    database.rename_session(&session.id, &make_title(title_source))?;
    session.title = make_title(title_source);

    // Only after the turn is safely persisted: a refresh failure must never cost the exchange.
    if let Some((text, failed)) = index_refresh_message(
        refresh_index_for_paths(provider.as_ref(), &active, database, project, edited).await,
    ) {
        // `-p` output is script input, so it stays on the plain streams.
        if failed {
            eprintln!("({text})");
        } else {
            println!("({text})");
        }
    }

    let title_response = provider
        .chat(ChatRequest {
            model: active.model.clone(),
            messages: vec![
                Message::system(
                    "Create a concise title of at most 6 words for this conversation. Return only the title without quotes or punctuation.",
                ),
                Message::user(title_source),
                Message::assistant(final_answer),
            ],
            tools: Vec::new(),
            session_id: cache::side_session_id(
                coding_session_id.as_deref(),
                cache::SideRequest::Title,
            ),
        })
        .await;
    match title_response {
        Ok(response) => {
            let title = clean_title(&response.content);
            if !title.is_empty() {
                database.save_generated_title(
                    &session.id,
                    &title,
                    &response.usage,
                    &active.model,
                    &response.finish_reason,
                )?;
            }
        }
        Err(error) => eprintln!(
            "{}",
            ui.style(
                &format!("Could not generate session title: {error:#}"),
                &[Style::Red]
            )
        ),
    }

    eprintln!(
        "\nTo resume this session: kamui -r {}",
        short_id(&session.id)
    );

    // Nothing outlives a single -p invocation: a background job has no way to be checked on or
    // stopped once the process exits.
    tools::kill_all_jobs(&tools.jobs());

    Ok(())
}

/// A background task that animates a single-line braille spinner until told to stop.
struct PlainSpinner {
    stop: Arc<Notify>,
    handle: JoinHandle<()>,
    width: usize,
}

/// The waiting indicator for a turn: inline on the scrollback in plain mode, a bouncing
/// wall in the fullscreen editor, or nothing when output is piped.
enum Spinner {
    None,
    Plain(PlainSpinner),
    Tui,
}

fn start_spinner(label: &'static str, ui: Ui, chat_ui: &mut ChatUi) -> Spinner {
    if !ui.interactive() {
        return Spinner::None;
    }
    if chat_ui.is_fullscreen() {
        if chat_ui.thinking_start(label).is_ok() {
            return Spinner::Tui;
        }
        return Spinner::None;
    }
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let stop = Arc::new(Notify::new());
    let stop_task = stop.clone();
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(80));
        let mut frame = 0usize;
        loop {
            tokio::select! {
                _ = stop_task.notified() => break,
                _ = interval.tick() => {
                    print!(
                        "\r{} {}",
                        FRAMES[frame % FRAMES.len()],
                        ui.style(label, &[Style::Dim])
                    );
                    let _ = io::stdout().flush();
                    frame += 1;
                }
            }
        }
    });
    Spinner::Plain(PlainSpinner {
        stop,
        handle,
        width: label.chars().count() + 2,
    })
}

impl Spinner {
    async fn finish(self, chat_ui: &mut ChatUi) {
        match self {
            Spinner::None => {}
            Spinner::Plain(spinner) => {
                spinner.stop.notify_one();
                let _ = spinner.handle.await;
                // Erase the spinner line so the response starts on a clean line.
                print!("\r{}\r", " ".repeat(spinner.width));
                let _ = io::stdout().flush();
            }
            Spinner::Tui => chat_ui.thinking_stop().await,
        }
    }
}

/// Resolves when the keyboard hub raises an interrupt; never fires in plain mode.
async fn wait_interrupt(interrupt: &Option<Arc<tokio::sync::Notify>>) {
    match interrupt {
        Some(notify) => notify.notified().await,
        None => std::future::pending().await,
    }
}

/// Stop the spinner if it is still running. Safe to call repeatedly.
async fn stop_spinner(spinner: &mut Spinner, chat_ui: &mut ChatUi) {
    let spinner = std::mem::replace(spinner, Spinner::None);
    spinner.finish(chat_ui).await;
}

fn input_channel() -> mpsc::UnboundedReceiver<String> {
    let (sender, receiver) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            let mut input = String::new();
            match io::stdin().read_line(&mut input) {
                Ok(0) | Err(_) => break,
                Ok(_) if sender.send(input).is_err() => break,
                Ok(_) => {}
            }
        }
    });
    receiver
}

#[allow(clippy::too_many_arguments)]
fn handle_command(
    input: &str,
    provider: &dyn Provider,
    context_window: Option<u64>,
    database: &Database,
    session: &mut Option<Session>,
    messages: &mut Vec<Message>,
    always_allowed: &mut HashSet<String>,
    last_turn_snapshot: &mut Option<HashMap<PathBuf, Option<String>>>,
    prices: &Prices,
    mut tui: Option<&mut ChatUi>,
) -> Result<()> {
    let (command, argument) = input.split_once(' ').unwrap_or((input, ""));
    let argument = argument.trim();
    // Command output is buffered and flushed once at the end: fullscreen mode renders it as
    // a single transcript notice, plain mode keeps direct stdout. Helpers append to the same
    // buffer so nothing ever prints raw into a frame ratatui owns.
    let mut out_buf = String::new();
    macro_rules! out {
        () => { out_buf.push('\n') };
        ($($arg:tt)*) => { out_buf.push_str(&format!($($arg)*)) };
    }

    match command {
        "/help" => print_help(&mut out_buf),
        "/new" => {
            *session = None;
            messages.clear();
            always_allowed.clear();
            *last_turn_snapshot = None;
            out!("Started a new chat. It will be saved after the first response.\n");
        }
        "/sessions" => {
            let sessions = database.list_sessions()?;
            if sessions.is_empty() {
                out!("No saved sessions.\n");
            } else {
                let active = session.as_ref().map(|session| session.id.as_str());
                out!("{}", format_session_rows(&sessions, active));
            }
        }
        "/resume" => {
            let resumed = resolve_session(database, argument)?;
            if resumed.provider != provider.name() {
                anyhow::bail!(
                    "session uses provider '{}', but '{}' is active",
                    resumed.provider,
                    provider.name()
                );
            }
            *messages = database.load_messages(&resumed.id)?;
            out!("Resumed: {} ({})\n", resumed.title, short_id(&resumed.id));
            // Note: Plan Mode restore is handled by the main loop's plan_mode state;
            // /resume via handle_command is not the startup resume path, so we don't
            // rehydrate here — the caller would need &mut plan_mode.
            *session = Some(resumed);
            if let Some(ui) = tui.as_deref_mut() {
                replay_tui_history(ui, messages)?;
            } else {
                print_history_preview(messages);
            }
        }
        "/delete" => {
            let target = resolve_session(database, argument)?;
            database.delete_session(&target.id)?;
            out!("Deleted: {}\n", target.title);
            if session
                .as_ref()
                .is_some_and(|session| target.id == session.id)
            {
                *session = None;
                messages.clear();
                always_allowed.clear();
                *last_turn_snapshot = None;
                out!("Started a new chat. It will be saved after the first response.\n");
            }
        }
        "/rename" => {
            let (id_prefix, new_title) =
                argument.split_once(char::is_whitespace).unwrap_or(("", ""));
            let new_title = new_title.trim();
            if id_prefix.is_empty() || new_title.is_empty() {
                anyhow::bail!("usage: /rename <id> <new title>");
            }
            let target = resolve_session(database, id_prefix.trim())?;
            database.rename_session(&target.id, new_title)?;
            if let Some(active) = session.as_mut()
                && active.id == target.id
            {
                active.title = new_title.to_string();
            }
            out!("Renamed {} to: {new_title}\n", short_id(&target.id));
        }
        "/search" => {
            if argument.is_empty() {
                anyhow::bail!("usage: /search <text>");
            }
            let hits = database.search_messages(argument, 20)?;
            if hits.is_empty() {
                out!("No messages matched \"{argument}\".\n");
            } else {
                // Buffered like every other command: the bare `println!` that used to close
                // this listing punched raw text straight through the frame ratatui owns.
                out!("{}", format_search_hits(&hits, argument));
            }
        }
        "/stats" => match session.as_ref() {
            Some(session) => print_stats(database, session, context_window, prices, &mut out_buf)?,
            None => out!("This chat has no saved messages yet.\n"),
        },
        "/usage" => print_usage_report(database, prices, &mut out_buf)?,
        "/memory" => {
            let entries = database.list_memory()?;
            if entries.is_empty() {
                out!("Nothing remembered yet.\n");
            } else {
                out!("{}", format_memory_rows(&entries));
            }
        }
        "/forget" => {
            if argument.is_empty() {
                anyhow::bail!("usage: /forget <text> or /forget all");
            }
            if argument.eq_ignore_ascii_case("all") {
                let count = database.clear_memory()?;
                out!("Forgot all {count} remembered fact(s).\n");
            } else if database.forget(argument)? {
                out!("Forgot the fact matching \"{argument}\".\n");
            } else {
                out!(
                    "No remembered fact matches \"{argument}\", or the text matches more than \
                     one. Use /memory to see exact wording.\n"
                );
            }
        }
        // Name the command and point at the nearest real one. Several rejected commands in a
        // row produced identical lines with nothing to tell them apart, which is how this was
        // reported in the first place.
        _ => match nearest_command(command) {
            Some(suggestion) => out!(
                "Unknown command \"{command}\". Did you mean /{suggestion}? Type /help for the \
                 full list.\n"
            ),
            None => out!("Unknown command \"{command}\". Type /help for available commands.\n"),
        },
    }

    if !out_buf.is_empty() {
        match tui {
            Some(ui) => ui.notice(out_buf.trim_end())?,
            None => print!("{out_buf}"),
        }
    }

    Ok(())
}

/// One row per session, each terminating itself. `out!` appends nothing of its own, so a row
/// formatter that omits the newline renders the entire listing as one run-on line -- which is
/// exactly what `/sessions`, `/search`, and `/memory` all did.
fn format_session_rows(
    sessions: &[crate::storage::SessionSummary],
    active: Option<&str>,
) -> String {
    let mut out = String::new();
    for item in sessions {
        let marker = if active == Some(item.id.as_str()) {
            "*"
        } else {
            " "
        };
        out.push_str(&format!(
            "{marker} {}  {}  {:<40} {:>3} messages  {:>8} tokens\n",
            short_id(&item.id),
            format_timestamp(item.updated_at),
            item.title,
            item.message_count,
            item.total_tokens
        ));
    }
    out
}

/// One row per search hit, same self-terminating rule as `format_session_rows`.
fn format_search_hits(hits: &[crate::storage::SearchHit], needle: &str) -> String {
    let mut out = String::new();
    for hit in hits {
        let speaker = match hit.role.as_str() {
            "user" => "You",
            "assistant" => "Assistant",
            "system" => "System",
            _ => "?",
        };
        out.push_str(&format!(
            "{}  {}  {:<30}  {speaker}: {}\n",
            short_id(&hit.session_id),
            format_timestamp(hit.created_at),
            crate::tui::truncate_chars(&hit.title, 30),
            make_snippet(&hit.content, needle),
        ));
    }
    out
}

/// The `/memory` listing: one fact per row, then the removal hint.
fn format_memory_rows(entries: &[crate::storage::MemoryEntry]) -> String {
    let mut out = String::from("Remembered facts:\n");
    for entry in entries {
        out.push_str(&format!("- {}\n", entry.content));
    }
    out.push_str("\nUse /forget <text> or /forget all.\n");
    out
}

/// The closest built-in to a rejected command: a prefix match first (`/sess` -> `sessions`),
/// then a small edit distance for typos. `None` when nothing is close enough to suggest.
fn nearest_command(typed: &str) -> Option<&'static str> {
    let typed = typed.trim_start_matches('/').to_ascii_lowercase();
    if typed.is_empty() {
        return None;
    }
    let names = crate::tui::BUILTINS.iter().map(|(name, _)| *name);
    if let Some(prefix) = names
        .clone()
        .filter(|name| name.starts_with(&typed))
        .min_by_key(|name| name.len())
    {
        return Some(prefix);
    }
    names
        .map(|name| (edit_distance(&typed, name), name))
        .filter(|(distance, _)| *distance <= 2)
        .min_by_key(|(distance, name)| (*distance, name.len()))
        .map(|(_, name)| name)
}

fn resolve_builtin_command(command: &str) -> Result<String> {
    if !command.starts_with('/') {
        return Ok(command.to_string());
    }
    let needle = command[1..].to_ascii_lowercase();
    if let Some((name, _)) = crate::tui::BUILTINS
        .iter()
        .find(|(name, _)| *name == needle)
    {
        return Ok(format!("/{name}"));
    }
    let matches: Vec<&str> = crate::tui::BUILTINS
        .iter()
        .filter(|(name, _)| name.starts_with(&needle))
        .map(|(name, _)| *name)
        .collect();
    match matches.as_slice() {
        [name] => Ok(format!("/{name}")),
        [] => Ok(command.to_string()),
        _ => anyhow::bail!(
            "Ambiguous command \"{command}\". Choose one of: {}",
            matches
                .iter()
                .map(|name| format!("/{name}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Levenshtein distance over chars, two rows at a time. Only ever run against the short
/// built-in command list, so the quadratic cost is irrelevant.
fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];
    for (i, left_char) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(left_char != *right_char);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

fn resolve_session(database: &Database, id_prefix: &str) -> Result<Session> {
    if id_prefix.is_empty() {
        anyhow::bail!("a session ID is required");
    }
    database
        .find_session(id_prefix)?
        .with_context(|| format!("session '{id_prefix}' was not found or is ambiguous"))
}

fn print_stats(
    database: &Database,
    session: &Session,
    context_window: Option<u64>,
    prices: &Prices,
    out: &mut String,
) -> Result<()> {
    let stats = database.session_stats(&session.id)?;
    let _ = writeln!(out, "\nSession:       {}", session.title);
    let _ = writeln!(out, "Requests:      {}", stats.request_count);
    let _ = writeln!(out, "Input tokens:  {}", stats.input_tokens);
    let _ = writeln!(out, "Output tokens: {}", stats.output_tokens);
    let _ = writeln!(out, "Total tokens:  {}", stats.total_tokens);
    // Cost is opt-in end to end: with no `[pricing]` configured there is no line, no zero, and not
    // even an extra query — the report stays exactly what it has always been.
    let mut unpriced = false;
    if let Some((cost, has_unpriced)) = session_cost(database, &session.id, prices)? {
        unpriced |= has_unpriced;
        let _ = writeln!(out, "Cost:          {cost}");
    }
    if stats.cached_tokens > 0 {
        let percent = if stats.input_tokens > 0 {
            (stats.cached_tokens as f64 / stats.input_tokens as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "Cached tokens: {} ({percent:.0}%)",
            stats.cached_tokens
        );
    }
    // Prompt-cache behaviour, turn by turn. Lifetime token totals lag -- one cold session drags
    // them for good -- so this counts turns, and it only appears for a provider that actually
    // returns cached tokens.
    let samples = database.cache_samples(&session.id)?;
    if samples.iter().any(|(_, cached)| *cached > 0)
        && let Some(cache) = cache::report(&samples)
    {
        let _ = writeln!(
            out,
            "Prompt cache:  median {:.0}% over {} turns | \u{2265}90%: {:.0}% | \u{2265}95%: {:.0}% | warm-up: {}",
            cache.median, cache.measured, cache.pct_ge_90, cache.pct_ge_95, cache.warmup
        );
    }
    if let (Some(last_input), Some(window)) = (stats.last_input_tokens, context_window) {
        // Buffered like every other line. `print!` here sent the report's most useful line to
        // raw stdout, which both dropped it from `/stats` and wrote it over the frame.
        let percent = last_input as f64 / window as f64 * 100.0;
        let _ = write!(out, "Last context:  {last_input}/{window} ({percent:.1}%)");
        if let Some(cached) = stats.last_cached_tokens.filter(|cached| *cached > 0) {
            let cached_percent = (cached as f64 / last_input as f64 * 100.0).min(100.0);
            let _ = write!(out, " | Cached: {cached} ({cached_percent:.0}%)");
        }
        let _ = writeln!(out);
    }
    let by_model = database.model_stats(&session.id)?;
    if by_model.len() > 1 {
        let _ = writeln!(out, "\n--- Per model ---");
        for m in &by_model {
            // Priced from this row's own (chat-only) tokens, so each line is honest about exactly
            // the numbers standing beside it.
            let cell = cost_cell(
                prices,
                [(Some(m.model.as_str()), m.input_tokens, m.output_tokens)],
            );
            if let Some((_, has_unpriced)) = &cell {
                unpriced |= has_unpriced;
            }
            let _ = writeln!(
                out,
                "{}",
                model_row(m, cell.as_ref().map(|(cost, _)| cost.as_str()))
            );
        }
    }
    if unpriced {
        let _ = writeln!(out, "\n{UNPRICED_NOTE}");
    }
    let _ = writeln!(out,);
    Ok(())
}

/// Explains a `+` or `unpriced` cell, printed only under a report that produced one.
const UNPRICED_NOTE: &str =
    "Some usage came from a model with no price in [pricing.models]; it is excluded, not free.";

/// This session's total cost, or `None` when no prices are configured. Sums every usage kind, not
/// just `kind = 'chat'`: the token totals it sits under include title generation, and so does the
/// bill. The query itself is skipped when there is nothing to price.
fn session_cost(
    database: &Database,
    session_id: &str,
    prices: &Prices,
) -> Result<Option<(String, bool)>> {
    if prices.is_empty() {
        return Ok(None);
    }
    let tokens = database.session_model_tokens(session_id)?;
    Ok(cost_cell(prices, model_token_rows(&tokens)))
}

/// The cost cell for one report row, plus whether any of that row's tokens could not be priced.
/// `None` when the user configured no prices at all — which is how the column stays absent
/// entirely, rather than showing a blank or a zero that would read as free.
fn cost_cell<'a>(
    prices: &Prices,
    rows: impl IntoIterator<Item = (Option<&'a str>, i64, i64)>,
) -> Option<(String, bool)> {
    if prices.is_empty() {
        return None;
    }
    let tally = prices.tally(rows);
    Some((prices.format(&tally), tally.has_unpriced()))
}

/// Adapt stored per-model token sums to what `cost_cell` takes.
fn model_token_rows(
    tokens: &[storage::ModelTokens],
) -> impl Iterator<Item = (Option<&str>, i64, i64)> {
    tokens
        .iter()
        .map(|row| (row.model.as_deref(), row.input_tokens, row.output_tokens))
}

/// The per-model token rows recorded for one period, or nothing when that period has none.
fn tokens_for<'a>(
    models: &'a HashMap<String, Vec<storage::ModelTokens>>,
    period: &str,
) -> &'a [storage::ModelTokens] {
    models.get(period).map_or(&[][..], Vec::as_slice)
}

/// One `/stats` per-model line. The cost cell is appended only when prices are configured, so the
/// line is unchanged for a user who configured none.
fn model_row(stat: &storage::ModelStat, cost: Option<&str>) -> String {
    let mut line = format!(
        "  {:<24} {:>3} req  {:>8} in  {:>8} out  {:>8} total",
        stat.model, stat.request_count, stat.input_tokens, stat.output_tokens, stat.total_tokens
    );
    if stat.cached_tokens > 0 {
        line.push_str(&format!("  {:>8} cached", stat.cached_tokens));
    }
    if let Some(cost) = cost {
        line.push_str(&format!("  {cost:>12}"));
    }
    line
}

/// One `/usage` line, with the same opt-in cost cell as `model_row`.
fn usage_row(period: &storage::UsagePeriod, cost: Option<&str>) -> String {
    let mut line = format!(
        "  {:<10} {:>4} req  {:>10} in  {:>10} out  {:>10} total",
        period.period,
        period.request_count,
        period.input_tokens,
        period.output_tokens,
        period.total_tokens
    );
    if period.cached_tokens > 0 {
        let percent = if period.input_tokens > 0 {
            (period.cached_tokens as f64 / period.input_tokens as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        line.push_str(&format!(
            "  {:>8} cached ({percent:.0}%)",
            period.cached_tokens
        ));
    }
    if let Some(cost) = cost {
        line.push_str(&format!("  {cost:>12}"));
    }
    line
}

/// When `send_session_id` is set (Orvix Coding Plan), ensure a persisted Kamui session exists and
/// return its id for the provider wire body.
/// What compaction costs a cache-pinned session, appended to the notice so the next turn's
/// collapsed hit rate has a stated cause. Empty for every other profile, where dropping older
/// messages costs nothing but the messages.
fn cache_reset_note(cache_pinned: bool) -> &'static str {
    if cache_pinned {
        " The cached prompt prefix resets, so the next turn warms up again."
    } else {
        ""
    }
}

fn ensure_coding_session_id(
    session: &mut Option<Session>,
    database: &Database,
    provider_name: &str,
    model: &str,
    send_session_id: bool,
) -> Result<Option<String>> {
    if !send_session_id {
        return Ok(None);
    }
    if session.is_none() {
        *session = Some(database.create_session(provider_name, model)?);
    }
    Ok(session.as_ref().map(|s| s.id.clone()))
}

/// Fold the older, un-summarized messages into a fresh running summary via a non-streaming request.
/// Returns the new summary, the new summarized-up-to index, and how many messages were folded in, or
/// `None` when there is nothing new worth summarizing.
async fn run_compaction(
    provider: &dyn Provider,
    model: &str,
    messages: &[Message],
    summary: Option<&str>,
    summarized_upto: usize,
    session_id: Option<String>,
) -> Result<Option<(String, usize, usize)>> {
    let Some(cutoff) = compaction::cutoff(messages.len(), summarized_upto) else {
        return Ok(None);
    };
    let rendered = compaction::render(&messages[summarized_upto..cutoff]);
    let request = compaction::summary_request(model, summary, &rendered, session_id);
    let response = provider.chat(request).await?;
    Ok(Some((
        response.content.trim().to_string(),
        cutoff,
        cutoff - summarized_upto,
    )))
}

/// List profiles, or switch to a named one and persist the choice. Rebuilding the provider swaps the
/// base URL and API key; the model and context window follow the profile.
#[allow(clippy::too_many_arguments)]
fn switch_profile<F>(
    name: &str,
    config: &Config,
    active: &mut Profile,
    provider: &mut Box<dyn Provider>,
    context_window: &mut Option<u64>,
    database: &Database,
    build_provider: &F,
    mut tui: Option<&mut ChatUi>,
) -> Result<()>
where
    F: Fn(&Profile) -> Box<dyn Provider>,
{
    macro_rules! out {
        ($($arg:tt)*) => {
            if let Some(ui) = tui.as_deref_mut() {
                ui.notice(&format!($($arg)*))?;
            } else {
                println!($($arg)*);
            }
        };
    }
    if name.is_empty() {
        out!("Profiles:");
        for profile in &config.profiles {
            let marker = if profile.name == active.name {
                "*"
            } else {
                " "
            };
            let tools = if profile.tools { "" } else { "  [no tools]" };
            out!(
                "{marker} {:<16} {:<22} {}{tools}",
                profile.name,
                profile.model,
                profile.base_url
            );
        }
        println!();
        return Ok(());
    }

    match config.find(name) {
        Some(profile) => {
            *active = profile.clone();
            *provider = build_provider(profile);
            *context_window = profile.context_window;
            database.set_setting(ACTIVE_PROFILE_KEY, &profile.name)?;
            out!("Now using {} ({}).\n", profile.model, profile.name);
            if let Some(note) = tools_disabled_note(profile) {
                out!("{note}\n");
            }
        }
        None => out!("Unknown profile '{name}'. Type /model to list profiles.\n"),
    }
    Ok(())
}

fn shutdown(
    chat_ui: &mut ChatUi,
    database: &Database,
    session: Option<&Session>,
    context_window: Option<u64>,
    jobs: &tools::JobRegistry,
    prices: &Prices,
) -> Result<()> {
    // Nothing should outlive the process: a still-running background job has no way to be
    // checked on or stopped once Kamui exits.
    tools::kill_all_jobs(jobs);
    // Hand the terminal back first. Everything below is printed for the user to keep, and the
    // alternate screen it would otherwise land on is discarded on the way out -- which is why
    // exiting had stopped leaving any trace of the session behind.
    chat_ui.leave_fullscreen();
    let mut buf = String::new();
    // Compact sign-off logo — same KAMUI art at ~50% height so the exit
    // feels branded but not as dominant as the fullscreen intro.
    for line in crate::ui::EXIT_LOGO_SMALL {
        buf.push_str(line);
        buf.push('\n');
    }
    buf.push('\n');
    if let Some(session) = session {
        print_stats(database, session, context_window, prices, &mut buf)?;
        buf.push_str(&format!(
            "To resume this session: kamui -r {}\n",
            short_id(&session.id)
        ));
    }
    buf.push_str("Goodbye\n");
    print!("{buf}");
    Ok(())
}

/// Snapshot a `patch_file` call's target before it is dispatched, so the turn can be reverted if
/// cancelled. First-touch-wins: if this path was already snapshotted earlier in the turn, the
/// existing entry (the file's state *before this turn*) is kept, not overwritten with an
/// intermediate edit. If the file exists but cannot be read as UTF-8, it is left unsnapshotted
/// rather than guessing — `patch_file` itself will fail the same way, so nothing gets written and
/// there is nothing to revert for that path.
fn snapshot_patch_target(
    root: &Path,
    arguments: &str,
    snapshot: &mut HashMap<PathBuf, Option<String>>,
) {
    let Some(target) = tools::patch_target(root, arguments) else {
        return;
    };
    if snapshot.contains_key(&target) {
        return;
    }
    if target.is_file() {
        if let Ok(content) = std::fs::read_to_string(&target) {
            snapshot.insert(target, Some(content));
        }
    } else {
        snapshot.insert(target, None);
    }
}

/// Revert every file in a turn's patch snapshot back to its pre-turn state: restore the original
/// content, or delete a file that did not exist before the turn. Best-effort — a failure on one
/// file is reported but does not stop the rest from being reverted. Returns how many files were
/// successfully reverted.
/// What reverting a turn's file snapshot actually did: which files came back, and which would
/// not. A bare count says neither, and a failure used to go to stderr -- straight through the
/// frame ratatui owns, where it corrupts the display instead of being read.
#[derive(Debug, Default)]
struct RevertOutcome {
    reverted: Vec<String>,
    failed: Vec<String>,
}

impl RevertOutcome {
    fn is_empty(&self) -> bool {
        self.reverted.is_empty() && self.failed.is_empty()
    }

    /// A report that names the files. `context` says which turn they belong to.
    fn summary(&self, context: &str) -> String {
        let mut out = if self.reverted.is_empty() {
            format!("Nothing reverted {context}.")
        } else {
            format!(
                "Reverted {} file(s) {context}: {}",
                self.reverted.len(),
                self.reverted.join(", ")
            )
        };
        if !self.failed.is_empty() {
            // Surfaced, never swallowed: a file that would not revert is still changed on disk.
            out.push_str(&format!(
                "\nCould not revert {}: {}",
                self.failed.len(),
                self.failed.join("; ")
            ));
        }
        out
    }
}

fn revert_snapshot(snapshot: &HashMap<PathBuf, Option<String>>) -> RevertOutcome {
    let mut outcome = RevertOutcome::default();
    // Sorted, so the same revert reads the same way twice: a HashMap yields no fixed order.
    let mut entries: Vec<(&PathBuf, &Option<String>)> = snapshot.iter().collect();
    entries.sort_by_key(|(left, _)| *left);
    for (path, original) in entries {
        let result = match original {
            Some(content) => tools::write_atomic(path, content),
            None => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            },
        };
        match result {
            Ok(()) => outcome.reverted.push(display_path(path)),
            Err(error) => outcome
                .failed
                .push(format!("{} ({error:#})", display_path(path))),
        }
    }
    outcome
}

/// If this turn touched any files, revert them and report how many. Called right before
/// abandoning a cancelled or failed turn so a Ctrl+C (or a dropped request) never leaves a
/// multi-file edit half-applied with no trace in session history.
fn revert_on_cancel(chat_ui: &mut ChatUi, snapshot: &HashMap<PathBuf, Option<String>>) {
    if snapshot.is_empty() {
        return;
    }
    let outcome = revert_snapshot(snapshot);
    if !outcome.is_empty() {
        let _ = chat_ui.notice(&outcome.summary("from the interrupted turn"));
    }
}

/// Replay recent user/assistant turns into the fullscreen transcript. Startup `-r` used to only
/// print a notice, so the home logo stayed up and the old chat looked gone even though the model
/// still had it. Notes do not leave intro, so we also drop the logo once a session is attached.
fn replay_tui_history(ui: &mut ChatUi, messages: &[Message]) -> Result<()> {
    let skip = messages.len().saturating_sub(RESUME_REPLAY_MESSAGES);
    if skip > 0 {
        ui.notice(&format!(
            "{skip} earlier message(s) not replayed \u{2014} still in context"
        ))?;
    }
    for message in &messages[skip..] {
        match message.role {
            Role::User => ui.user(&message.content)?,
            Role::Assistant if !message.content.is_empty() => {
                ui.assistant_replay(&message.content)?
            }
            _ => {}
        }
    }
    // A resume notice is a Note card, which would otherwise keep the logo home screen.
    ui.leave_intro()?;
    Ok(())
}

fn print_history_preview(messages: &[Message]) {
    if messages.is_empty() {
        println!("No previous messages.\n");
        return;
    }

    let start = messages.len().saturating_sub(RESUME_PREVIEW_MESSAGES);
    if start > 0 {
        println!("... {start} earlier messages omitted\n");
    }
    for message in &messages[start..] {
        let speaker = match message.role_name() {
            "user" => "You",
            "assistant" => "Assistant",
            "system" => "System",
            "tool" => "Tool",
            _ => "?",
        };
        let body = if message.content.is_empty() && !message.tool_calls.is_empty() {
            let names: Vec<&str> = message
                .tool_calls
                .iter()
                .map(|call| call.name.as_str())
                .collect();
            format!("(requested tools: {})", names.join(", "))
        } else {
            message.content.clone()
        };
        println!("{speaker}:\n{body}\n");
    }
    println!("--- End of history ---\n");
}

/// Refresh the fullscreen sidebar rail (opencode-style): session identity on top, model and
/// context usage beneath. Called at startup and after every completed round so the context
/// figure tracks the live conversation.
/// Model-picker entries: every configured profile plus the registry entry that opens the
/// add-provider wizard (onboarding reused as an in-TUI model registry).
fn model_dialog_items(config: &Config) -> Vec<(String, String)> {
    let mut items: Vec<(String, String)> = config
        .profiles
        .iter()
        .map(|profile| {
            (
                profile.name.clone(),
                format!("{} · {}", profile.name, profile.model),
            )
        })
        .collect();
    items.push((
        "__add__".to_string(),
        "＋ Add provider / model…".to_string(),
    ));
    items
}

/// Refreshes the picker source from the live config (after adds/switches).
fn refresh_model_source(config: &Config, hub: &InputHub) {
    hub.set_models(model_dialog_items(config));
}
/// Pushes recent sessions into the Ctrl+S switcher (id -> title labels).
fn refresh_session_source(database: &Database, hub: &InputHub) {
    if let Ok(sessions) = database.list_sessions() {
        hub.set_sessions(
            sessions
                .into_iter()
                .take(15)
                .map(|session| (session.id.clone(), session.title))
                .collect(),
        );
    }
}

/// The sidebar's MCP block: one row per configured server with its live tool count, and an
/// explicit "unavailable" for one that failed to start. A server that simply vanishes from the
/// rail is indistinguishable from one that was never configured.
/// How much a turn is allowed to do on its own. Kamui already had all three behaviours -- Plan
/// Mode, ordinary approvals, and `--auto-approve` -- but only as a command, a default, and a
/// launch flag, so there was no way to see which was in force or to move between them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Approvals required for anything that mutates. The default.
    Build,
    /// Approvals bypassed for the rest of the session (what `--auto-approve` starts in).
    Auto,
    /// Read-only tools plus `update_plan` until a plan is approved.
    Plan,
}

impl Mode {
    const CYCLE: [Mode; 3] = [Mode::Build, Mode::Auto, Mode::Plan];

    fn next(self) -> Self {
        let index = Self::CYCLE.iter().position(|m| *m == self).unwrap_or(0);
        Self::CYCLE[(index + 1) % Self::CYCLE.len()]
    }

    fn prev(self) -> Self {
        let index = Self::CYCLE.iter().position(|m| *m == self).unwrap_or(0);
        Self::CYCLE[(index + Self::CYCLE.len() - 1) % Self::CYCLE.len()]
    }

    fn label(self) -> &'static str {
        match self {
            Mode::Build => "build",
            Mode::Auto => "auto",
            Mode::Plan => "plan",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "build" | "normal" => Some(Mode::Build),
            "auto" | "auto-approve" | "bypass" => Some(Mode::Auto),
            "plan" => Some(Mode::Plan),
            _ => None,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Mode::Build => "build \u{2014} every command and edit asks first",
            Mode::Auto => {
                "auto \u{2014} commands and edits run without asking, for this session only"
            }
            Mode::Plan => "plan \u{2014} read-only until you approve a plan",
        }
    }
}

/// The sidebar's model row. A profile with `tools = false` says so here: without tools the
/// agent cannot read a file or run a command, and a model that is never offered any will
/// happily invent tool-call syntax in plain prose instead, which reads as the agent being
/// broken rather than switched off.
fn model_label(active: &Profile) -> String {
    if active.tools {
        active.model.clone()
    } else {
        format!("{} \u{b7} no tools", active.model)
    }
}

/// Spelled out once when it matters: at startup and whenever the active profile changes.
fn tools_disabled_note(active: &Profile) -> Option<String> {
    (!active.tools).then(|| {
        format!(
            "Profile '{}' has tools = false, so {} is offered no tools: it cannot read files, \
             search, or run commands. Set tools = true in kamui.toml, or /model to a profile \
             that has them.",
            active.name, active.model
        )
    })
}

fn mcp_sidebar_value(statuses: &[ConnectionStatus]) -> String {
    statuses
        .iter()
        .map(|server| match &server.error {
            Some(_) => format!("- {}\n  unavailable", server.name),
            None => format!("- {}\n  {} tool(s)", server.name, server.tool_count),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(clippy::too_many_arguments)]
fn update_sidebar(
    chat_ui: &mut ChatUi,
    session: Option<&Session>,
    model: &str,
    mode: &str,
    version: &str,
    project: &ProjectContext,
    mcp: &str,
    last_input_tokens: Option<u64>,
    last_cached_tokens: Option<u64>,
    context_window: Option<u64>,
    // Cache-pinned profile (Orvix Coding Plan): always surface the cache line, including warm-up.
    cache_pinned: bool,
    // Session-level cache summary (`median …`), when enough chat turns exist.
    session_cache: Option<String>,
    last_turn: Option<String>,
    activity: Option<String>,
) {
    if !chat_ui.is_fullscreen() {
        return;
    }
    let mut entries: Vec<(String, String)> = Vec::new();
    // Groups mirror the prototype pick: Session identity, Runtime (model/mode/git/project),
    // Context with a variant-C usage bar, then Activity + Last turn metrics.
    let mut session_value = match session {
        Some(session) => session.title.clone(),
        None => "New chat".to_string(),
    };
    if let Some(session) = session {
        session_value.push_str(&format!("\nid\t{}", short_id(&session.id)));
    }
    session_value.push_str(&format!("\nversion\tv{version}"));
    entries_push(&mut entries, "Session", session_value);
    let mut runtime = format!("model\t{model}\nmode\t{mode}");
    if let Some(git) = git_status(project.root()) {
        let dirty = if git.changed == 0 {
            String::new()
        } else {
            format!(" · {} changed", git.changed)
        };
        runtime.push_str(&format!("\ngit\t{}{dirty}", git.branch));
    }
    runtime.push_str(&format!("\nproject\t{}", display_path(project.root())));
    if !mcp.is_empty() {
        runtime.push_str(&format!("\nmcp\t{mcp}"));
    }
    entries_push(&mut entries, "Runtime", runtime);
    let mut context_line = match (last_input_tokens, context_window) {
        (Some(tokens), Some(window)) => {
            format!(
                "{tokens} tokens ({:.1}% of {window})",
                tokens as f64 / window as f64 * 100.0
            )
        }
        (Some(tokens), None) => format!("{tokens} tokens"),
        (None, _) => "\u{2014}".to_string(),
    };
    if let (Some(tokens), Some(window)) = (last_input_tokens, context_window)
        && window > 0
    {
        let pct = ((tokens as f64 / window as f64) * 100.0).min(100.0).round() as u8;
        context_line.push_str(&format!("\nbar\t{pct}"));
    }
    match (last_input_tokens, last_cached_tokens) {
        (Some(tokens), Some(cached)) if cached > 0 && tokens > 0 => {
            let hit = (cached as f64 / tokens as f64 * 100.0).min(100.0);
            context_line.push_str(&format!("\ncache\t{cached} ({hit:.0}%)"));
        }
        (Some(_), Some(0)) if cache_pinned => {
            context_line.push_str("\ncache\t0 (warm-up)");
        }
        _ => {}
    }
    if let Some(summary) = session_cache {
        context_line.push_str(&format!("\nsession\t{summary}"));
    }
    entries_push(&mut entries, "Context", context_line);
    // Status-bar badge: tokens + context pressure; cache hit when the turn had one.
    match last_input_tokens {
        Some(tokens) => {
            let pct: u8 = context_window
                .map(|window| ((tokens as f64 / window as f64) * 100.0).min(100.0).round() as u8)
                .unwrap_or(0);
            let mut text = if tokens >= 1000 {
                format!("{:.1}k tok", tokens as f64 / 1000.0)
            } else {
                format!("{tokens} tok")
            };
            if let Some(cached) = last_cached_tokens.filter(|c| *c > 0)
                && tokens > 0
            {
                let hit = (cached as f64 / tokens as f64 * 100.0).min(100.0);
                text.push_str(&format!(" · {hit:.0}% hit"));
            } else if cache_pinned && last_cached_tokens == Some(0) {
                text.push_str(" · warm");
            }
            let _ = chat_ui.set_token_badge(Some((text, pct)));
        }
        None => {
            let _ = chat_ui.set_token_badge(None);
        }
    }
    if let Some(activity) = activity {
        entries_push(&mut entries, "Activity", activity);
    }
    if let Some(last_turn) = last_turn {
        entries_push(&mut entries, "Last turn", last_turn);
    }
    let _ = chat_ui.set_sidebar(entries);
}

/// Session median cache line for the Context rail, or `None` until there are measured turns.
fn session_cache_line(database: &Database, session_id: &str) -> Option<String> {
    let samples = database.cache_samples(session_id).ok()?;
    let report = cache::report(&samples)?;
    Some(format!(
        "median {:.0}% · {} turns · \u{2265}90% {:.0}%",
        report.median, report.measured, report.pct_ge_90
    ))
}

/// Push a sidebar group, merging into the previous entry when the same group is
/// pushed twice (startup + refresh both contribute before the first turn).
fn entries_push(entries: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some(existing) = entries.iter_mut().find(|(k, _)| k == key) {
        existing.1.push('\n');
        existing.1.push_str(&value);
    } else {
        entries.push((key.to_string(), value));
    }
}

/// Prefix-cache miss detection (Pi parity: `cache-stats.ts` `detectCacheMiss`).
/// Compares this turn's cached tokens against the previous turn's: a drop larger
/// than the noise floor means the prefix broke somewhere. Returns a short label
/// for the `cache` line, or `None` when there is nothing worth reporting (first
/// turn, or the delta is within noise). Callers pass what changed this turn so
/// the label names the likely cause instead of just saying "miss".
///
/// Smallest observable contract: same-prefix turns stay silent, a fresh-prefix
/// turn after a cached one reports `miss`, first turns never report.
fn cache_miss_label(
    prev_cached: Option<u64>,
    cached: u64,
    input: u64,
    model_switched: bool,
    head_rebuilt: bool,
) -> Option<&'static str> {
    /// Misses at or below this many tokens are noise (cold start, rounding),
    /// not a broken prefix. Same role as Pi's `NOISE_FLOOR_TOKENS = 1024`.
    const NOISE_FLOOR_TOKENS: u64 = 1024;
    let prev = prev_cached?;
    if prev <= NOISE_FLOOR_TOKENS || input == 0 {
        return None;
    }
    if cached >= prev.saturating_sub(NOISE_FLOOR_TOKENS) {
        return None;
    }
    Some(if model_switched {
        "miss (model switch)"
    } else if head_rebuilt {
        "miss (prefix rebuilt)"
    } else {
        "miss"
    })
}

#[allow(clippy::too_many_arguments)]
fn format_usage(
    input: u64,
    output: u64,
    total: u64,
    cached: u64,
    // Whether the active profile pins this session to a cached prefix (`send_session_id`).
    cache_pinned: bool,
    finish_reason: &str,
    ttft: Option<Duration>,
    elapsed: Duration,
    context_window: Option<u64>,
    miss_label: Option<&str>,
) -> String {
    let mut line = format!("Tokens: {input} input + {output} output = {total} total");
    // On a cache-pinned profile the field is always shown: a zero there is the whole signal --
    // either the session's first turn or a prefix that churned -- and hiding it left the failure
    // looking exactly like a provider that reports nothing.
    if let Some(percent) = cache::hit_percent(input, cached).filter(|_| cached > 0) {
        line.push_str(&format!(" | Cached: {cached} ({percent:.0}%)"));
    } else if let Some(miss) = miss_label {
        // A named cause beats a bare zero, so it wins when there is one.
        line.push_str(&format!(" | Cache {miss}"));
    } else if cache_pinned {
        line.push_str(" | Cached: 0 (warm-up)");
    }
    if let Some(window) = context_window {
        let percent = input as f64 / window as f64 * 100.0;
        line.push_str(&format!(" | Context: {percent:.1}%"));
    }
    if let Some(ttft) = ttft {
        line.push_str(&format!(
            " | TTFT: {}",
            crate::terminal::format_duration(ttft)
        ));
    }
    line.push_str(&format!(
        " | Time: {} | Finish: {finish_reason}",
        crate::terminal::format_duration(elapsed)
    ));
    line
}

/// What a `/warnings fix` turn achieved, measured by reloading the skill library rather than
/// by trusting the agent's own account of it.
struct SkillFixReport {
    summary: String,
    /// Replacement warning banner: empty once every flagged folder loads.
    banner: Option<String>,
}

/// The stable part of a skill warning: which folder it concerns, without the reason. A folder
/// that fails for a *different* reason after the repair has still not been fixed.
fn skill_warning_key(warning: &str) -> &str {
    for marker in [" is missing", " could not be read", " is invalid"] {
        if let Some(index) = warning.find(marker) {
            return &warning[..index];
        }
    }
    warning
}

fn skill_fix_report(before: &[String], after: &[String]) -> SkillFixReport {
    let before_keys: HashSet<&str> = before.iter().map(|w| skill_warning_key(w)).collect();
    let after_keys: HashSet<&str> = after.iter().map(|w| skill_warning_key(w)).collect();
    let repaired = before_keys.difference(&after_keys).count();
    let broke = after_keys.difference(&before_keys).count();

    let mut summary = if repaired == 0 {
        format!(
            "Skill repair: nothing fixed \u{2014} {} of {} folder(s) still fail to load.",
            after_keys.len(),
            before_keys.len()
        )
    } else if after_keys.is_empty() {
        format!("Skill repair: all {repaired} folder(s) now load. Restart Kamui to use them.")
    } else {
        format!(
            "Skill repair: {repaired} of {} folder(s) fixed, {} still failing.",
            before_keys.len(),
            after_keys.len()
        )
    };
    if broke > 0 {
        summary.push_str(&format!(
            " {broke} folder(s) newly broken \u{2014} /warnings details."
        ));
    }
    if !after_keys.is_empty() {
        summary.push_str(" /warnings details lists what is left.");
    }
    SkillFixReport {
        summary,
        banner: (!after.is_empty()).then(|| {
            format!(
                "{} skill folder(s) skipped (invalid name or frontmatter) \u{2014} /warnings details, /warnings fix",
                after.len()
            )
        }),
    }
}

/// A one-line summary of what a prompt is carrying besides its own words. `None` when it
/// carries nothing notable, so the ordinary case stays silent.
///
/// Omitted files matter as much as attached ones: an `@src` that quietly delivered twelve of
/// fifty files produced an answer built on partial context, with nothing on screen saying so.
fn describe_attachments(expanded: &crate::context::Expanded) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if expanded.attached_files > 0 {
        parts.push(format!("{} file(s) attached", expanded.attached_files));
    }
    if expanded.omitted_files > 0 {
        parts.push(format!(
            "{} left out (binary, too large, or over the context budget)",
            expanded.omitted_files
        ));
    }
    if let Some(images) = describe_images(&expanded.images) {
        parts.push(images);
    }
    (!parts.is_empty()).then(|| format!("\u{1f4ce} {}", parts.join(" \u{b7} ")))
}

/// The image half of `describe_attachments`.
fn describe_images(images: &[crate::provider::ImageAttachment]) -> Option<String> {
    if images.is_empty() {
        return None;
    }
    let kinds: Vec<String> = images
        .iter()
        .map(|image| {
            // `data` is base64, which costs four characters per three bytes.
            let bytes = image.data.len() / 4 * 3;
            format!("{} ~{}", image.media_type, format_bytes(bytes))
        })
        .collect();
    Some(format!(
        "{} image(s) ({}) \u{2014} needs a vision model",
        images.len(),
        kinds.join(", ")
    ))
}

fn format_bytes(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

/// A tool result stripped of its `Error: ` marker, which the outcome row already conveys.
fn tool_body(output: &str) -> &str {
    output.strip_prefix("Error: ").unwrap_or(output)
}

/// Fold one agent-loop round's usage into the turn total: output tokens accumulate across every
/// round, while the input count tracks the final round so it still reflects the context that was
/// sent. Total is the last input plus all output generated during the turn.
fn accumulate_usage(total: &mut Usage, round: &Usage) {
    total.completion_tokens += round.completion_tokens;
    total.prompt_tokens = round.prompt_tokens;
    total.cached_tokens = round.cached_tokens;
    total.total_tokens = total.prompt_tokens + total.completion_tokens;
}

fn make_title(input: &str) -> String {
    let mut title: String = input.chars().take(40).collect();
    if input.chars().count() > 40 {
        title.push_str("...");
    }
    title
}

fn clean_title(title: &str) -> String {
    title
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches(['"', '\'', '.', ':'])
        .chars()
        .take(60)
        .collect()
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Render a path for display, trimming the Windows verbatim prefix that `canonicalize` adds
/// (`\\?\C:\...` and `\\?\UNC\server\share`). The canonical form stays in use internally for
/// path-safety checks.
pub(crate) fn display_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(plain) = text.strip_prefix(r"\\?\") {
        plain.to_string()
    } else {
        text
    }
}

#[derive(serde::Deserialize)]
struct AskUserArguments {
    question: String,
    #[serde(default)]
    options: Vec<String>,
}

/// Print an `ask_user` question (with numbered options, if any) and read one line of stdin as the
/// answer. If the user types a number that matches an offered option, the option's text is
/// returned instead of the raw digit, so the model gets a proper answer either way; any other
/// text (a number out of range, or free-form text when no options were offered) is returned as
/// typed. Returns an `Error: ...` string, not an `Err`, for bad JSON — same convention as
/// `ToolRegistry::dispatch` — so the model can recover on the next round.
async fn read_approval_line(
    input_rx: &mut Option<mpsc::UnboundedReceiver<String>>,
    use_tui: bool,
    hub: Option<&mut InputHub>,
    title: &str,
    body: String,
) -> Option<String> {
    if use_tui {
        let hub = hub.expect("tui implies hub");
        hub.open_permission_modal(title, body);
        let answer = hub.request_line().await;
        hub.close_permission_modal();
        answer
    } else {
        input_rx.as_mut().unwrap().recv().await
    }
}

async fn read_plan_approval_line(
    input_rx: &mut Option<mpsc::UnboundedReceiver<String>>,
    use_tui: bool,
    hub: Option<&mut InputHub>,
    title: &str,
    body: String,
) -> Option<String> {
    if use_tui {
        let hub = hub.expect("tui implies hub");
        hub.open_permission_modal_with_options(title, body, ui::PLAN_OPTIONS.to_vec());
        let answer = hub.request_line().await;
        hub.close_permission_modal();
        answer
    } else {
        input_rx.as_mut().unwrap().recv().await
    }
}

fn is_plan_approval(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "approve" | "approved"
    )
}

async fn ask_user(
    input_rx: &mut Option<mpsc::UnboundedReceiver<String>>,
    use_tui: bool,
    arguments: &str,
    hub: Option<&mut InputHub>,
) -> Result<String> {
    let arguments: AskUserArguments = match serde_json::from_str(arguments) {
        Ok(arguments) => arguments,
        Err(error) => return Ok(format!("Error: invalid ask_user arguments: {error}")),
    };
    if arguments.question.trim().is_empty() {
        return Ok("Error: ask_user requires a non-empty 'question' argument".to_string());
    }

    let answer = if use_tui {
        let hub = hub.expect("tui implies hub");
        hub.open_ask_modal(&arguments.question, arguments.options.clone());
        let answer = hub.request_line().await.unwrap_or_default();
        hub.close_ask_modal();
        answer
    } else {
        println!("  ? {}", arguments.question);
        for (index, option) in arguments.options.iter().enumerate() {
            println!("    {}. {option}", index + 1);
        }
        print!("    > ");
        io::stdout().flush()?;
        input_rx.as_mut().unwrap().recv().await.unwrap_or_default()
    };
    let answer = answer.trim();
    let resolved = answer
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .and_then(|index| arguments.options.get(index))
        .map(String::as_str)
        .unwrap_or(answer);
    Ok(resolved.to_string())
}

#[derive(serde::Deserialize)]
struct SpawnAgentArguments {
    prompt: String,
}

/// Dispatch `spawn_agent`, converting any failure to an `Error: ...` string (same convention as
/// `ToolRegistry::dispatch`) so a misbehaving sub-agent fails the tool call, not the whole turn.
async fn dispatch_spawn_agent(
    provider: &dyn Provider,
    model: &str,
    project: &ProjectContext,
    arguments: &str,
    session_id: Option<String>,
) -> String {
    match run_spawned_agent(provider, model, project, arguments, session_id).await {
        Ok(output) => output,
        Err(error) => format!("Error: {error:#}"),
    }
}

/// Run independent `spawn_agent` calls concurrently while preserving the original tool-call order
/// in the map consumed by the parent loop. Batching caps provider fan-out and rate-limit pressure.
async fn dispatch_spawn_agents(
    provider: &dyn Provider,
    model: &str,
    project: &ProjectContext,
    calls: &[&ToolCall],
    session_id: Option<String>,
) -> HashMap<String, (String, Duration)> {
    let mut outputs = HashMap::with_capacity(calls.len());
    for batch in calls.chunks(MAX_CONCURRENT_SUB_AGENTS) {
        let futures = batch.iter().map(|call| {
            let session_id = cache::sub_agent_session_id(session_id.as_deref(), &call.id);
            async move {
                let started = Instant::now();
                (
                    call.id.clone(),
                    (
                        dispatch_spawn_agent(provider, model, project, &call.arguments, session_id)
                            .await,
                        started.elapsed(),
                    ),
                )
            }
        });
        outputs.extend(join_all(futures).await);
    }
    outputs
}

/// Run an isolated sub-agent to completion and return just its final answer: a fresh system
/// prompt and no shared history with the parent conversation, so the parent's context is not
/// polluted by the sub-agent's own exploration trace. Scoped to `ToolRegistry::read_only` — none
/// of those tools ever require confirmation, so there is no approval flow to reproduce here, and
/// `tool_definitions_only` omits `spawn_agent` itself, so a sub-agent cannot recurse.
async fn run_spawned_agent(
    provider: &dyn Provider,
    model: &str,
    project: &ProjectContext,
    arguments: &str,
    session_id: Option<String>,
) -> Result<String> {
    let arguments: SpawnAgentArguments = serde_json::from_str(arguments)
        .context("spawn_agent requires a 'prompt' string argument")?;
    if arguments.prompt.trim().is_empty() {
        anyhow::bail!("spawn_agent requires a non-empty 'prompt' argument");
    }

    let sub_tools = tools::ToolRegistry::read_only(project.root().to_path_buf());
    let tool_definitions = sub_tools.tool_definitions_only();
    let system = prompt::build(true, project.system_message().as_deref(), None);
    let mut messages = vec![Message::system(system), Message::user(&arguments.prompt)];

    let mut round = 0usize;
    loop {
        round += 1;
        if round > MAX_TOOL_ROUNDS {
            anyhow::bail!(
                "sub-agent stopped after {MAX_TOOL_ROUNDS} tool rounds without a final answer"
            );
        }

        let response = provider
            .chat(ChatRequest {
                model: model.to_string(),
                messages: messages.clone(),
                tools: tool_definitions.clone(),
                session_id: session_id.clone(),
            })
            .await
            .context("sub-agent request failed")?;

        if response.tool_calls.is_empty() {
            return Ok(response.content);
        }

        messages.push(Message::tool_request(
            response.content,
            response.tool_calls.clone(),
        ));
        for call in &response.tool_calls {
            let output = sub_tools.dispatch(call).await;
            messages.push(Message::tool_result(&call.id, output));
        }
    }
}

/// Rebuild the semantic-search index: walk the project the same `.gitignore`-aware way `grep`/
/// `glob` do, skip any file whose content hash matches what was indexed last time, chunk and embed
/// the rest, and drop entries for files that no longer exist. Returns a one-line summary.
/// Consecutive failures, with nothing succeeding, that mean the problem is the endpoint rather
/// than the files. Below this a failure is treated as one bad file and skipped.
const SYSTEMIC_INDEX_FAILURES: usize = 3;

async fn run_index(
    provider: &dyn Provider,
    active: &Profile,
    database: &Database,
    project: &ProjectContext,
) -> Result<String> {
    let embedding_model = active.embedding_model.as_deref().context(
        "this profile has no embedding_model configured; set one under [provider] or \
         [profiles.*] in kamui.toml to use semantic search",
    )?;

    let root = project.root();
    let key = project.key();
    let mut seen = std::collections::HashSet::new();
    let mut indexed = 0usize;
    let mut skipped = 0usize;
    let mut chunk_total = 0usize;
    let mut failed: Vec<String> = Vec::new();

    for path in tools::walk(root) {
        let relative = tools::relative_slug(root, &path);
        seen.insert(relative.clone());
        // Binary or otherwise unreadable-as-text files are simply not indexable, same as grep.
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let hash = content_hash(&content);
        if database.indexed_file_is_current(&key, &relative, &hash, embedding_model)? {
            skipped += 1;
            continue;
        }

        match index_file(
            provider,
            embedding_model,
            database,
            &key,
            &relative,
            &content,
            &hash,
        )
        .await
        {
            Ok(chunks) => {
                chunk_total += chunks;
                indexed += 1;
            }
            Err(error) => {
                // A file the embedding endpoint refuses is skipped, not fatal. Aborting left the
                // project permanently unindexable: a re-run skips the files already stored and
                // reaches the same bad one again.
                failed.push(format!("{relative} ({error:#})"));
                // Nothing succeeding after several tries is systemic (endpoint down, bad key),
                // and walking the rest of the project would only produce one error per file.
                if indexed == 0 && failed.len() >= SYSTEMIC_INDEX_FAILURES {
                    return Err(error).with_context(|| {
                        format!("indexing failed on the first {} file(s)", failed.len())
                    });
                }
            }
        }
    }

    // Anything indexed before but not seen on this walk no longer exists (or is now ignored).
    let mut removed = 0usize;
    for file in database.indexed_files(&key)? {
        if !seen.contains(&file.path) {
            database.delete_chunks_for_path(&key, &file.path)?;
            database.delete_indexed_file(&key, &file.path)?;
            removed += 1;
        }
    }

    let mut summary = format!(
        "Indexed {indexed} file(s) ({chunk_total} new chunks), skipped {skipped} unchanged, \
         removed {removed} deleted. {} chunk(s) total.",
        database.chunk_count(&key)?
    );
    if !failed.is_empty() {
        // Named, not just counted: a file missing from the index is one `search_code` can
        // never find, and knowing which one is what makes that fixable.
        summary.push_str(&format!(
            " Could not index {}: {}",
            failed.len(),
            failed.join("; ")
        ));
    }
    Ok(summary)
}

/// Chunk, embed, and store one file's content for a project, replacing whatever was indexed for
/// that path before, and record the hash so a later run can skip it while it stays unchanged.
/// Returns how many chunks were written. Shared by `/index` and the post-turn refresh so both
/// store chunks the same way.
#[allow(clippy::too_many_arguments)]
async fn index_file(
    provider: &dyn Provider,
    embedding_model: &str,
    database: &Database,
    project_key: &str,
    relative: &str,
    content: &str,
    hash: &str,
) -> Result<usize> {
    let chunks = tools::chunk_text(content);
    let mut prepared = Vec::new();
    for batch in chunks.chunks(EMBEDDING_BATCH_SIZE) {
        let texts: Vec<String> = batch.iter().map(|(_, _, text)| text.clone()).collect();
        let embeddings = provider
            .embed(embedding_model, texts)
            .await
            .with_context(|| format!("failed to embed {relative}"))?;
        if embeddings.len() != batch.len() {
            anyhow::bail!(
                "embedding provider returned {} vector(s) for {} chunk(s) in {relative}",
                embeddings.len(),
                batch.len()
            );
        }
        for ((start, end, text), embedding) in batch.iter().cloned().zip(embeddings) {
            prepared.push(storage::NewCodeChunk {
                start_line: start,
                end_line: end,
                content: text,
                embedding,
            });
        }
    }
    let written = prepared.len();
    database.replace_file_index(project_key, relative, hash, embedding_model, &prepared)?;
    Ok(written)
}

/// Re-embed the files a completed turn edited, so `search_code` cannot answer with text that no
/// longer exists at the lines it reports.
///
/// Refresh only: a file the turn *created* is not added to the index on Kamui's own initiative.
/// Stale content in an already-indexed file is actively misleading, while a file missing from the
/// index is merely incomplete — and the startup staleness hint already surfaces it — so only the
/// former justifies spending the user's embedding budget unasked. Having run `/index` at least
/// once is what opts a project in; without that, or without an `embedding_model`, this does
/// nothing and costs nothing.
///
/// Best-effort by contract: callers report a failure and carry on, since the turn's real work is
/// already done and persisted, and `/index` can always rebuild.
async fn refresh_index_for_paths(
    provider: &dyn Provider,
    active: &Profile,
    database: &Database,
    project: &ProjectContext,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<usize> {
    let Some(embedding_model) = active.embedding_model.as_deref() else {
        return Ok(0);
    };
    let key = project.key();
    let root = project.root();
    let mut refreshed = 0;
    for path in paths {
        let relative = tools::relative_slug(root, &path);
        let Some(stored_hash) = database.indexed_file_hash(&key, &relative)? else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            // Gone, or no longer readable as text: dropping its chunks is cheaper than embedding
            // and safer than serving text nothing can be checked against.
            database.delete_chunks_for_path(&key, &relative)?;
            database.delete_indexed_file(&key, &relative)?;
            refreshed += 1;
            continue;
        };
        // A turn can edit a file and end up back at the indexed content — patched then reverted,
        // or an edit that cancels out. Nothing to re-embed then.
        let hash = content_hash(&content);
        if hash == stored_hash
            && database.indexed_file_is_current(&key, &relative, &hash, embedding_model)?
        {
            continue;
        }
        index_file(
            provider,
            embedding_model,
            database,
            &key,
            &relative,
            &content,
            &hash,
        )
        .await?;
        refreshed += 1;
    }
    Ok(refreshed)
}

/// Report the outcome of a post-turn index refresh on one line, saying nothing when there was
/// nothing to refresh.
/// What the post-turn index refresh has to say, and whether it is a failure. Separated from
/// the printing because the two callers print differently: the interactive loop must go
/// through the UI (this runs after every editing turn, and it used to scribble straight
/// over the frame ratatui owns), while `-p` is plain stdout by contract.
fn index_refresh_message(outcome: Result<usize>) -> Option<(String, bool)> {
    match outcome {
        Ok(0) => None,
        Ok(count) => Some((
            format!("refreshed {count} file(s) in the code index"),
            false,
        )),
        // Never silent: a failed refresh leaves `search_code` quoting code that is gone.
        Err(error) => Some((
            format!("index refresh failed: {error:#} — /index rebuilds it"),
            true,
        )),
    }
}

fn report_index_refresh(chat_ui: &mut ChatUi, outcome: Result<usize>) {
    if let Some((text, failed)) = index_refresh_message(outcome) {
        let _ = if failed {
            chat_ui.error(&text)
        } else {
            chat_ui.notice(&text)
        };
    }
}

/// How far the stored index has drifted from what is on disk, as counts of files that changed,
/// appeared, or disappeared since they were last indexed.
#[derive(Default, PartialEq, Eq, Debug)]
struct IndexStaleness {
    changed: usize,
    added: usize,
    removed: usize,
}

impl IndexStaleness {
    fn is_fresh(&self) -> bool {
        *self == Self::default()
    }

    /// A one-line summary listing only the non-zero counts, e.g. `3 changed, 1 new`.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.changed > 0 {
            parts.push(format!("{} changed", self.changed));
        }
        if self.added > 0 {
            parts.push(format!("{} new", self.added));
        }
        if self.removed > 0 {
            parts.push(format!("{} removed", self.removed));
        }
        parts.join(", ")
    }
}

/// Compare the stored index against the project tree, returning `None` when this project has never
/// been indexed (so nothing is reported to a user who has not opted into semantic search).
///
/// Deliberately cheap: it walks the tree and compares each file's mtime against when it was
/// indexed, rather than reading and hashing every file the way `/index` does. That makes it a hint
/// — a checkout can bump an mtime without changing content — but it costs no file reads, no
/// network, and no embedding spend at startup, and `/index` still does the authoritative hash
/// comparison before re-embedding anything.
fn index_staleness(
    database: &Database,
    project: &ProjectContext,
) -> Result<Option<IndexStaleness>> {
    let indexed: HashMap<String, i64> = database
        .indexed_files(&project.key())?
        .into_iter()
        .map(|file| (file.path, file.indexed_at))
        .collect();
    if indexed.is_empty() {
        return Ok(None);
    }

    let root = project.root();
    let mut staleness = IndexStaleness::default();
    let mut seen = HashSet::new();
    for path in tools::walk(root) {
        let relative = tools::relative_slug(root, &path);
        match indexed.get(&relative) {
            Some(indexed_at) => {
                if modified_at(&path).is_some_and(|modified| modified > *indexed_at) {
                    staleness.changed += 1;
                }
            }
            None => staleness.added += 1,
        }
        seen.insert(relative);
    }
    staleness.removed = indexed.keys().filter(|path| !seen.contains(*path)).count();

    Ok(Some(staleness))
}

/// A file's modification time as a Unix timestamp, or `None` when the platform or filesystem does
/// not report one — treated as "unchanged" rather than guessed at.
fn modified_at(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs() as i64)
}

/// A fast, non-cryptographic change-detection hash — good enough to decide whether a file needs
/// re-embedding, not a security primitive.
fn content_hash(content: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

#[derive(serde::Deserialize)]
struct SearchCodeArguments {
    query: String,
}

/// How many of the highest-scoring chunks `search_code` returns.
const SEARCH_CODE_RESULTS: usize = 8;

/// Dispatch `search_code`, converting any failure to an `Error: ...` string (same convention as
/// `ToolRegistry::dispatch`) so a bad query or a missing index fails the tool call, not the turn.
async fn dispatch_search_code(
    provider: &dyn Provider,
    embedding_model: &str,
    database: &Database,
    project: &ProjectContext,
    arguments: &str,
) -> String {
    match run_search_code(provider, embedding_model, database, project, arguments).await {
        Ok(output) => output,
        Err(error) => format!("Error: {error:#}"),
    }
}

async fn run_search_code(
    provider: &dyn Provider,
    embedding_model: &str,
    database: &Database,
    project: &ProjectContext,
    arguments: &str,
) -> Result<String> {
    let arguments: SearchCodeArguments = serde_json::from_str(arguments)
        .context("search_code requires a 'query' string argument")?;
    if arguments.query.trim().is_empty() {
        anyhow::bail!("search_code requires a non-empty 'query' argument");
    }
    if database.chunk_count(&project.key())? == 0 {
        anyhow::bail!("no code index found for this project; run /index first");
    }

    let mut query_embedding = provider
        .embed(embedding_model, vec![arguments.query.clone()])
        .await
        .context("failed to embed the query")?;
    let query_vector = query_embedding
        .pop()
        .context("provider returned no embedding for the query")?;
    let buckets = storage::lsh_probe_buckets(storage::embedding_signature(&query_vector));
    let chunks = database.candidate_chunks(&project.key(), &arguments.query, &buckets)?;
    if chunks.is_empty() {
        anyhow::bail!("no code index found for this project; run /index first");
    }

    let mut scored: Vec<(f32, storage::CodeChunk)> = chunks
        .into_iter()
        .map(|chunk| (cosine_similarity(&query_vector, &chunk.embedding), chunk))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(SEARCH_CODE_RESULTS);

    Ok(scored
        .into_iter()
        .map(|(score, chunk)| {
            format!(
                "{}:{}-{} (score={score:.2})\n{}",
                chunk.path, chunk.start_line, chunk.end_line, chunk.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n"))
}

/// Standard cosine similarity, in `[-1.0, 1.0]` for non-zero vectors (`0.0` for a mismatched or
/// zero vector, which should not occur for embeddings from the same model but is handled rather
/// than panicking on a division by zero).
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Total bytes of stored memory content allowed before `remember` refuses to add more, keeping
/// the system prompt (which carries every entry on every request) from growing unbounded.
const MAX_MEMORY_BYTES: i64 = 4 * 1024;

/// Build the frozen request head: one message per stable block — the base system prompt (with
/// project instructions) and the eager skill list. Only an explicit user action changes either,
/// and the callers that perform one reset `head_messages`, so between those the head is the same
/// bytes every turn.
///
/// Memory and the rolling summary are deliberately *not* here. Both change on their own — a
/// `remember` call, a compaction — and anything in the head sits ahead of the whole conversation,
/// so putting them here would mean one of those re-read every earlier turn at full price. They
/// ride behind the history instead, as `cache::volatile_tail`.
fn build_head_messages(base_system: String, skills_eager: Option<&str>) -> Vec<Message> {
    let mut head = vec![Message::system(base_system)];
    if let Some(skills) = skills_eager
        && !skills.is_empty()
    {
        head.push(Message::system(skills.to_string()));
    }
    head
}

/// Render every remembered fact as a system-prompt block, or an empty string when there is
/// nothing remembered yet (so callers can skip adding an empty section).
fn render_memory_snapshot(entries: &[storage::MemoryEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut text =
        "Remembered facts about the user (persist across sessions and projects):".to_string();
    for entry in entries {
        text.push_str("\n- ");
        text.push_str(&entry.content);
    }
    text
}

fn is_memory_tool(name: &str) -> bool {
    matches!(
        name,
        tools::REMEMBER_TOOL | tools::UPDATE_MEMORY_TOOL | tools::FORGET_TOOL
    )
}

// --- Plan Mode (ticket #9) ---

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlanStatus {
    Pending,
    Approved,
}

#[derive(Debug, Clone)]
struct PlanModeState {
    status: PlanStatus,
    plan_json: Option<String>,
}

fn looks_like_multi_step(input: &str) -> bool {
    // Heuristic: ≥3 bullet/numbered lines or explicit "step" mentions.
    let lines: Vec<&str> = input.lines().collect();
    let mut hits = 0usize;
    for line in &lines {
        let t = line.trim();
        if t.starts_with("- ")
            || t.starts_with("* ")
            || t.starts_with("- [")
            || (t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains(". "))
        {
            hits += 1;
        }
        if t.to_ascii_lowercase().contains("step") {
            hits += 1;
        }
    }
    hits >= 3
}

fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        tools::PATCH_FILE_TOOL | "run_command" | "command_status" | "stop_command"
    )
}

#[derive(serde::Deserialize)]
struct FactArguments {
    fact: String,
}

#[derive(serde::Deserialize)]
struct UpdateMemoryArguments {
    matching: String,
    fact: String,
}

#[derive(serde::Deserialize)]
struct ForgetArguments {
    matching: String,
}

/// Dispatch one of the memory pseudo-tools (`remember`/`update_memory`/`forget`), all of which
/// need `Database` directly rather than going through `ToolRegistry`. Synchronous and infallible
/// at the call site (it always returns a tool-result string, `Error: ...` on failure) so it slots
/// into the same position `tools.dispatch(call).await` would.
fn dispatch_memory_tool(database: &Database, name: &str, arguments: &str) -> String {
    let result = (|| -> Result<String> {
        match name {
            tools::REMEMBER_TOOL => {
                let arguments: FactArguments = serde_json::from_str(arguments)
                    .context("tool arguments were not valid JSON")?;
                let fact = arguments.fact.trim();
                if fact.is_empty() {
                    anyhow::bail!("remember requires a non-empty 'fact' argument");
                }
                let existing = database.total_memory_bytes()?;
                if existing + fact.len() as i64 > MAX_MEMORY_BYTES {
                    anyhow::bail!(
                        "memory is full ({existing}/{MAX_MEMORY_BYTES} bytes); use update_memory \
                         or forget to make room before adding more"
                    );
                }
                database.remember(fact)?;
                Ok(format!("remembered: {fact}"))
            }
            tools::UPDATE_MEMORY_TOOL => {
                let arguments: UpdateMemoryArguments = serde_json::from_str(arguments)
                    .context("tool arguments were not valid JSON")?;
                let matching = arguments.matching.trim();
                let fact = arguments.fact.trim();
                if matching.is_empty() || fact.is_empty() {
                    anyhow::bail!(
                        "update_memory requires non-empty 'matching' and 'fact' arguments"
                    );
                }
                if database.update_memory(matching, fact)? {
                    Ok(format!("updated memory matching \"{matching}\" to: {fact}"))
                } else {
                    anyhow::bail!(
                        "no single remembered fact matches \"{matching}\"; it may not exist, or \
                         the substring matches more than one entry"
                    )
                }
            }
            tools::FORGET_TOOL => {
                let arguments: ForgetArguments = serde_json::from_str(arguments)
                    .context("tool arguments were not valid JSON")?;
                let matching = arguments.matching.trim();
                if matching.is_empty() {
                    anyhow::bail!("forget requires a non-empty 'matching' argument");
                }
                if database.forget(matching)? {
                    Ok(format!("forgot the fact matching \"{matching}\""))
                } else {
                    anyhow::bail!(
                        "no single remembered fact matches \"{matching}\"; it may not exist, or \
                         the substring matches more than one entry"
                    )
                }
            }
            _ => unreachable!("is_memory_tool already filtered to a known name"),
        }
    })();
    result.unwrap_or_else(|error| format!("Error: {error:#}"))
}

/// Collapsed preview: head/tail trimmed, expand hint — box truncates rows to width.
fn preview_output(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let preview = if total <= 20 {
        let clipped = lines.join("\n");
        let mut out: String = clipped.chars().take(1000).collect();
        if clipped.chars().count() > 1000 {
            out.push_str(" … (truncated, collapsed)");
        }
        out
    } else {
        let head = lines[..10].join("\n");
        let tail = lines[total - 10..].join("\n");
        let hidden = total - 20;
        format!("{head}\n… ({hidden} lines hidden, collapsed) …\n{tail}")
    };
    let mut out: String = preview.chars().take(1000).collect();
    if preview.chars().count() > 1000 {
        out.push('…');
    }
    out
}

/// Build a single-line preview of `content` centered on the first match of `query`.
fn make_snippet(content: &str, query: &str) -> String {
    const WINDOW: usize = 80;
    const LEAD: usize = 24;

    let normalized: Vec<char> = content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect();
    // ASCII-fold both sides so indexing stays aligned one-to-one with `normalized`.
    let haystack: Vec<char> = normalized.iter().map(|c| c.to_ascii_lowercase()).collect();
    let needle: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();

    let start = match haystack
        .windows(needle.len().max(1))
        .position(|window| window == needle.as_slice())
    {
        Some(position) => position.saturating_sub(LEAD),
        None => 0,
    };

    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.extend(normalized[start..].iter().take(WINDOW));
    if normalized.len() - start > WINDOW {
        snippet.push('…');
    }
    snippet
}

fn format_timestamp(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|value| value.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown time".to_string())
}

pub(crate) fn print_help(out: &mut String) {
    let _ = writeln!(
        out,
        "!<command>        Run a shell command directly (no model involvement)"
    );
    let _ = writeln!(
        out,
        "/mode [name]      build / auto / plan \u{2014} what a turn may do on its own (Tab cycles)"
    );
    let _ = writeln!(
        out,
        "/plan             Enter Plan Mode (gate mutating tools until plan approved)"
    );
    let _ = writeln!(out, "/skills           List discovered skills");
    let _ = writeln!(out, "/warnings         Hide or show warning messages");
    let _ = writeln!(out, "/new              Start a new session");
    let _ = writeln!(out, "/sessions         List saved sessions");
    let _ = writeln!(out, "/resume <id>      Resume a session");
    let _ = writeln!(
        out,
        "/model [name]     List provider profiles, or switch to one"
    );
    let _ = writeln!(out, "/rename <id> <t>  Rename a session");
    let _ = writeln!(out, "/search <text>    Search saved messages");
    let _ = writeln!(
        out,
        "/compact          Summarize older messages to free up context"
    );
    let _ = writeln!(
        out,
        "/undo             Revert the files patched by the last turn"
    );
    let _ = writeln!(
        out,
        "/jobs             List session and persistent scheduled jobs"
    );
    let _ = writeln!(
        out,
        "/index            Rebuild the semantic-search index (needs embedding_model)"
    );
    let _ = writeln!(out, "/commands         List your own prompt commands");
    let _ = writeln!(out, "/delete <id>      Delete a session");
    let _ = writeln!(out, "/stats            Show current session usage");
    let _ = writeln!(
        out,
        "/usage            Show token usage by day and month, across all sessions"
    );
    let _ = writeln!(out, "/status           Show project and connection status");
    let _ = writeln!(
        out,
        "/mcp              List MCP servers and their tool counts"
    );
    let _ = writeln!(
        out,
        "/memory           List facts Kamui remembers across sessions and projects"
    );
    let _ = writeln!(
        out,
        "/forget <text>    Forget one remembered fact, or /forget all"
    );
    let _ = writeln!(out, "/exit             Save and quit\n");
}

/// How much history `/usage` reports before summarizing everything into the lifetime total.
const USAGE_REPORT_DAYS: usize = 14;
const USAGE_REPORT_MONTHS: usize = 6;

/// Token usage across every session, by day and by month. Unlike `/stats`, which is scoped to the
/// active session, this answers "how much have I spent lately" over the whole database.
fn print_usage_report(database: &Database, prices: &Prices, out: &mut String) -> Result<()> {
    let daily = database.usage_by_day(USAGE_REPORT_DAYS)?;
    if daily.is_empty() {
        let _ = writeln!(out, "No usage recorded yet.\n");
        return Ok(());
    }

    // Per-model sums exist only to be priced, so they are not even queried without prices; the
    // report then runs exactly the queries, and prints exactly the columns, that it always has.
    let priced = !prices.is_empty();
    let daily_models = if priced {
        database.usage_model_tokens_by_day()?
    } else {
        HashMap::new()
    };
    let monthly_models = if priced {
        database.usage_model_tokens_by_month()?
    } else {
        HashMap::new()
    };

    let mut unpriced = false;
    #[allow(clippy::too_many_arguments)]
    fn row(
        out: &mut String,
        unpriced: &mut bool,
        prices: &Prices,
        period: &storage::UsagePeriod,
        tokens: &[storage::ModelTokens],
    ) {
        let cell = cost_cell(prices, model_token_rows(tokens));
        if let Some((_, has_unpriced)) = &cell {
            *unpriced |= has_unpriced;
        }
        let _ = writeln!(
            out,
            "{}",
            usage_row(period, cell.as_ref().map(|(cost, _)| cost.as_str()))
        );
    }

    let _ = writeln!(out, "\nLast {USAGE_REPORT_DAYS} days");
    for period in &daily {
        row(
            out,
            &mut unpriced,
            prices,
            period,
            tokens_for(&daily_models, &period.period),
        );
    }

    let monthly = database.usage_by_month(USAGE_REPORT_MONTHS)?;
    if monthly.len() > 1 {
        let _ = writeln!(out, "\nBy month");
        for period in &monthly {
            row(
                out,
                &mut unpriced,
                prices,
                period,
                tokens_for(&monthly_models, &period.period),
            );
        }
    }

    let total = database.usage_total()?;
    let total_models = if priced {
        database.usage_model_tokens()?
    } else {
        Vec::new()
    };
    let _ = writeln!(out, "\nAll time");
    row(out, &mut unpriced, prices, &total, &total_models);
    let _ = writeln!(
        out,
        "\nRequests count chat turns only; tokens include title generation."
    );
    if unpriced {
        let _ = writeln!(out, "{UNPRICED_NOTE}");
    }
    let _ = writeln!(out,);
    Ok(())
}

fn print_skills(
    library: &crate::skills::SkillLibrary,
    disabled: &std::collections::HashSet<String>,
    out: &mut String,
) {
    if library.list().is_empty() {
        let _ = writeln!(out, "No skills discovered.");
        let _ = writeln!(out, "Create a skill as a folder with SKILL.md:");
        let _ = writeln!(
            out,
            "  <project>/.kamui/skills/my-skill/SKILL.md  ->  /my-skill  (project)"
        );
        let _ = writeln!(
            out,
            "  <config dir>/kamui/skills/my-skill/SKILL.md ->  /my-skill  (global)"
        );
        let _ = writeln!(
            out,
            "Compat: .agents/skills is also scanned. Use /skill:<name> if a skill collides with a built-in or command.\n"
        );
        for warning in library.warnings() {
            let _ = writeln!(out, "  warning: {warning}");
        }
        if !library.warnings().is_empty() {
            let _ = writeln!(out,);
        }
        return;
    }
    let _ = writeln!(
        out,
        "Skills (eager: name+description in prompt, body on /<skill> or /skill:<name>):"
    );
    let term_w = Term::stdout().size().1 as usize;
    let max_desc = term_w.saturating_sub(40).clamp(20, 60);
    for skill in library.list() {
        let state = if disabled.contains(&skill.name) {
            "[disabled]"
        } else {
            "[enabled] "
        };
        let tools_hint = skill
            .allowed_tools
            .as_deref()
            .map(|tools| format!(" [tools: {tools}]"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  {state} /{:<18} {:<18} {}{tools_hint}",
            skill.name,
            skill.source.badge(),
            crate::tui::truncate_chars(&skill.description, max_desc)
        );
    }
    if !library.warnings().is_empty() {
        let _ = writeln!(out, "\nWarnings (invalid skills skipped):");
        for warning in library.warnings() {
            let _ = writeln!(out, "  - {warning}");
        }
    }
    let _ = writeln!(
        out,
        "\nInvoke with /<skill-name> or /skill:<name> (namespaced, wins over collisions).\n"
    );
}

/// List the user's own prompt commands, or explain where to put one when there are none yet.
fn print_commands(library: &commands::CommandLibrary, out: &mut String) {
    if library.is_empty() {
        let _ = writeln!(out, "No custom commands yet.");
        let _ = writeln!(out, "Add a markdown file to create one:");
        let _ = writeln!(
            out,
            "  <project>/.kamui/commands/review.md  ->  /review   (this project only)"
        );
        let _ = writeln!(
            out,
            "  <config dir>/kamui/commands/review.md ->  /review   (every project)\n"
        );
        return;
    }
    let _ = writeln!(out, "Your commands:");
    for command in library.list() {
        let description = command.description.as_deref().unwrap_or("");
        let _ = writeln!(
            out,
            "  /{:<18} {:<9} {description}",
            command.name,
            command.source.label()
        );
    }
    let _ = writeln!(
        out,
        "\nInvoke one with /<name>; anything after it is appended to the prompt.\n"
    );
}

struct GitStatus {
    branch: String,
    changed: usize,
}

/// The `/mcp` listing: every configured server with its live tool count, plus the built-in
/// tools that are always present. Unlike the one-line sidebar value, this is the full report.
fn print_mcp(statuses: &[ConnectionStatus], tools: &ToolRegistry) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Built-in tools: {} (read_file, list_directory, grep, glob, run_command, patch_file, ...)",
        tools.len()
    );
    if statuses.is_empty() {
        out.push_str("\nNo MCP servers configured. Add [mcp.<name>] to kamui.toml.\n");
        return out;
    }
    let _ = writeln!(out, "\nMCP servers:");
    let mut total = 0usize;
    for server in statuses {
        if server.disabled {
            let _ = writeln!(
                out,
                "  - {}  disabled (run /mcp toggle {})",
                server.name, server.name
            );
            continue;
        }
        match &server.error {
            Some(error) => {
                let _ = writeln!(out, "  - {}  unavailable", server.name);
                let _ = writeln!(out, "      {error}");
            }
            None => {
                let _ = writeln!(
                    out,
                    "  - {}  {} tool(s){}",
                    server.name,
                    server.tool_count,
                    if server.trusted { " · trusted" } else { "" }
                );
                total += server.tool_count;
            }
        }
    }
    let _ = writeln!(out, "\nTotal MCP tools: {total}");
    out
}

/// Set an `[mcp.<name>]` server's `enabled` flag to an explicit value and mirror it in the
/// in-memory status list. Connections are made at startup, so the caller tells the user to
/// restart.
fn set_mcp_enabled(
    config: &Config,
    statuses: &mut [ConnectionStatus],
    name: &str,
    enabled: bool,
) -> Result<()> {
    config
        .mcp_servers
        .iter()
        .find(|s| s.name == name)
        .with_context(|| format!("no MCP server named '{name}'"))?;
    let path = crate::config::global_config_path()?;
    crate::config::set_mcp_enabled(&path, name, enabled)?;
    if let Some(status) = statuses.iter_mut().find(|s| s.name == name) {
        status.disabled = !enabled;
    }
    Ok(())
}

/// Flip an `[mcp.<name>]` server's `enabled` flag in the global config and mirror it in the
/// in-memory status list. Connections are made at startup, so the caller tells the user to
/// restart. Returns the new enabled state.
fn toggle_mcp(config: &Config, statuses: &mut [ConnectionStatus], name: &str) -> Result<bool> {
    let server = config
        .mcp_servers
        .iter()
        .find(|s| s.name == name)
        .with_context(|| format!("no MCP server named '{name}'"))?;
    let now_enabled = !server.enabled;
    set_mcp_enabled(config, statuses, name, now_enabled)?;
    Ok(now_enabled)
}

fn print_status(
    project: &ProjectContext,
    active: &Profile,
    tools: &ToolRegistry,
    mcp_statuses: &[ConnectionStatus],
    allow_commands: &[String],
) {
    let git = git_status(project.root());
    let project_name = project
        .root()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");

    println!(
        "╭─ Kamui v{} ─────────────────────────",
        env!("CARGO_PKG_VERSION")
    );
    println!(
        "│ Project  {project_name}  ({})",
        display_path(project.root())
    );
    match git {
        Some(git) => println!("│ Git      {}  ·  {} changed", git.branch, git.changed),
        None => println!("│ Git      not a repository"),
    }
    println!("│ Model    {}  ({})", active.model, active.name);
    println!("│ Tools    {} available", tools.len());
    if !allow_commands.is_empty() {
        println!("│ Allow    {}", allow_commands.join(", "));
    }
    if mcp_statuses.is_empty() {
        println!("│ MCP      none configured");
    } else {
        for server in mcp_statuses {
            match &server.error {
                Some(_) => println!("│ MCP      {}  unavailable", server.name),
                None => println!(
                    "│ MCP      {}  connected · {} tools{}",
                    server.name,
                    server.tool_count,
                    if server.trusted { " · trusted" } else { "" }
                ),
            }
        }
    }
    if let Some(name) = project.instruction_name() {
        println!("│ Rules    {name}");
    }
    println!("╰──────────────────────────────────────\n");
}

fn git_status(root: &Path) -> Option<GitStatus> {
    let branch = Command::new("git")
        .current_dir(root)
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if !branch.status.success() {
        return None;
    }
    let mut branch = String::from_utf8(branch.stdout).ok()?.trim().to_string();
    if branch.is_empty() {
        let head = Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()?;
        if !head.status.success() {
            return None;
        }
        branch = format!("detached@{}", String::from_utf8(head.stdout).ok()?.trim());
    }

    let status = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    Some(GitStatus {
        branch,
        changed: String::from_utf8(status.stdout).ok()?.lines().count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::ModelPrice;
    use std::fs;
    use uuid::Uuid;

    fn summary(id: &str, title: &str) -> crate::storage::SessionSummary {
        crate::storage::SessionSummary {
            id: id.to_string(),
            title: title.to_string(),
            message_count: 2,
            total_tokens: 100,
            updated_at: 0,
        }
    }

    #[test]
    fn tui_resume_replays_a_tail_of_stored_messages() {
        assert_eq!(0usize.saturating_sub(RESUME_REPLAY_MESSAGES), 0);
        assert_eq!(4usize.saturating_sub(RESUME_REPLAY_MESSAGES), 0);
        assert_eq!(
            14usize.saturating_sub(RESUME_REPLAY_MESSAGES),
            4,
            "older turns stay in model context but off the screen"
        );
    }

    fn image(media: &str, decoded_bytes: usize) -> crate::provider::ImageAttachment {
        crate::provider::ImageAttachment {
            media_type: media.to_string(),
            // Base64 spends four characters per three bytes.
            data: "A".repeat(decoded_bytes / 3 * 4),
        }
    }

    /// An expansion carrying the given images and file counts.
    fn expanded(
        images: Vec<crate::provider::ImageAttachment>,
        attached: usize,
        omitted: usize,
    ) -> crate::context::Expanded {
        crate::context::Expanded {
            text: String::new(),
            images,
            attached_files: attached,
            omitted_files: omitted,
        }
    }

    #[test]
    fn stats_put_the_context_line_in_the_report_not_on_stdout() {
        // Every other line of the report is buffered into `out`; this one used `print!`, so it
        // was missing from `/stats` in the transcript and went straight through the frame
        // ratatui owns instead. It is also the line most worth reading.
        let database = Database::open_in_memory_for_tests();
        let session = database.create_session("test", "m").unwrap();
        database
            .save_turn(
                &session.id,
                &[Message::user("hi"), Message::assistant("hello")],
                &Usage {
                    prompt_tokens: 400,
                    completion_tokens: 50,
                    total_tokens: 450,
                    cached_tokens: 0,
                },
                "m",
                "stop",
            )
            .unwrap();

        let mut out = String::new();
        print_stats(
            &database,
            &session,
            Some(1000),
            &Prices::default(),
            &mut out,
        )
        .unwrap();

        assert!(
            out.contains("Last context:"),
            "the line is in the report: {out:?}"
        );
        assert!(out.contains("400/1000"), "with the numbers: {out:?}");
        assert!(out.contains("40.0%"), "and the percentage: {out:?}");
    }

    #[test]
    fn stats_omit_the_context_line_when_no_window_is_configured() {
        let database = Database::open_in_memory_for_tests();
        let session = database.create_session("test", "m").unwrap();
        database
            .save_turn(
                &session.id,
                &[Message::user("hi"), Message::assistant("hello")],
                &Usage {
                    prompt_tokens: 400,
                    completion_tokens: 50,
                    total_tokens: 450,
                    cached_tokens: 0,
                },
                "m",
                "stop",
            )
            .unwrap();

        let mut out = String::new();
        print_stats(&database, &session, None, &Prices::default(), &mut out).unwrap();

        assert!(
            !out.contains("Last context:"),
            "nothing to measure against: {out:?}"
        );
    }

    #[test]
    fn a_text_only_prompt_says_nothing_about_attachments() {
        assert!(describe_attachments(&expanded(Vec::new(), 0, 0)).is_none());
    }

    #[test]
    fn files_left_out_of_a_directory_are_reported_too() {
        // `@src` delivering twelve of fifty files answered from partial context and said so only
        // inside the prompt sent to the model.
        let note = describe_attachments(&expanded(Vec::new(), 12, 38)).expect("a note");
        assert!(note.contains("12 file(s) attached"), "{note}");
        assert!(note.contains("38 left out"), "{note}");
        assert!(
            note.contains("context budget"),
            "the reason is given: {note}"
        );

        // A directory that fitted entirely says nothing about omissions.
        let full = describe_attachments(&expanded(Vec::new(), 4, 0)).expect("a note");
        assert!(full.contains("4 file(s) attached"), "{full}");
        assert!(!full.contains("left out"), "{full}");
    }

    #[test]
    fn attached_images_are_reported_with_type_and_size() {
        // `@clipboard` and `@shot.png` look like plain text in the transcript, so the only way
        // to know an image was picked up is for the attachment to announce itself.
        let note = describe_attachments(&expanded(vec![image("image/png", 210 * 1024)], 0, 0))
            .expect("a note");
        assert!(note.contains("1 image(s)"), "{note}");
        assert!(note.contains("image/png"), "{note}");
        assert!(note.contains("KiB"), "{note}");
        assert!(
            note.contains("vision model"),
            "the requirement is stated: {note}"
        );

        let two = describe_attachments(&expanded(
            vec![image("image/png", 1024), image("image/jpeg", 2048)],
            0,
            0,
        ))
        .expect("a note");
        assert!(two.contains("2 image(s)"), "{two}");
        assert!(two.contains("image/jpeg"), "{two}");
    }

    #[test]
    fn byte_sizes_scale_to_readable_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn session_listing_puts_each_session_on_its_own_row() {
        // Regression: `out!` appends no newline, so the row formatter has to. Without it the
        // whole listing arrived as one unreadable run-on line.
        let rows = format_session_rows(
            &[
                summary("aaaaaaaa-1111", "first"),
                summary("bbbbbbbb-2222", "second"),
                summary("cccccccc-3333", "third"),
            ],
            Some("bbbbbbbb-2222"),
        );
        let lines: Vec<&str> = rows.lines().collect();
        assert_eq!(lines.len(), 3, "one row per session: {rows:?}");
        assert!(
            lines[0].starts_with("  "),
            "inactive rows are unmarked: {:?}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("* "),
            "the active session is marked: {:?}",
            lines[1]
        );
        assert!(lines[2].contains("third"), "{:?}", lines[2]);
    }

    #[test]
    fn search_listing_puts_each_hit_on_its_own_row() {
        let hits = vec![
            crate::storage::SearchHit {
                session_id: "aaaaaaaa-1111".into(),
                title: "a chat".into(),
                role: "user".into(),
                content: "the needle is here".into(),
                created_at: 0,
            },
            crate::storage::SearchHit {
                session_id: "bbbbbbbb-2222".into(),
                title: "another".into(),
                role: "assistant".into(),
                content: "needle again".into(),
                created_at: 0,
            },
        ];
        let rows = format_search_hits(&hits, "needle");
        assert_eq!(rows.lines().count(), 2, "one row per hit: {rows:?}");
        assert!(rows.lines().next().unwrap().contains("You:"), "{rows:?}");
        assert!(
            rows.lines().nth(1).unwrap().contains("Assistant:"),
            "{rows:?}"
        );
    }

    #[test]
    fn memory_listing_puts_each_fact_on_its_own_row() {
        let entries = vec![
            crate::storage::MemoryEntry {
                content: "prefers rust".into(),
            },
            crate::storage::MemoryEntry {
                content: "lives in jakarta".into(),
            },
        ];
        let rows = format_memory_rows(&entries);
        let lines: Vec<&str> = rows.lines().collect();
        assert_eq!(lines[0], "Remembered facts:");
        assert_eq!(lines[1], "- prefers rust");
        assert_eq!(lines[2], "- lives in jakarta");
        assert!(
            rows.contains("/forget"),
            "the removal hint survives: {rows:?}"
        );
    }

    #[test]
    fn every_advertised_command_is_dispatched_somewhere() {
        // The slash menu and the dispatcher drifting apart is what makes a listed command come
        // back as "Unknown command", so hold them together here.
        let source = include_str!("chat.rs");
        for (name, _) in crate::tui::BUILTINS {
            let inline = source.contains(&format!("command == \"/{name}\""));
            let arm = source.contains(&format!("\"/{name}\" =>"));
            let early = source.contains(&format!("input == \"/{name}\""));
            assert!(
                inline || arm || early,
                "/{name} is offered by the slash menu but nothing handles it"
            );
        }
    }

    #[test]
    fn unknown_commands_suggest_the_nearest_builtin() {
        assert_eq!(nearest_command("/sess"), Some("sessions"));
        assert_eq!(nearest_command("/sesions"), Some("sessions"));
        assert_eq!(nearest_command("/stat"), Some("stats"));
        // Two edits away still earns a suggestion: /quit is a common try for /exit.
        assert_eq!(nearest_command("/quit"), Some("exit"));
        assert_eq!(
            nearest_command("/deploy"),
            None,
            "nothing close enough to guess"
        );
        assert_eq!(nearest_command("/"), None);
    }

    #[test]
    fn edit_distance_counts_single_character_edits() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "abd"), 1);
        assert_eq!(edit_distance("ab", "abc"), 1);
    }

    fn status(name: &str, tools: usize, trusted: bool, error: Option<&str>) -> ConnectionStatus {
        ConnectionStatus {
            name: name.to_string(),
            tool_count: tools,
            trusted,
            disabled: false,
            error: error.map(str::to_string),
        }
    }

    fn profile(name: &str, model: &str, tools: bool) -> Profile {
        Profile {
            name: name.to_string(),
            model: model.to_string(),
            base_url: "http://localhost".to_string(),
            api_key: "k".to_string(),
            context_window: None,
            tools,
            embedding_model: None,
            completions_path: None,
            send_session_id: false,
        }
    }

    #[test]
    fn tab_cycles_forward_through_every_mode_and_back() {
        let mut mode = Mode::Build;
        let mut seen = Vec::new();
        for _ in 0..3 {
            mode = mode.next();
            seen.push(mode);
        }
        assert_eq!(
            seen,
            vec![Mode::Auto, Mode::Plan, Mode::Build],
            "the cycle closes"
        );
        // Shift+Tab retraces it exactly.
        assert_eq!(Mode::Build.prev(), Mode::Plan);
        assert_eq!(Mode::Plan.prev(), Mode::Auto);
        assert_eq!(Mode::Auto.prev(), Mode::Build);
    }

    #[test]
    fn modes_are_addressable_by_name() {
        assert_eq!(Mode::parse("plan"), Some(Mode::Plan));
        assert_eq!(
            Mode::parse("  AUTO "),
            Some(Mode::Auto),
            "case and space are forgiven"
        );
        assert_eq!(
            Mode::parse("bypass"),
            Some(Mode::Auto),
            "what it does, not just its name"
        );
        assert_eq!(Mode::parse("normal"), Some(Mode::Build));
        assert_eq!(Mode::parse("yolo"), None);
    }

    #[test]
    fn every_mode_says_what_it_permits() {
        // The label is what the rail shows; the description is what the switch announces, and
        // it has to state the consequence, not just repeat the name.
        for mode in Mode::CYCLE {
            assert!(
                mode.describe().starts_with(mode.label()),
                "{}",
                mode.describe()
            );
            assert!(
                mode.describe().len() > mode.label().len() + 10,
                "{} explains nothing",
                mode.label()
            );
        }
        assert!(Mode::Auto.describe().contains("without asking"));
        assert!(
            Mode::Auto.describe().contains("this session"),
            "the blast radius is stated"
        );
        assert!(Mode::Plan.describe().contains("read-only"));
    }

    #[test]
    fn the_model_row_says_when_a_profile_has_no_tools() {
        // A model offered no tools will invent tool-call syntax in prose, which reads as the
        // agent being broken rather than switched off. The rail has to say which it is.
        assert_eq!(
            model_label(&profile("muse", "orvix/auto", true)),
            "orvix/auto"
        );
        let off = model_label(&profile("ornith", "ornith:latest", false));
        assert!(off.starts_with("ornith:latest"), "{off}");
        assert!(off.contains("no tools"), "{off}");
    }

    #[test]
    fn disabled_tools_are_explained_in_full() {
        assert!(tools_disabled_note(&profile("muse", "orvix/auto", true)).is_none());
        let note = tools_disabled_note(&profile("ornith", "ornith:latest", false)).expect("note");
        assert!(note.contains("ornith"), "names the profile: {note}");
        assert!(note.contains("tools = false"), "names the setting: {note}");
        assert!(note.contains("/model"), "offers a way out: {note}");
    }

    #[test]
    fn mcp_sidebar_lists_every_server_with_its_tool_count() {
        let value = mcp_sidebar_value(&[
            status("mcptools", 79, false, None),
            status("filesystem", 11, true, None),
        ]);
        let rows: Vec<&str> = value.split('\n').collect();
        assert_eq!(rows.len(), 4, "two rows per server: {rows:?}");
        assert!(rows[0].contains("mcptools"), "{rows:?}");
        assert!(rows[1].contains("79 tool(s)"), "{rows:?}");
    }

    #[test]
    fn mcp_sidebar_names_a_server_that_failed_to_start() {
        // Dropping it would make a broken server look like an unconfigured one.
        let value = mcp_sidebar_value(&[status("excel", 0, false, Some("spawn failed"))]);
        assert!(value.contains("excel"), "{value}");
        assert!(value.contains("unavailable"), "{value}");
    }

    #[test]
    fn mcp_sidebar_is_empty_without_servers() {
        assert!(mcp_sidebar_value(&[]).is_empty());
    }

    fn warn(folder: &str, reason: &str) -> String {
        format!("skill '{folder}' in /skills {reason}")
    }

    #[test]
    fn skill_fix_report_counts_a_full_repair() {
        let before = vec![
            warn("alpha", "is invalid: missing name"),
            warn("beta", "is missing SKILL.md"),
        ];
        let report = skill_fix_report(&before, &[]);
        assert!(
            report.summary.contains("all 2 folder(s) now load"),
            "{}",
            report.summary
        );
        assert!(report.banner.is_none(), "banner clears once nothing fails");
    }

    #[test]
    fn skill_fix_report_counts_a_partial_repair() {
        let before = vec![
            warn("alpha", "is invalid: missing name"),
            warn("beta", "is missing SKILL.md"),
            warn("gamma", "is invalid: missing description"),
        ];
        let after = vec![warn("gamma", "is invalid: missing description")];
        let report = skill_fix_report(&before, &after);
        assert!(
            report
                .summary
                .contains("2 of 3 folder(s) fixed, 1 still failing"),
            "{}",
            report.summary
        );
        assert!(
            report.banner.is_some(),
            "banner still names the remaining failure"
        );
    }

    #[test]
    fn a_folder_failing_for_a_new_reason_does_not_count_as_fixed() {
        // The agent rewrote the frontmatter but broke it differently. Comparing whole warning
        // strings would call that a fix; comparing the folder they name does not.
        let before = vec![warn("alpha", "is missing SKILL.md")];
        let after = vec![warn("alpha", "is invalid: missing description")];
        let report = skill_fix_report(&before, &after);
        assert!(
            report.summary.contains("nothing fixed"),
            "{}",
            report.summary
        );
    }

    #[test]
    fn skill_fix_report_flags_folders_broken_by_the_repair() {
        let before = vec![warn("alpha", "is missing SKILL.md")];
        let after = vec![warn("beta", "is invalid: missing name")];
        let report = skill_fix_report(&before, &after);
        assert!(
            report.summary.contains("1 of 1 folder(s) fixed"),
            "{}",
            report.summary
        );
        assert!(
            report.summary.contains("newly broken"),
            "{}",
            report.summary
        );
    }

    fn temporary_directory() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("kamui-status-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }

    /// The point of "optional": with no `[pricing]` in `kamui.toml`, `/stats` and `/usage` print
    /// exactly the lines they printed before cost tracking existed — no cost column, no zeroes —
    /// and never even reach the database for per-model sums.
    #[test]
    fn reports_are_unchanged_when_no_prices_are_configured() {
        let prices = Prices::default();
        let period = storage::UsagePeriod {
            period: "2026-08-20".to_string(),
            request_count: 3,
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
            cached_tokens: 0,
        };
        let stat = storage::ModelStat {
            model: "gpt-5".to_string(),
            request_count: 3,
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
            cached_tokens: 0,
        };

        assert!(cost_cell(&prices, [(Some("gpt-5"), 10, 4)]).is_none());
        assert_eq!(
            usage_row(&period, None),
            "  2026-08-20    3 req          10 in           4 out          14 total"
        );
        assert_eq!(
            model_row(&stat, None),
            "  gpt-5                      3 req        10 in         4 out        14 total"
        );

        // `UnreachableProvider`'s counterpart for storage: the session has usage, but with no
        // prices there is nothing to report and nothing to query.
        let database = Database::open_in_memory_for_tests();
        let session = database.create_session("test", "gpt-5").unwrap();
        database
            .save_turn(
                &session.id,
                &[Message::user("hi")],
                &Usage {
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    total_tokens: 14,
                    cached_tokens: 0,
                },
                "gpt-5",
                "stop",
            )
            .unwrap();

        assert!(
            session_cost(&database, &session.id, &prices)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_configured_price_adds_a_cost_cell_to_a_report_row() {
        let prices = Prices::new(
            None,
            [(
                "gpt-5".to_string(),
                ModelPrice {
                    input_per_million: 1.0,
                    output_per_million: 2.0,
                },
            )],
        )
        .unwrap();
        let period = storage::UsagePeriod {
            period: "2026-08-20".to_string(),
            request_count: 1,
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            total_tokens: 1_500_000,
            cached_tokens: 0,
        };

        let (cost, unpriced) = cost_cell(&prices, [(Some("gpt-5"), 1_000_000, 500_000)]).unwrap();

        assert_eq!(cost, "$2.0000");
        assert!(!unpriced);
        assert!(usage_row(&period, Some(&cost)).ends_with("total       $2.0000"));
    }

    #[test]
    fn a_model_with_usage_but_no_price_is_never_reported_as_free() {
        let prices = Prices::new(
            None,
            [(
                "gpt-5".to_string(),
                ModelPrice {
                    input_per_million: 1.0,
                    output_per_million: 1.0,
                },
            )],
        )
        .unwrap();

        let (cost, unpriced) = cost_cell(&prices, [(Some("codeqwen:latest"), 500, 500)]).unwrap();

        assert_eq!(cost, "unpriced");
        assert!(unpriced);
    }

    /// A session's cost covers every usage kind, so a title generated by a second, unpriced model
    /// marks the total rather than quietly vanishing from it.
    #[test]
    fn session_cost_covers_title_generation_and_marks_an_unpriced_model() {
        let database = Database::open_in_memory_for_tests();
        let session = database.create_session("test", "gpt-5").unwrap();
        let million = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
        };
        database
            .save_turn(
                &session.id,
                &[Message::user("hi")],
                &million,
                "gpt-5",
                "stop",
            )
            .unwrap();
        database
            .save_generated_title(&session.id, "A title", &million, "cheap-titler", "stop")
            .unwrap();
        let prices = Prices::new(
            None,
            [(
                "gpt-5".to_string(),
                ModelPrice {
                    input_per_million: 1.0,
                    output_per_million: 1.0,
                },
            )],
        )
        .unwrap();

        let (cost, unpriced) = session_cost(&database, &session.id, &prices)
            .unwrap()
            .unwrap();

        assert_eq!(cost, "$1.0000+");
        assert!(unpriced);
    }

    #[test]
    fn git_status_reports_branch_and_changed_files() {
        let root = temporary_directory();
        assert!(
            Command::new("git")
                .args(["init", "-b", "status-test"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("one.txt"), "one").unwrap();
        fs::write(root.join("two.txt"), "two").unwrap();

        let status = git_status(&root).unwrap();

        assert_eq!(status.branch, "status-test");
        assert_eq!(status.changed, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_status_returns_none_outside_a_repository() {
        let root = temporary_directory();
        assert!(git_status(&root).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn make_title_truncates_long_input() {
        assert_eq!(make_title("short"), "short");
        let title = make_title(&"a".repeat(45));
        assert_eq!(title.chars().count(), 43); // 40 characters plus "..."
        assert!(title.ends_with("..."));
    }

    #[test]
    fn clean_title_strips_wrapping_punctuation_and_extra_lines() {
        assert_eq!(clean_title("\"Rust Ownership\""), "Rust Ownership");
        assert_eq!(clean_title("Title:\nsecond line"), "Title");
        assert_eq!(clean_title("  spaced.  "), "spaced");
    }

    #[test]
    fn truncate_appends_ellipsis_only_when_needed() {
        assert_eq!(crate::tui::truncate_chars("hello", 10), "hello");
        // The ellipsis lives inside the budget. This used to return six columns for a
        // budget of five, overflowing every column it was sized for.
        assert_eq!(crate::tui::truncate_chars("hello world", 5), "hell…");
    }

    #[test]
    fn preview_output_caps_lines_and_chars() {
        let many_lines = (0..25)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let previewed = preview_output(&many_lines);
        assert!(previewed.contains("lines hidden, collapsed"));
        assert!(previewed.starts_with("line 0"));
        assert!(previewed.contains("line 24"));
        assert_eq!(preview_output("short"), "short");
        assert!(!preview_output(&"x".repeat(1200)).ends_with('x'));
    }

    #[test]
    fn short_id_takes_the_first_eight_characters() {
        assert_eq!(short_id("0123456789"), "01234567");
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn display_path_trims_windows_verbatim_prefixes() {
        use std::path::Path;
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\dev\project")),
            r"C:\Users\dev\project"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\dir")),
            r"\\server\share\dir"
        );
        assert_eq!(
            display_path(Path::new("/home/dev/project")),
            "/home/dev/project"
        );
    }

    #[test]
    fn accumulate_usage_sums_output_and_keeps_the_last_input() {
        let mut total = Usage::default();
        accumulate_usage(
            &mut total,
            &Usage {
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120,
                cached_tokens: 10,
            },
        );
        accumulate_usage(
            &mut total,
            &Usage {
                prompt_tokens: 150,
                completion_tokens: 30,
                total_tokens: 180,
                cached_tokens: 40,
            },
        );

        assert_eq!(total.prompt_tokens, 150); // final round's context size
        assert_eq!(total.completion_tokens, 50); // output summed across rounds
        assert_eq!(total.total_tokens, 200); // last input + all output
        assert_eq!(total.cached_tokens, 40); // last round wins, like prompt_tokens
    }

    #[test]
    fn cache_miss_label_stays_silent_on_first_turn_and_hits() {
        // No previous turn: never report.
        assert_eq!(cache_miss_label(None, 0, 5000, false, false), None);
        assert_eq!(cache_miss_label(None, 3000, 5000, false, false), None);
        // Same-prefix hit: cached holds steady.
        assert_eq!(cache_miss_label(Some(4000), 3950, 5000, false, false), None);
        // Previous turn too small to trust: noise, not a miss.
        assert_eq!(cache_miss_label(Some(500), 0, 5000, false, false), None);
    }

    #[test]
    fn cache_miss_label_names_the_likely_cause() {
        // Fresh prefix after a cached turn: plain miss.
        assert_eq!(
            cache_miss_label(Some(4000), 0, 5000, false, false),
            Some("miss")
        );
        assert_eq!(
            cache_miss_label(Some(4000), 0, 5000, true, false),
            Some("miss (model switch)")
        );
        assert_eq!(
            cache_miss_label(Some(4000), 0, 5000, false, true),
            Some("miss (prefix rebuilt)")
        );
        // Partial drop within the noise floor: still a hit.
        assert_eq!(cache_miss_label(Some(4000), 3500, 5000, false, false), None);
    }

    #[test]
    fn format_duration_switches_units_at_one_second() {
        // Kept here as well as in `terminal`: this is the boundary the two copies had to agree
        // on, and now only one implementation can define it.
        assert_eq!(
            crate::terminal::format_duration(Duration::from_millis(320)),
            "320ms"
        );
        assert_eq!(
            crate::terminal::format_duration(Duration::from_millis(999)),
            "999ms"
        );
        assert_eq!(
            crate::terminal::format_duration(Duration::from_millis(4200)),
            "4.2s"
        );
        assert_eq!(
            crate::terminal::format_duration(Duration::from_secs(1)),
            "1.0s"
        );
    }

    #[test]
    fn make_snippet_centers_on_the_match_without_ellipsis_when_short() {
        let snippet = make_snippet("the quick brown fox jumps", "brown");
        assert!(snippet.contains("brown"));
        assert!(!snippet.contains('…'));
    }

    #[test]
    fn make_snippet_is_case_insensitive_and_normalizes_whitespace() {
        let snippet = make_snippet("Hello\n\n  WORLD   here", "world");
        assert!(snippet.contains("WORLD"));
        assert!(!snippet.contains('\n'));
    }

    #[test]
    fn make_snippet_marks_truncation_with_an_ellipsis() {
        let mut content = "x ".repeat(60); // pushes the match past the leading window
        content.push_str("NEEDLE tail");
        let snippet = make_snippet(&content, "needle");
        assert!(snippet.starts_with('…'));
        assert!(snippet.contains("NEEDLE"));
    }

    fn respond_with(text: &str) -> Option<mpsc::UnboundedReceiver<String>> {
        let (sender, receiver) = mpsc::unbounded_channel();
        sender.send(text.to_string()).unwrap();
        Some(receiver)
    }

    #[tokio::test]
    async fn ask_user_rejects_invalid_json_arguments() {
        let mut rx = respond_with("anything");
        let output = ask_user(&mut rx, false, "not json", None).await.unwrap();
        assert!(output.starts_with("Error:"));
    }

    #[tokio::test]
    async fn ask_user_rejects_a_blank_question() {
        let mut rx = respond_with("anything");
        let output = ask_user(&mut rx, false, r#"{"question":"   "}"#, None)
            .await
            .unwrap();
        assert!(output.starts_with("Error:"));
    }

    #[tokio::test]
    async fn ask_user_returns_free_text_when_no_options_are_offered() {
        let mut rx = respond_with("Tuesday works better");
        let output = ask_user(&mut rx, false, r#"{"question":"When?"}"#, None)
            .await
            .unwrap();
        assert_eq!(output, "Tuesday works better");
    }

    #[tokio::test]
    async fn ask_user_resolves_a_numbered_choice_to_its_option_text() {
        let mut rx = respond_with("2");
        let output = ask_user(
            &mut rx,
            false,
            r#"{"question":"Pick one","options":["red","green","blue"]}"#,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output, "green");
    }

    #[tokio::test]
    async fn ask_user_falls_back_to_raw_text_for_an_out_of_range_number() {
        let mut rx = respond_with("99");
        let output = ask_user(
            &mut rx,
            false,
            r#"{"question":"Pick one","options":["red","green"]}"#,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output, "99");
    }

    #[tokio::test]
    async fn ask_user_accepts_free_text_even_when_options_are_offered() {
        let mut rx = respond_with("actually neither");
        let output = ask_user(
            &mut rx,
            false,
            r#"{"question":"Pick one","options":["red","green"]}"#,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output, "actually neither");
    }

    #[test]
    fn render_memory_snapshot_is_empty_with_no_entries() {
        assert_eq!(render_memory_snapshot(&[]), "");
    }

    #[test]
    fn render_memory_snapshot_lists_every_fact() {
        let entries = vec![
            storage::MemoryEntry {
                content: "Prefers bun over node.".to_owned(),
            },
            storage::MemoryEntry {
                content: "Prefers uv over pip.".to_owned(),
            },
        ];

        let rendered = render_memory_snapshot(&entries);

        assert!(rendered.contains("Prefers bun over node."));
        assert!(rendered.contains("Prefers uv over pip."));
    }

    #[test]
    fn head_messages_skip_empty_blocks() {
        let head = build_head_messages("base".to_string(), None);
        assert_eq!(head.len(), 1);
        assert_eq!(head[0].content, "base");
        assert_eq!(
            build_head_messages("base".to_string(), Some("")).len(),
            1,
            "a project with no skills adds no empty block"
        );
    }

    #[test]
    fn head_messages_keep_blocks_separate() {
        let head = build_head_messages("base".to_string(), Some("Skills:\n- review"));
        assert_eq!(head.len(), 2);
        assert_eq!(head[0].content, "base");
        assert!(head[1].content.contains("review"));
    }

    /// The merged contract: a remembered fact reaches the model on the next turn without moving
    /// one byte of the head, because it rides behind the history rather than in front of it.
    #[test]
    fn remembering_something_leaves_the_frozen_head_alone() {
        let head = build_head_messages("base".to_string(), Some("Skills:\n- review"));
        let before = crate::cache::head_text(&head);

        let history = [Message::user("q"), Message::assistant("a")];
        let turn = crate::cache::turn_messages(
            &head,
            &history,
            crate::cache::volatile_tail("Remembered facts:\n- bun\n- uv", None),
            Message::user("next"),
        );

        assert_eq!(
            crate::cache::head_text(&head),
            before,
            "the head is not rebuilt when memory changes"
        );
        assert!(
            turn[turn.len() - 2].content.contains("uv"),
            "the new fact is in the tail, behind the history"
        );
        assert!(
            !turn[0].content.contains("uv") && !turn[1].content.contains("uv"),
            "and never in the head"
        );
    }

    #[test]
    fn dispatch_memory_tool_remembers_a_fact() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_memory_tool(&database, "remember", r#"{"fact":"Prefers bun."}"#);
        assert!(output.starts_with("remembered:"), "{output}");
        assert_eq!(database.list_memory().unwrap()[0].content, "Prefers bun.");
    }

    #[test]
    fn dispatch_memory_tool_rejects_a_blank_fact() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_memory_tool(&database, "remember", r#"{"fact":"  "}"#);
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[test]
    fn dispatch_memory_tool_refuses_once_the_memory_cap_is_reached() {
        let database = Database::open_in_memory_for_tests();
        database
            .remember(&"x".repeat(MAX_MEMORY_BYTES as usize))
            .unwrap();

        let output = dispatch_memory_tool(&database, "remember", r#"{"fact":"one more"}"#);

        assert!(
            output.starts_with("Error:") && output.contains("full"),
            "{output}"
        );
    }

    #[test]
    fn dispatch_memory_tool_updates_a_matched_fact() {
        let database = Database::open_in_memory_for_tests();
        database.remember("Prefers node over bun.").unwrap();

        let output = dispatch_memory_tool(
            &database,
            "update_memory",
            r#"{"matching":"node over bun","fact":"bun over node."}"#,
        );

        assert!(output.starts_with("updated memory"), "{output}");
        assert_eq!(database.list_memory().unwrap()[0].content, "bun over node.");
    }

    #[test]
    fn dispatch_memory_tool_update_reports_an_error_when_nothing_matches() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_memory_tool(
            &database,
            "update_memory",
            r#"{"matching":"nonexistent","fact":"x"}"#,
        );
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[test]
    fn dispatch_memory_tool_forgets_a_matched_fact() {
        let database = Database::open_in_memory_for_tests();
        database.remember("Prefers bun.").unwrap();

        let output = dispatch_memory_tool(&database, "forget", r#"{"matching":"bun"}"#);

        assert!(output.starts_with("forgot"), "{output}");
        assert!(database.list_memory().unwrap().is_empty());
    }

    #[test]
    fn dispatch_memory_tool_forget_reports_an_error_when_nothing_matches() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_memory_tool(&database, "forget", r#"{"matching":"nonexistent"}"#);
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[test]
    fn snapshot_patch_target_keeps_the_pre_turn_content_on_repeated_edits() {
        // patch_target canonicalizes internally, so the root must be canonical too for the
        // returned keys to compare equal to `root.join(...)`.
        let root = temporary_directory().canonicalize().unwrap();
        fs::write(root.join("a.txt"), "original").unwrap();
        let mut snapshot = HashMap::new();

        snapshot_patch_target(
            &root,
            r#"{"path":"a.txt","old_text":"original","new_text":"first edit"}"#,
            &mut snapshot,
        );
        // A second, later call for the same path must not overwrite the pre-turn baseline.
        snapshot_patch_target(
            &root,
            r#"{"path":"a.txt","old_text":"first edit","new_text":"second edit"}"#,
            &mut snapshot,
        );

        assert_eq!(
            snapshot.get(&root.join("a.txt")).unwrap().as_deref(),
            Some("original")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_patch_target_records_none_for_a_new_file() {
        let root = temporary_directory().canonicalize().unwrap();
        let mut snapshot = HashMap::new();

        snapshot_patch_target(
            &root,
            r#"{"path":"new.txt","old_text":"","new_text":"hello"}"#,
            &mut snapshot,
        );

        assert_eq!(snapshot.get(&root.join("new.txt")), Some(&None));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revert_snapshot_restores_edited_files_and_deletes_created_ones() {
        let root = temporary_directory();
        fs::write(root.join("edited.txt"), "changed").unwrap();
        fs::write(root.join("created.txt"), "new content").unwrap();
        let mut snapshot = HashMap::new();
        snapshot.insert(root.join("edited.txt"), Some("original".to_string()));
        snapshot.insert(root.join("created.txt"), None);

        let outcome = revert_snapshot(&snapshot);

        assert_eq!(outcome.reverted.len(), 2);
        assert!(outcome.failed.is_empty());
        // The report names the files, so you can see what came back rather than how many did.
        let summary = outcome.summary("from the last turn");
        assert!(summary.contains("edited.txt"), "{summary}");
        assert!(summary.contains("created.txt"), "{summary}");
        assert!(summary.contains("from the last turn"), "{summary}");
        assert_eq!(
            fs::read_to_string(root.join("edited.txt")).unwrap(),
            "original"
        );
        assert!(!root.join("created.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revert_snapshot_treats_an_already_missing_file_as_reverted() {
        let root = temporary_directory();
        let mut snapshot = HashMap::new();
        snapshot.insert(root.join("never-written.txt"), None);

        assert_eq!(revert_snapshot(&snapshot).reverted.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_that_will_not_revert_is_reported_not_swallowed() {
        // A directory cannot be overwritten with file content, so this stands in for any
        // revert that fails. The file is still changed on disk, which is the part worth saying.
        let root = temporary_directory();
        fs::create_dir(root.join("blocked")).unwrap();
        let mut snapshot = HashMap::new();
        snapshot.insert(root.join("blocked"), Some("original".to_string()));

        let outcome = revert_snapshot(&snapshot);

        assert!(outcome.reverted.is_empty());
        assert_eq!(outcome.failed.len(), 1);
        let summary = outcome.summary("from the last turn");
        assert!(summary.contains("Could not revert"), "{summary}");
        assert!(summary.contains("blocked"), "the file is named: {summary}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_quiet_index_refresh_says_nothing() {
        // Most turns touch no indexed file. Announcing "refreshed 0 files" every time would be
        // noise in the transcript.
        assert!(index_refresh_message(Ok(0)).is_none());
    }

    #[test]
    fn index_refresh_reports_work_and_failure_differently() {
        let (text, failed) = index_refresh_message(Ok(3)).expect("a message");
        assert!(text.contains("3 file(s)"), "{text}");
        assert!(!failed, "a successful refresh is not an error");

        let (text, failed) = index_refresh_message(Err(anyhow::anyhow!("no embedding endpoint")))
            .expect("a message");
        assert!(failed, "a failed refresh is an error, not a notice");
        assert!(
            text.contains("no embedding endpoint"),
            "the cause survives: {text}"
        );
        assert!(text.contains("/index"), "and says how to recover: {text}");
    }

    #[test]
    fn an_empty_revert_says_so_rather_than_claiming_success() {
        let outcome = RevertOutcome::default();
        assert!(outcome.is_empty());
        assert!(
            outcome
                .summary("from the last turn")
                .starts_with("Nothing reverted")
        );
    }

    #[test]
    fn is_memory_tool_recognizes_only_the_three_memory_tools() {
        assert!(is_memory_tool("remember"));
        assert!(is_memory_tool("update_memory"));
        assert!(is_memory_tool("forget"));
        assert!(!is_memory_tool("ask_user"));
        assert!(!is_memory_tool("run_command"));
    }

    #[test]
    fn new_command_clears_always_allowed_and_undo_state() {
        let database = Database::open_in_memory_for_tests();
        let mut session: Option<Session> = None;
        let mut messages: Vec<Message> = vec![Message::user("hi")];
        let mut always_allowed: HashSet<String> = HashSet::from(["run_command".to_string()]);
        let mut last_turn_snapshot: Option<HashMap<PathBuf, Option<String>>> = Some(HashMap::from(
            [(PathBuf::from("a.txt"), Some("x".to_string()))],
        ));

        handle_command(
            "/new",
            &UnreachableProvider,
            None,
            &database,
            &mut session,
            &mut messages,
            &mut always_allowed,
            &mut last_turn_snapshot,
            &Prices::default(),
            None,
        )
        .unwrap();

        assert!(session.is_none());
        assert!(messages.is_empty());
        assert!(always_allowed.is_empty());
        assert!(last_turn_snapshot.is_none());
    }

    #[test]
    fn delete_command_clears_state_when_deleting_the_active_session() {
        let database = Database::open_in_memory_for_tests();
        let created = database.create_session("test", "m").unwrap();
        let id = created.id.clone();
        let mut session = Some(created);
        let mut messages: Vec<Message> = vec![Message::user("hi")];
        let mut always_allowed: HashSet<String> = HashSet::from(["patch_file".to_string()]);
        let mut last_turn_snapshot: Option<HashMap<PathBuf, Option<String>>> = Some(HashMap::new());

        handle_command(
            &format!("/delete {id}"),
            &UnreachableProvider,
            None,
            &database,
            &mut session,
            &mut messages,
            &mut always_allowed,
            &mut last_turn_snapshot,
            &Prices::default(),
            None,
        )
        .unwrap();

        assert!(session.is_none());
        assert!(messages.is_empty());
        assert!(always_allowed.is_empty());
        assert!(last_turn_snapshot.is_none());
    }

    /// A `Provider` that panics if actually called — for tests asserting that some check rejects
    /// or skips its input before ever making a request.
    struct UnreachableProvider;

    #[async_trait::async_trait]
    impl Provider for UnreachableProvider {
        fn name(&self) -> &'static str {
            "unreachable"
        }
        async fn chat(&self, _request: ChatRequest) -> Result<crate::provider::ChatResponse> {
            panic!("the provider should not have been called");
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<mpsc::UnboundedReceiver<Result<crate::provider::StreamEvent>>> {
            panic!("the provider should not have been called");
        }
        async fn embed(&self, _model: &str, _input: Vec<String>) -> Result<Vec<Vec<f32>>> {
            panic!("the provider should not have been asked to embed anything");
        }
    }

    /// A `Provider` that answers `embed` with a deterministic vector per input, so index writes can
    /// be asserted without a network call.
    struct StubEmbeddingProvider;

    #[async_trait::async_trait]
    impl Provider for StubEmbeddingProvider {
        fn name(&self) -> &'static str {
            "stub-embeddings"
        }
        async fn chat(&self, _request: ChatRequest) -> Result<crate::provider::ChatResponse> {
            panic!("these tests only exercise embedding");
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<mpsc::UnboundedReceiver<Result<crate::provider::StreamEvent>>> {
            panic!("these tests only exercise embedding");
        }
        async fn embed(&self, _model: &str, input: Vec<String>) -> Result<Vec<Vec<f32>>> {
            Ok(input.iter().map(|text| vec![text.len() as f32]).collect())
        }
    }

    /// A `Provider` whose `embed` refuses any input containing `poison`, standing in for a file
    /// the embedding endpoint will not accept.
    struct PickyEmbeddingProvider;

    #[async_trait::async_trait]
    impl Provider for PickyEmbeddingProvider {
        fn name(&self) -> &'static str {
            "picky-embeddings"
        }
        async fn chat(&self, _request: ChatRequest) -> Result<crate::provider::ChatResponse> {
            panic!("these tests only exercise embedding");
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<mpsc::UnboundedReceiver<Result<crate::provider::StreamEvent>>> {
            panic!("these tests only exercise embedding");
        }
        async fn embed(&self, _model: &str, input: Vec<String>) -> Result<Vec<Vec<f32>>> {
            if input.iter().any(|text| text.contains("poison")) {
                anyhow::bail!("input rejected by the embedding endpoint");
            }
            Ok(input.iter().map(|text| vec![text.len() as f32]).collect())
        }
    }

    fn profile_with_embedding(embedding_model: Option<&str>) -> Profile {
        Profile {
            name: "default".to_string(),
            model: "gpt-5".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "k".to_string(),
            context_window: None,
            tools: true,
            embedding_model: embedding_model.map(str::to_string),
            completions_path: None,
            send_session_id: false,
        }
    }

    /// Seed the index for a project-relative path without going through a provider, so a test can
    /// start from "already indexed" and assert what a refresh does next.
    fn seed_index(database: &Database, project: &ProjectContext, relative: &str, content: &str) {
        let key = project.key();
        database
            .replace_file_index(
                &key,
                relative,
                &content_hash(content),
                "embed-1",
                &[storage::NewCodeChunk {
                    start_line: 1,
                    end_line: 1,
                    content: content.to_string(),
                    embedding: vec![1.0],
                }],
            )
            .unwrap();
    }

    struct ConcurrentProvider {
        active: std::sync::atomic::AtomicUsize,
        maximum: std::sync::atomic::AtomicUsize,
        session_ids: std::sync::Mutex<Vec<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl Provider for ConcurrentProvider {
        fn name(&self) -> &'static str {
            "concurrent"
        }

        async fn chat(&self, request: ChatRequest) -> Result<crate::provider::ChatResponse> {
            use std::sync::atomic::Ordering;
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            self.session_ids
                .lock()
                .unwrap()
                .push(request.session_id.clone());
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            let first_round = request.messages.len() == 2;
            Ok(crate::provider::ChatResponse {
                content: request.messages[1].content.clone(),
                tool_calls: first_round
                    .then(|| ToolCall {
                        id: "read".to_string(),
                        name: "list_directory".to_string(),
                        arguments: r#"{"path":"."}"#.to_string(),
                    })
                    .into_iter()
                    .collect(),
                usage: Usage::default(),
                finish_reason: "stop".to_string(),
            })
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<mpsc::UnboundedReceiver<Result<crate::provider::StreamEvent>>> {
            panic!("the concurrent sub-agent test uses non-streaming chat");
        }

        async fn embed(&self, _model: &str, _input: Vec<String>) -> Result<Vec<Vec<f32>>> {
            panic!("the concurrent sub-agent test does not embed");
        }
    }

    #[tokio::test]
    async fn spawn_agents_run_concurrently_with_a_four_agent_cap() {
        use std::sync::atomic::Ordering;
        let provider = ConcurrentProvider {
            active: std::sync::atomic::AtomicUsize::new(0),
            maximum: std::sync::atomic::AtomicUsize::new(0),
            session_ids: std::sync::Mutex::new(Vec::new()),
        };
        let project = temporary_project();
        let calls = (0..6)
            .map(|index| ToolCall {
                id: format!("c{index}"),
                name: tools::SPAWN_AGENT_TOOL.to_string(),
                arguments: format!(r#"{{"prompt":"task {index}"}}"#),
            })
            .collect::<Vec<_>>();
        let references = calls.iter().collect::<Vec<_>>();

        let outputs = dispatch_spawn_agents(
            &provider,
            "model",
            &project,
            &references,
            Some("parent".to_string()),
        )
        .await;

        assert_eq!(outputs.len(), 6);
        assert_eq!(outputs["c0"].0, "task 0");
        assert_eq!(outputs["c5"].0, "task 5");
        assert_eq!(provider.maximum.load(Ordering::SeqCst), 4);
        let session_ids = provider.session_ids.lock().unwrap();
        for index in 0..6 {
            let expected = Some(format!("parent:agent:c{index}"));
            assert_eq!(
                session_ids.iter().filter(|id| **id == expected).count(),
                2,
                "both rounds of sub-agent c{index} should share its derived id"
            );
        }
        assert!(!session_ids.contains(&Some("parent".to_string())));
        fs::remove_dir_all(project.root()).unwrap();
    }

    #[tokio::test]
    async fn refresh_re_embeds_an_indexed_file_that_changed() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        let path = project.root().join("a.rs");
        seed_index(&database, &project, "a.rs", "fn old() {}");
        fs::write(&path, "fn new() {}").unwrap();

        let refreshed = refresh_index_for_paths(
            &StubEmbeddingProvider,
            &profile_with_embedding(Some("embed-1")),
            &database,
            &project,
            vec![path],
        )
        .await
        .unwrap();

        assert_eq!(refreshed, 1);
        let chunks = database.all_chunks(&project.key()).unwrap();
        assert_eq!(chunks.len(), 1, "the old chunk should have been replaced");
        assert_eq!(chunks[0].content, "fn new() {}");
        assert_eq!(
            database.indexed_file_hash(&project.key(), "a.rs").unwrap(),
            Some(content_hash("fn new() {}")),
            "the stored hash should follow the new content"
        );
    }

    #[tokio::test]
    async fn refresh_skips_a_file_that_still_matches_the_index() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        let path = project.root().join("a.rs");
        fs::write(&path, "fn a() {}").unwrap();
        seed_index(&database, &project, "a.rs", "fn a() {}");

        // `UnreachableProvider` panics on `embed`, so reaching zero here proves nothing was spent.
        let refreshed = refresh_index_for_paths(
            &UnreachableProvider,
            &profile_with_embedding(Some("embed-1")),
            &database,
            &project,
            vec![path],
        )
        .await
        .unwrap();

        assert_eq!(refreshed, 0);
    }

    /// The deliberate boundary: a turn that creates a file does not grow the index on Kamui's own
    /// initiative — the startup staleness hint reports it and the user decides.
    #[tokio::test]
    async fn refresh_ignores_a_file_that_was_never_indexed() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        let path = project.root().join("new.rs");
        fs::write(&path, "fn brand_new() {}").unwrap();

        let refreshed = refresh_index_for_paths(
            &UnreachableProvider,
            &profile_with_embedding(Some("embed-1")),
            &database,
            &project,
            vec![path],
        )
        .await
        .unwrap();

        assert_eq!(refreshed, 0);
        assert!(database.all_chunks(&project.key()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn refresh_drops_the_index_entry_for_a_file_that_disappeared() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        seed_index(&database, &project, "gone.rs", "fn gone() {}");

        let refreshed = refresh_index_for_paths(
            &UnreachableProvider,
            &profile_with_embedding(Some("embed-1")),
            &database,
            &project,
            vec![project.root().join("gone.rs")],
        )
        .await
        .unwrap();

        assert_eq!(refreshed, 1);
        assert!(database.all_chunks(&project.key()).unwrap().is_empty());
        assert!(database.indexed_files(&project.key()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn refresh_does_nothing_without_an_embedding_model() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        let path = project.root().join("a.rs");
        seed_index(&database, &project, "a.rs", "fn old() {}");
        fs::write(&path, "fn new() {}").unwrap();

        let refreshed = refresh_index_for_paths(
            &UnreachableProvider,
            &profile_with_embedding(None),
            &database,
            &project,
            vec![path],
        )
        .await
        .unwrap();

        assert_eq!(refreshed, 0);
    }

    #[tokio::test]
    async fn spawn_agent_rejects_invalid_json_without_calling_the_provider() {
        let root = temporary_directory().canonicalize().unwrap();
        let project = ProjectContext::from_root(root.clone()).unwrap();

        let output =
            dispatch_spawn_agent(&UnreachableProvider, "gpt-5", &project, "not json", None).await;

        assert!(output.starts_with("Error:"), "{output}");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn spawn_agent_rejects_an_empty_prompt_without_calling_the_provider() {
        let root = temporary_directory().canonicalize().unwrap();
        let project = ProjectContext::from_root(root.clone()).unwrap();

        let output = dispatch_spawn_agent(
            &UnreachableProvider,
            "gpt-5",
            &project,
            r#"{"prompt":"   "}"#,
            None,
        )
        .await;

        assert!(output.starts_with("Error:"), "{output}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cosine_similarity_of_identical_vectors_is_one() {
        let vector = vec![1.0_f32, 2.0, 3.0];
        assert!((cosine_similarity(&vector, &vector) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_of_orthogonal_vectors_is_zero() {
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_handles_mismatched_or_empty_vectors() {
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn content_hash_is_stable_and_change_sensitive() {
        assert_eq!(content_hash("hello"), content_hash("hello"));
        assert_ne!(content_hash("hello"), content_hash("world"));
    }

    fn temporary_project() -> ProjectContext {
        ProjectContext::from_root(temporary_directory()).unwrap()
    }

    #[tokio::test]
    async fn search_code_rejects_invalid_json_without_calling_the_provider() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_search_code(
            &UnreachableProvider,
            "text-embedding-3-small",
            &database,
            &temporary_project(),
            "not json",
        )
        .await;
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[tokio::test]
    async fn search_code_rejects_an_empty_query_without_calling_the_provider() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_search_code(
            &UnreachableProvider,
            "text-embedding-3-small",
            &database,
            &temporary_project(),
            r#"{"query":"   "}"#,
        )
        .await;
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[tokio::test]
    async fn search_code_reports_a_missing_index_without_calling_the_provider() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_search_code(
            &UnreachableProvider,
            "text-embedding-3-small",
            &database,
            &temporary_project(),
            r#"{"query":"how does auth work"}"#,
        )
        .await;
        assert!(output.contains("no code index found"), "{output}");
    }

    /// A project indexed elsewhere must not satisfy `search_code` here — the chunks exist, but not
    /// for this root, so the tool still reports a missing index rather than answering with another
    /// project's code.
    #[tokio::test]
    async fn search_code_ignores_another_projects_index() {
        let database = Database::open_in_memory_for_tests();
        database
            .insert_chunk("/somewhere/else", "src/main.rs", 1, 5, "other", &[0.1])
            .unwrap();

        let output = dispatch_search_code(
            &UnreachableProvider,
            "text-embedding-3-small",
            &database,
            &temporary_project(),
            r#"{"query":"how does auth work"}"#,
        )
        .await;

        assert!(output.contains("no code index found"), "{output}");
    }

    #[test]
    fn staleness_is_not_reported_for_an_unindexed_project() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        assert_eq!(index_staleness(&database, &project).unwrap(), None);
    }

    #[test]
    fn staleness_counts_changed_new_and_removed_files() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        let key = project.key();
        let root = project.root();

        // `indexed.rs` is indexed and untouched, `edited.rs` is indexed then modified, `new.rs`
        // appeared after indexing, and `gone.rs` was indexed but no longer exists on disk.
        fs::write(root.join("indexed.rs"), "fn a() {}").unwrap();
        fs::write(root.join("edited.rs"), "fn b() {}").unwrap();
        database.set_indexed_file(&key, "indexed.rs", "h1").unwrap();
        database.set_indexed_file(&key, "edited.rs", "h2").unwrap();
        database.set_indexed_file(&key, "gone.rs", "h3").unwrap();
        fs::write(root.join("new.rs"), "fn c() {}").unwrap();

        // Push the mtime forward instead of sleeping, so the test stays fast and deterministic.
        let edited = fs::OpenOptions::new()
            .write(true)
            .open(root.join("edited.rs"))
            .unwrap();
        edited
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(120))
            .unwrap();

        let staleness = index_staleness(&database, &project).unwrap().unwrap();

        assert_eq!(
            staleness,
            IndexStaleness {
                changed: 1,
                added: 1,
                removed: 1,
            }
        );
        assert!(!staleness.is_fresh());
        assert_eq!(staleness.describe(), "1 changed, 1 new, 1 removed");
    }

    #[test]
    fn staleness_is_fresh_when_every_indexed_file_is_untouched() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        fs::write(project.root().join("a.rs"), "fn a() {}").unwrap();
        database
            .set_indexed_file(&project.key(), "a.rs", "h1")
            .unwrap();

        let staleness = index_staleness(&database, &project).unwrap().unwrap();

        assert!(staleness.is_fresh(), "{staleness:?}");
        assert_eq!(staleness.describe(), "");
    }

    #[tokio::test]
    async fn one_unindexable_file_does_not_abort_the_whole_index() {
        // Aborting made the project permanently unindexable: a re-run skips the files already
        // stored and reaches the same bad one again, so the index could never be completed.
        let root = temporary_directory().canonicalize().unwrap();
        fs::write(root.join("good-one.txt"), "fine content").unwrap();
        fs::write(root.join("bad.txt"), "poison content").unwrap();
        fs::write(root.join("good-two.txt"), "also fine").unwrap();
        let project = ProjectContext::from_root(root.clone()).unwrap();
        let database = Database::open_in_memory_for_tests();

        let summary = run_index(
            &PickyEmbeddingProvider,
            &profile_with_embedding(Some("embed-model")),
            &database,
            &project,
        )
        .await
        .expect("the run survives one bad file");

        assert!(summary.contains("Indexed 2 file(s)"), "{summary}");
        assert!(summary.contains("Could not index 1"), "{summary}");
        assert!(summary.contains("bad.txt"), "the file is named: {summary}");
        assert!(
            database.chunk_count(&project.key()).unwrap() > 0,
            "the good files were still stored"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn an_endpoint_that_refuses_everything_fails_instead_of_walking_the_project() {
        // Every file failing is systemic. Reporting one error per file would bury the cause.
        let root = temporary_directory().canonicalize().unwrap();
        for i in 1..=6 {
            fs::write(root.join(format!("poison-{i}.txt")), "poison").unwrap();
        }
        let project = ProjectContext::from_root(root.clone()).unwrap();
        let database = Database::open_in_memory_for_tests();

        let error = run_index(
            &PickyEmbeddingProvider,
            &profile_with_embedding(Some("embed-model")),
            &database,
            &project,
        )
        .await
        .expect_err("a systemic failure is an error, not a summary");
        let text = format!("{error:#}");
        assert!(text.contains("first 3 file(s)"), "it stops early: {text}");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn run_index_requires_an_embedding_model() {
        let root = temporary_directory().canonicalize().unwrap();
        let project = ProjectContext::from_root(root.clone()).unwrap();
        let database = Database::open_in_memory_for_tests();
        let active = profile_with_embedding(None);

        let error = run_index(&UnreachableProvider, &active, &database, &project)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("embedding_model"));
        fs::remove_dir_all(root).unwrap();
    }
}
