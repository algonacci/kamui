use crate::context::ProjectContext;
use crate::provider::{ChatRequest, ChatResponse, Message, Provider, StreamEvent};
use crate::storage::{Database, Session};
use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use std::io::{self, Write};
use tokio::sync::mpsc;

const RESUME_PREVIEW_MESSAGES: usize = 6;

pub async fn start_chat(
    provider: &dyn Provider,
    default_model: String,
    context_window: Option<u64>,
    database: &Database,
    project: &ProjectContext,
    resume_id: Option<String>,
) -> Result<()> {
    print_banner();
    println!("Data: {}", database.path().display());
    println!("Project: {}", project.root().display());
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
    let mut input = input_channel();

    'chat: loop {
        print!("> ");
        io::stdout().flush()?;

        let input = tokio::select! {
            input = input.recv() => match input {
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
            if let Err(error) = handle_command(
                input,
                provider,
                context_window,
                database,
                &mut session,
                &mut messages,
            ) {
                eprintln!("Command failed: {error:#}\n");
            }
            continue;
        }

        let user_message = Message::user(input);
        let mut request_messages = messages.clone();
        if let Some(instructions) = project.system_message() {
            request_messages.insert(0, Message::system(instructions));
        }
        let expanded_input = match project.expand_file_references(input) {
            Ok(input) => input,
            Err(error) => {
                eprintln!("\nCould not attach file: {error:#}\n");
                continue;
            }
        };
        request_messages.push(Message::user(expanded_input));

        let request = provider.chat_stream(ChatRequest {
            model: session
                .as_ref()
                .map(|session| session.model.clone())
                .unwrap_or_else(|| default_model.clone()),
            messages: request_messages,
        });
        let mut stream = tokio::select! {
            response = request => match response {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("\nRequest failed: {error:#}\n");
                    continue;
                }
            },
            signal = tokio::signal::ctrl_c() => {
                signal.context("failed to listen for Ctrl+C")?;
                println!();
                shutdown(database, session.as_ref(), context_window)?;
                break;
            }
        };

        println!();
        let mut content = String::new();
        let response = loop {
            let event = tokio::select! {
                event = stream.recv() => event,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl+C")?;
                    println!();
                    shutdown(database, session.as_ref(), context_window)?;
                    return Ok(());
                }
            };
            match event {
                Some(Ok(StreamEvent::Delta(delta))) => {
                    print!("{delta}");
                    io::stdout().flush()?;
                    content.push_str(&delta);
                }
                Some(Ok(StreamEvent::Done {
                    usage,
                    finish_reason,
                })) => {
                    println!("\n");
                    break ChatResponse {
                        content,
                        usage,
                        finish_reason,
                    };
                }
                Some(Err(error)) => {
                    eprintln!("\n\nRequest failed: {error:#}\n");
                    continue 'chat;
                }
                None => {
                    eprintln!("\n\nRequest failed: provider stream closed unexpectedly\n");
                    continue 'chat;
                }
            }
        };
        print_usage(
            response.usage.prompt_tokens,
            response.usage.completion_tokens,
            response.usage.total_tokens,
            &response.finish_reason,
            context_window,
        );

        let assistant_message = Message::assistant(response.content);
        let is_first_exchange = session.is_none();
        let active_session = match session.as_mut() {
            Some(session) => session,
            None => session.insert(database.create_session(provider.name(), &default_model)?),
        };
        database.save_exchange(
            &active_session.id,
            &user_message,
            &assistant_message,
            &response.usage,
            &response.finish_reason,
        )?;
        if active_session.title == "New chat" {
            active_session.title = make_title(input);
        }
        messages.push(user_message);
        messages.push(assistant_message);

        if is_first_exchange {
            let title_request = provider.chat(ChatRequest {
                model: default_model.clone(),
                messages: vec![
                    Message::system(
                        "Create a concise title of at most 6 words for this conversation. Return only the title without quotes or punctuation.",
                    ),
                    messages[0].clone(),
                    messages[1].clone(),
                ],
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
    println!();
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
            _ => unreachable!(),
        };
        println!("{speaker}:\n{}\n", message.content);
    }
    println!("--- End of history ---\n");
}

fn print_usage(
    input: u64,
    output: u64,
    total: u64,
    finish_reason: &str,
    context_window: Option<u64>,
) {
    print!("Tokens: {input} input + {output} output = {total} total");
    if let Some(window) = context_window {
        let percent = input as f64 / window as f64 * 100.0;
        print!(" | Context: {percent:.1}%");
    }
    println!(" | Finish: {finish_reason}\n");
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
    println!("/delete <id>      Delete a session");
    println!("/stats            Show current session usage");
    println!("/exit             Save and quit\n");
}
