mod chat;
mod context;
mod provider;
mod storage;

use anyhow::{Context, Result};
use context::ProjectContext;
use directories::BaseDirs;
use provider::openai::OpenAIProvider;
use storage::Database;

#[tokio::main]
async fn main() -> Result<()> {
    load_environment();

    let resume_id = match parse_command()? {
        Command::Chat(resume_id) => resume_id,
        Command::Help => {
            print_help();
            return Ok(());
        }
        Command::Version => {
            println!("kamui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    };

    let provider = OpenAIProvider::from_env()?;
    let model = std::env::var("OPENAI_MODEL").context("OPENAI_MODEL is not configured")?;
    let context_window = std::env::var("KAMUI_CONTEXT_WINDOW")
        .ok()
        .map(|value| {
            value
                .parse()
                .context("KAMUI_CONTEXT_WINDOW must be an integer")
        })
        .transpose()?;
    let database = Database::open()?;
    let project = ProjectContext::discover()?;

    chat::start_chat(
        &provider,
        model,
        context_window,
        &database,
        &project,
        resume_id,
    )
    .await?;

    Ok(())
}

fn load_environment() {
    // Local project configuration takes precedence; the global file fills missing values.
    dotenvy::dotenv().ok();
    if let Some(base_dirs) = BaseDirs::new() {
        dotenvy::from_path(base_dirs.config_dir().join("kamui").join(".env")).ok();
    }
}

enum Command {
    Chat(Option<String>),
    Help,
    Version,
}

fn parse_command() -> Result<Command> {
    let mut arguments = std::env::args().skip(1);
    match arguments.next().as_deref() {
        None => Ok(Command::Chat(None)),
        Some("-r" | "--resume") => {
            let id = arguments.next().context("usage: kamui -r <session-id>")?;
            if arguments.next().is_some() {
                anyhow::bail!("usage: kamui -r <session-id>");
            }
            Ok(Command::Chat(Some(id)))
        }
        Some("-h" | "--help") => Ok(Command::Help),
        Some("-V" | "--version") => Ok(Command::Version),
        Some(_) => anyhow::bail!("usage: kamui [-r <session-id>]"),
    }
}

fn print_help() {
    println!("Kamui - provider-agnostic LLM chat CLI\n");
    println!("Usage: kamui [OPTIONS]\n");
    println!("Options:");
    println!("  -r, --resume <ID>  Resume a saved session");
    println!("  -h, --help         Print help");
    println!("  -V, --version      Print version");
}
