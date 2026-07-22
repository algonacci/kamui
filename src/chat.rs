use crate::provider::{ChatRequest, Message, Provider};
use anyhow::Result;
use std::io::{self, Write};

pub async fn start_chat(provider: &dyn Provider, model: String) -> Result<()> {
    println!("╭──────────────────────────────╮");
    println!("│         Kamui v0.1.0         │");
    println!("╰──────────────────────────────╯");
    println!();
    println!("Type 'exit' to quit.");
    println!();

    let mut messages = Vec::new();

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        let input = input.trim();

        if input.eq_ignore_ascii_case("exit") {
            println!("Goodbye 👋");
            break;
        }

        if input.is_empty() {
            continue;
        }

        messages.push(Message::user(input));

        let response = provider
            .chat(ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
            })
            .await?;

        println!();
        println!("{}", response.content);
        println!();
        println!(
            "Tokens: {} input + {} output = {} total | Finish: {}",
            response.usage.prompt_tokens,
            response.usage.completion_tokens,
            response.usage.total_tokens,
            response.finish_reason
        );
        println!();

        messages.push(Message::assistant(response.content));
    }

    Ok(())
}
