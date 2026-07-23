use std::path::Path;

use anyhow::{Result, bail};
use dialoguer::{Confirm, FuzzySelect, Input, Password, theme::ColorfulTheme};

use crate::{config, provider::openai::OpenAIProvider};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub async fn run(path: &Path) -> Result<()> {
    config::ensure_onboarding_supported(path)?;
    let theme = ColorfulTheme::default();

    println!("Kamui onboarding");
    println!("================");
    println!();
    println!("Connect an OpenAI-compatible model provider.");

    loop {
        let base_url = Input::<String>::with_theme(&theme)
            .with_prompt("Provider base URL")
            .default(DEFAULT_BASE_URL.to_owned())
            .interact_text()?
            .trim_end_matches('/')
            .to_owned();
        let api_key = Password::with_theme(&theme)
            .with_prompt("API key")
            .interact()?
            .trim()
            .to_owned();

        println!("Checking available models...");
        match OpenAIProvider::list_models(&api_key, &base_url).await {
            Ok(models) => {
                let selected = FuzzySelect::with_theme(&theme)
                    .with_prompt("Choose the default model (type to search)")
                    .items(&models)
                    .default(0)
                    .interact()?;
                config::save_onboarding(path, &base_url, &api_key, &models[selected])?;
                println!("Connected. Found {} models.", models.len());
                println!("Configuration saved to {}", path.display());
                println!();
                return Ok(());
            }
            Err(error) => {
                eprintln!("Could not load models: {error:#}");
                if !Confirm::with_theme(&theme)
                    .with_prompt("Try provider setup again?")
                    .default(true)
                    .interact()?
                {
                    bail!("provider setup cancelled");
                }
            }
        }
    }
}
