use anyhow::Result;
use std::io::{self, Write};

pub async fn start_chat() -> Result<()> {
    println!("╭──────────────────────────────╮");
    println!("│         Kamui v0.1.0         │");
    println!("╰──────────────────────────────╯");
    println!();
    println!("Type 'exit' to quit.");
    println!();

    let mut history: Vec<String> = Vec::new();

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

        history.push(format!("User: {}", input));

        // sementara fake response
        let response = format!("You said: {}", input);

        println!();
        println!("{}", response);
        println!();

        history.push(format!("Assistant: {}", response));
    }

    Ok(())
}