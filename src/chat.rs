use crate::compaction;
use crate::config::{Config, Profile};
use crate::context::ProjectContext;
use crate::prompt;
use crate::provider::{ChatRequest, Message, Provider, StreamEvent, Usage};
use crate::storage::{Database, Session};
use crate::tools::ToolRegistry;
use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

const RESUME_PREVIEW_MESSAGES: usize = 6;
/// Upper bound on model/tool round-trips within a single user turn, to stop runaway tool loops.
/// Generous enough for multi-file edits while still bounding a stuck loop.
const MAX_TOOL_ROUNDS: usize = 25;
/// Settings key for the persisted active provider profile.
const ACTIVE_PROFILE_KEY: &str = "active_profile";

pub async fn start_chat<F>(
    config: Config,
    tools: ToolRegistry,
    database: &Database,
    project: &ProjectContext,
    resume_id: Option<String>,
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
    let mut provider = build_provider(&active);
    let mut context_window = active.context_window;

    print_banner();
    println!("Data: {}", database.path().display());
    println!("Project: {}", display_path(project.root()));
    println!("Model: {} ({})", active.model, active.name);
    if let Some(name) = project.instruction_name() {
        println!("Instructions: {name}");
    }
    println!("Type /help for commands or exit to quit.\n");

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
            println!("Resuming: {} ({})\n", session.title, short_id(&session.id));
            print_history_preview(&messages);
            (Some(session), messages)
        }
        None => {
            println!("New chat\n");
            (None, Vec::new())
        }
    };
    let mut input_rx = input_channel();

    // Rolling context compaction: `summary` folds in messages before `summarized_upto`; the rest of
    // `messages` is sent verbatim. Both reset whenever a command replaces the loaded history.
    let mut summary: Option<String> = None;
    let mut summarized_upto: usize = 0;

    'chat: loop {
        print!("> ");
        io::stdout().flush()?;

        let input = tokio::select! {
            input = input_rx.recv() => match input {
                Some(input) => input,
                None => {
                    shutdown(database, session.as_ref(), context_window)?;
                    break;
                }
            },
            signal = tokio::signal::ctrl_c() => {
                signal.context("failed to listen for Ctrl+C")?;
                println!();
                shutdown(database, session.as_ref(), context_window)?;
                break;
            }
        };
        let input = input.trim();

        if input.eq_ignore_ascii_case("exit") || input == "/exit" {
            shutdown(database, session.as_ref(), context_window)?;
            break;
        }
        if input.is_empty() {
            continue;
        }
        if input.starts_with('/') {
            let (command, argument) = input.split_once(' ').unwrap_or((input, ""));
            if command == "/model" {
                if let Err(error) = switch_profile(
                    argument.trim(),
                    &config,
                    &mut active,
                    &mut provider,
                    &mut context_window,
                    database,
                    &build_provider,
                ) {
                    eprintln!("Command failed: {error:#}\n");
                }
                continue;
            }
            if command == "/compact" {
                let outcome = tokio::select! {
                    result = run_compaction(
                        provider.as_ref(), &active.model, &messages, summary.as_deref(), summarized_upto,
                    ) => result,
                    signal = tokio::signal::ctrl_c() => {
                        signal.context("failed to listen for Ctrl+C")?;
                        println!("\n(interrupted — back to prompt)\n");
                        continue;
                    }
                };
                match outcome {
                    Ok(Some((new_summary, new_upto, count))) => {
                        summary = Some(new_summary);
                        summarized_upto = new_upto;
                        println!("Compacted {count} earlier messages into the summary.\n");
                    }
                    Ok(None) => println!("Not enough history to compact yet.\n"),
                    Err(error) => eprintln!("Compaction failed: {error:#}\n"),
                }
                continue;
            }
            let messages_before = messages.len();
            if let Err(error) = handle_command(
                input,
                provider.as_ref(),
                context_window,
                database,
                &mut session,
                &mut messages,
            ) {
                eprintln!("Command failed: {error:#}\n");
            }
            // Compaction state is tied to the current history; reset it if a command replaced it.
            if messages.len() != messages_before {
                summary = None;
                summarized_upto = 0;
            }
            continue;
        }

        let user_message = Message::user(input);
        let expanded = match project.expand_file_references(input) {
            Ok(expanded) => expanded,
            Err(error) => {
                eprintln!("\nCould not attach file: {error:#}\n");
                continue;
            }
        };

        let model = active.model.clone();
        // Some models/endpoints reject the `tools` field; a profile can opt out so plain chat works.
        let tool_definitions = if active.tools {
            tools.definitions()
        } else {
            Vec::new()
        };

        // Auto-compact older history once the recent portion grows past the threshold.
        summarized_upto = summarized_upto.min(messages.len());
        if compaction::total_bytes(&messages[summarized_upto..])
            > compaction::threshold(active.context_window)
        {
            let outcome = tokio::select! {
                result = run_compaction(
                    provider.as_ref(), &active.model, &messages, summary.as_deref(), summarized_upto,
                ) => result,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl+C")?;
                    println!("\n(interrupted — back to prompt)\n");
                    continue 'chat;
                }
            };
            match outcome {
                Ok(Some((new_summary, new_upto, count))) => {
                    summary = Some(new_summary);
                    summarized_upto = new_upto;
                    println!("(compacted {count} earlier messages into a running summary)\n");
                }
                Ok(None) => {}
                Err(error) => eprintln!("(could not compact history: {error:#})\n"),
            }
        }

        // Working conversation for this turn: the agentic system prompt (plus project instructions
        // and any running summary), the un-summarized recent history, and the expanded prompt.
        // Intermediate tool messages live here only; they are not persisted.
        let mut system = prompt::build(active.tools, project.system_message().as_deref());
        if let Some(summary) = &summary {
            system.push_str("\n\nSummary of the earlier conversation so far:\n\n");
            system.push_str(summary);
        }
        let mut turn_messages = vec![Message::system(system)];
        turn_messages.extend(messages[summarized_upto..].iter().cloned());
        turn_messages.push(Message::user_with_images(expanded.text, expanded.images));

        // Agent loop: stream a turn, run any tools it requests, and repeat until a plain answer.
        // `tool_trail` collects this turn's intermediate tool-request and tool-result messages so
        // they can be persisted alongside the prompt and final answer.
        let mut final_usage = Usage::default();
        let mut final_finish = String::new();
        let mut last_content = String::new();
        let mut tool_trail: Vec<Message> = Vec::new();
        let mut round = 0usize;
        let assistant_message = 'agent: loop {
            round += 1;
            if round > MAX_TOOL_ROUNDS {
                eprintln!(
                    "\nStopped after {MAX_TOOL_ROUNDS} tool rounds without a final answer.\n"
                );
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
            });
            println!();
            // Animate a spinner from the moment the request is sent until the first token (or a
            // terminal event) arrives, so the wait for the model does not look frozen.
            let mut spinner = Some(start_spinner("Thinking..."));
            let mut stream = tokio::select! {
                response = request => match response {
                    Ok(stream) => stream,
                    Err(error) => {
                        stop_spinner(&mut spinner).await;
                        eprintln!("\nRequest failed: {error:#}\n");
                        continue 'chat;
                    }
                },
                signal = tokio::signal::ctrl_c() => {
                    stop_spinner(&mut spinner).await;
                    signal.context("failed to listen for Ctrl+C")?;
                    println!("\n(interrupted — back to prompt)\n");
                    continue 'chat;
                }
            };

            let mut content = String::new();
            let mut ttft: Option<Duration> = None;
            let (usage, finish_reason, tool_calls) = loop {
                let event = tokio::select! {
                    event = stream.recv() => event,
                    signal = tokio::signal::ctrl_c() => {
                        stop_spinner(&mut spinner).await;
                        signal.context("failed to listen for Ctrl+C")?;
                        println!("\n(interrupted — back to prompt)\n");
                        continue 'chat;
                    }
                };
                match event {
                    Some(Ok(StreamEvent::Delta(delta))) => {
                        stop_spinner(&mut spinner).await;
                        if ttft.is_none() {
                            ttft = Some(started.elapsed());
                        }
                        print!("{delta}");
                        io::stdout().flush()?;
                        content.push_str(&delta);
                    }
                    Some(Ok(StreamEvent::Done {
                        usage,
                        finish_reason,
                        tool_calls,
                    })) => {
                        stop_spinner(&mut spinner).await;
                        println!();
                        break (usage, finish_reason, tool_calls);
                    }
                    Some(Err(error)) => {
                        stop_spinner(&mut spinner).await;
                        eprintln!("\n\nRequest failed: {error:#}\n");
                        continue 'chat;
                    }
                    None => {
                        stop_spinner(&mut spinner).await;
                        eprintln!("\n\nRequest failed: provider stream closed unexpectedly\n");
                        continue 'chat;
                    }
                }
            };
            print_usage(
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens,
                &finish_reason,
                ttft,
                started.elapsed(),
                context_window,
            );
            accumulate_usage(&mut final_usage, &usage);
            final_finish = finish_reason;
            last_content = content.clone();

            if tool_calls.is_empty() {
                break 'agent Message::assistant(content);
            }

            // The model requested tools. Record the request, run each tool, feed the results back.
            let request_message = Message::tool_request(content, tool_calls.clone());
            turn_messages.push(request_message.clone());
            tool_trail.push(request_message);
            for call in &tool_calls {
                println!(
                    "  \u{2192} {}({})",
                    call.name,
                    truncate(call.arguments.trim(), 120)
                );
                let output = if tools.requires_confirmation(&call.name) {
                    if let Some(preview) = tools.preview(call) {
                        println!("{preview}");
                    }
                    print!("    approve? [y/N] ");
                    io::stdout().flush()?;
                    let answer = tokio::select! {
                        answer = input_rx.recv() => answer,
                        signal = tokio::signal::ctrl_c() => {
                            signal.context("failed to listen for Ctrl+C")?;
                            println!("\n(interrupted — back to prompt)\n");
                            continue 'chat;
                        }
                    };
                    let approved = matches!(
                        answer.as_deref().map(str::trim),
                        Some("y" | "Y" | "yes" | "Yes")
                    );
                    if approved {
                        tokio::select! {
                            output = tools.dispatch(call) => output,
                            signal = tokio::signal::ctrl_c() => {
                                signal.context("failed to listen for Ctrl+C")?;
                                println!("\n    (interrupted — back to prompt)\n");
                                continue 'chat;
                            }
                        }
                    } else {
                        println!("    skipped");
                        "The user declined to run this command.".to_string()
                    }
                } else {
                    tokio::select! {
                        output = tools.dispatch(call) => output,
                        signal = tokio::signal::ctrl_c() => {
                            signal.context("failed to listen for Ctrl+C")?;
                            println!("\n    (interrupted — back to prompt)\n");
                            continue 'chat;
                        }
                    }
                };
                match output.strip_prefix("Error: ") {
                    Some(error) => println!("    ! {error}"),
                    None => println!("    ok ({} chars)", output.chars().count()),
                }
                let result_message = Message::tool_result(&call.id, output);
                turn_messages.push(result_message.clone());
                tool_trail.push(result_message);
            }
        };

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
        if active_session.title == "New chat" {
            active_session.title = make_title(input);
        }
        messages.extend(turn_record);

        if is_first_exchange {
            let title_request = provider.chat(ChatRequest {
                model: active.model.clone(),
                messages: vec![
                    Message::system(
                        "Create a concise title of at most 6 words for this conversation. Return only the title without quotes or punctuation.",
                    ),
                    Message::user(input),
                    Message::assistant(final_answer),
                ],
                tools: Vec::new(),
            });
            let title_response = tokio::select! {
                response = title_request => response,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl+C")?;
                    println!();
                    shutdown(database, session.as_ref(), context_window)?;
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
                Err(error) => eprintln!("Could not generate session title: {error:#}\n"),
            }
        }
    }

    Ok(())
}

/// A background task that animates a single-line braille spinner until told to stop.
struct Spinner {
    stop: Arc<Notify>,
    handle: JoinHandle<()>,
    width: usize,
}

fn start_spinner(label: &'static str) -> Spinner {
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
                    print!("\r{} {label}", FRAMES[frame % FRAMES.len()]);
                    let _ = io::stdout().flush();
                    frame += 1;
                }
            }
        }
    });
    Spinner {
        stop,
        handle,
        width: label.chars().count() + 2,
    }
}

impl Spinner {
    async fn finish(self) {
        self.stop.notify_one();
        let _ = self.handle.await;
        // Erase the spinner line so the response starts on a clean line.
        print!("\r{}\r", " ".repeat(self.width));
        let _ = io::stdout().flush();
    }
}

/// Stop the spinner if it is still running. Safe to call repeatedly.
async fn stop_spinner(spinner: &mut Option<Spinner>) {
    if let Some(spinner) = spinner.take() {
        spinner.finish().await;
    }
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
) -> Result<()> {
    let (command, argument) = input.split_once(' ').unwrap_or((input, ""));
    let argument = argument.trim();

    match command {
        "/help" => print_help(),
        "/new" => {
            *session = None;
            messages.clear();
            println!("Started a new chat. It will be saved after the first response.\n");
        }
        "/sessions" => {
            let sessions = database.list_sessions()?;
            if sessions.is_empty() {
                println!("No saved sessions.\n");
            } else {
                for item in sessions {
                    let marker = if session
                        .as_ref()
                        .is_some_and(|session| item.id == session.id)
                    {
                        "*"
                    } else {
                        " "
                    };
                    println!(
                        "{marker} {}  {}  {:<40} {:>3} messages  {:>8} tokens",
                        short_id(&item.id),
                        format_timestamp(item.updated_at),
                        item.title,
                        item.message_count,
                        item.total_tokens
                    );
                }
                println!();
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
            println!("Resumed: {} ({})\n", resumed.title, short_id(&resumed.id));
            *session = Some(resumed);
            print_history_preview(messages);
        }
        "/delete" => {
            let target = resolve_session(database, argument)?;
            database.delete_session(&target.id)?;
            println!("Deleted: {}\n", target.title);
            if session
                .as_ref()
                .is_some_and(|session| target.id == session.id)
            {
                *session = None;
                messages.clear();
                println!("Started a new chat. It will be saved after the first response.\n");
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
            println!("Renamed {} to: {new_title}\n", short_id(&target.id));
        }
        "/search" => {
            if argument.is_empty() {
                anyhow::bail!("usage: /search <text>");
            }
            let hits = database.search_messages(argument, 20)?;
            if hits.is_empty() {
                println!("No messages matched \"{argument}\".\n");
            } else {
                for hit in hits {
                    let speaker = match hit.role.as_str() {
                        "user" => "You",
                        "assistant" => "Assistant",
                        "system" => "System",
                        _ => "?",
                    };
                    println!(
                        "{}  {}  {:<30}  {speaker}: {}",
                        short_id(&hit.session_id),
                        format_timestamp(hit.created_at),
                        truncate(&hit.title, 30),
                        make_snippet(&hit.content, argument),
                    );
                }
                println!();
            }
        }
        "/stats" => match session.as_ref() {
            Some(session) => print_stats(database, session, context_window)?,
            None => println!("This chat has no saved messages yet.\n"),
        },
        _ => println!("Unknown command. Type /help for available commands.\n"),
    }

    Ok(())
}

fn resolve_session(database: &Database, id_prefix: &str) -> Result<Session> {
    if id_prefix.is_empty() {
        anyhow::bail!("a session ID is required");
    }
    database
        .find_session(id_prefix)?
        .with_context(|| format!("session '{id_prefix}' was not found or is ambiguous"))
}

fn print_stats(database: &Database, session: &Session, context_window: Option<u64>) -> Result<()> {
    let stats = database.session_stats(&session.id)?;
    println!("\nSession:       {}", session.title);
    println!("Requests:      {}", stats.request_count);
    println!("Input tokens:  {}", stats.input_tokens);
    println!("Output tokens: {}", stats.output_tokens);
    println!("Total tokens:  {}", stats.total_tokens);
    if let (Some(last_input), Some(window)) = (stats.last_input_tokens, context_window) {
        let percent = last_input as f64 / window as f64 * 100.0;
        println!("Last context:  {last_input}/{window} ({percent:.1}%)");
    }
    let by_model = database.model_stats(&session.id)?;
    if by_model.len() > 1 {
        println!("\n--- Per model ---");
        for m in &by_model {
            println!(
                "  {:<24} {:>3} req  {:>8} in  {:>8} out  {:>8} total",
                m.model, m.request_count, m.input_tokens, m.output_tokens, m.total_tokens
            );
        }
    }
    println!();
    Ok(())
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
) -> Result<Option<(String, usize, usize)>> {
    let Some(cutoff) = compaction::cutoff(messages.len(), summarized_upto) else {
        return Ok(None);
    };
    let rendered = compaction::render(&messages[summarized_upto..cutoff]);
    let request = compaction::summary_request(model, summary, &rendered);
    let response = provider.chat(request).await?;
    Ok(Some((
        response.content.trim().to_string(),
        cutoff,
        cutoff - summarized_upto,
    )))
}

/// List profiles, or switch to a named one and persist the choice. Rebuilding the provider swaps the
/// base URL and API key; the model and context window follow the profile.
fn switch_profile<F>(
    name: &str,
    config: &Config,
    active: &mut Profile,
    provider: &mut Box<dyn Provider>,
    context_window: &mut Option<u64>,
    database: &Database,
    build_provider: &F,
) -> Result<()>
where
    F: Fn(&Profile) -> Box<dyn Provider>,
{
    if name.is_empty() {
        println!("Profiles:");
        for profile in &config.profiles {
            let marker = if profile.name == active.name {
                "*"
            } else {
                " "
            };
            let tools = if profile.tools { "" } else { "  [no tools]" };
            println!(
                "{marker} {:<16} {:<22} {}{tools}",
                profile.name, profile.model, profile.base_url
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
            println!("Now using {} ({}).\n", profile.model, profile.name);
        }
        None => println!("Unknown profile '{name}'. Type /model to list profiles.\n"),
    }
    Ok(())
}

fn shutdown(
    database: &Database,
    session: Option<&Session>,
    context_window: Option<u64>,
) -> Result<()> {
    if let Some(session) = session {
        print_stats(database, session, context_window)?;
        println!("To resume this session: kamui -r {}", short_id(&session.id));
    }
    println!("Goodbye");
    Ok(())
}

fn print_history_preview(messages: &[Message]) {
    if messages.is_empty() {
        println!("No previous messages.\n");
        return;
    }

    let start = messages.len().saturating_sub(RESUME_PREVIEW_MESSAGES);
    if start > 0 {
        println!("... {} earlier messages omitted\n", start);
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

#[allow(clippy::too_many_arguments)]
fn print_usage(
    input: u64,
    output: u64,
    total: u64,
    finish_reason: &str,
    ttft: Option<Duration>,
    elapsed: Duration,
    context_window: Option<u64>,
) {
    print!("Tokens: {input} input + {output} output = {total} total");
    if let Some(window) = context_window {
        let percent = input as f64 / window as f64 * 100.0;
        print!(" | Context: {percent:.1}%");
    }
    if let Some(ttft) = ttft {
        print!(" | TTFT: {}", format_duration(ttft));
    }
    print!(" | Time: {}", format_duration(elapsed));
    println!(" | Finish: {finish_reason}\n");
}

/// Fold one agent-loop round's usage into the turn total: output tokens accumulate across every
/// round, while the input count tracks the final round so it still reflects the context that was
/// sent. Total is the last input plus all output generated during the turn.
fn accumulate_usage(total: &mut Usage, round: &Usage) {
    total.completion_tokens += round.completion_tokens;
    total.prompt_tokens = round.prompt_tokens;
    total.total_tokens = total.prompt_tokens + total.completion_tokens;
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 1.0 {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{seconds:.1}s")
    }
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
fn display_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(plain) = text.strip_prefix(r"\\?\") {
        plain.to_string()
    } else {
        text
    }
}

fn truncate(text: &str, max: usize) -> String {
    let mut result: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        result.push('…');
    }
    result
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

fn print_banner() {
    println!("╭──────────────────────────────╮");
    println!("│         Kamui v0.1.0         │");
    println!("╰──────────────────────────────╯\n");
}

fn print_help() {
    println!("/new              Start a new session");
    println!("/sessions         List saved sessions");
    println!("/resume <id>      Resume a session");
    println!("/model [name]     List provider profiles, or switch to one");
    println!("/rename <id> <t>  Rename a session");
    println!("/search <text>    Search saved messages");
    println!("/compact          Summarize older messages to free up context");
    println!("/delete <id>      Delete a session");
    println!("/stats            Show current session usage");
    println!("/exit             Save and quit\n");
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello…");
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
            },
        );
        accumulate_usage(
            &mut total,
            &Usage {
                prompt_tokens: 150,
                completion_tokens: 30,
                total_tokens: 180,
            },
        );

        assert_eq!(total.prompt_tokens, 150); // final round's context size
        assert_eq!(total.completion_tokens, 50); // output summed across rounds
        assert_eq!(total.total_tokens, 200); // last input + all output
    }

    #[test]
    fn format_duration_switches_units_at_one_second() {
        assert_eq!(format_duration(Duration::from_millis(320)), "320ms");
        assert_eq!(format_duration(Duration::from_millis(999)), "999ms");
        assert_eq!(format_duration(Duration::from_millis(4200)), "4.2s");
        assert_eq!(format_duration(Duration::from_secs(1)), "1.0s");
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
}
