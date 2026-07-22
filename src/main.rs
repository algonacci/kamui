mod chat;
mod compaction;
mod config;
mod context;
mod prompt;
mod provider;
mod storage;
mod tools;

use anyhow::{Context, Result};
use config::Config;
use context::ProjectContext;
use provider::Provider;
use provider::openai::OpenAIProvider;
use storage::Database;

#[tokio::main]
async fn main() -> Result<()> {
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

    let config = match Config::load()? {
        config::Loaded::Ready(config) => config,
        config::Loaded::NeedsSetup(path) => {
            println!("Kamui needs a bit of setup before the first chat.");
            println!("Edit your config and add your provider api_key (and model):");
            println!("  {}", path.display());
            println!("Then run kamui again.");
            return Ok(());
        }
    };
    let database = Database::open()?;
    let project = ProjectContext::discover()?;

    chat::start_chat(config, &database, &project, resume_id, |profile| {
        Box::new(OpenAIProvider::new(
            profile.api_key.clone(),
            profile.base_url.clone(),
        )) as Box<dyn Provider>
    })
    .await?;

    Ok(())
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
