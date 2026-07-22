mod chat;
mod provider;

use anyhow::{Context, Result};
use provider::openai::OpenAIProvider;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let provider = OpenAIProvider::from_env()?;
    let model = std::env::var("OPENAI_MODEL").context("OPENAI_MODEL is not configured")?;

    chat::start_chat(&provider, model).await?;

    Ok(())
}
