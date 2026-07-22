mod chat;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    chat::start_chat().await?;

    Ok(())
}