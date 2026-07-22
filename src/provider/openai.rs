use super::{ChatRequest, ChatResponse, Message, Provider, StreamEvent, Usage};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    base_url: String,
}

impl OpenAIProvider {
    pub fn from_env() -> Result<Self> {
        let api_key =
            std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY is not configured")?;
        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

        Ok(Self {
            client: Client::new(),
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct OpenAIResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: String,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: String,
}

#[derive(Serialize)]
struct OpenAIStreamRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Deserialize)]
struct OpenAIStreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    content: Option<String>,
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .context("failed to call provider")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("provider returned {status}: {body}");
        }

        let mut response: OpenAIResponse = response
            .json()
            .await
            .context("provider returned an invalid response")?;
        let choice = response
            .choices
            .pop()
            .context("provider returned no choices")?;

        Ok(ChatResponse {
            content: choice.message.content,
            usage: response.usage,
            finish_reason: choice.finish_reason,
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::UnboundedReceiver<Result<StreamEvent>>> {
        let request = OpenAIStreamRequest {
            model: request.model,
            messages: request.messages,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };
        let mut response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .context("failed to call provider")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("provider returned {status}: {body}");
        }

        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let result = read_stream(&mut response, &sender).await;
            if let Err(error) = result {
                let _ = sender.send(Err(error));
            }
        });
        Ok(receiver)
    }
}

async fn read_stream(
    response: &mut reqwest::Response,
    sender: &mpsc::UnboundedSender<Result<StreamEvent>>,
) -> Result<()> {
    let mut buffer = Vec::new();
    let mut usage = Usage::default();
    let mut finish_reason = String::new();

    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read provider stream")?
    {
        buffer.extend_from_slice(&chunk);
        while let Some(end) = find_event_end(&buffer) {
            let event = buffer.drain(..end).collect::<Vec<_>>();
            let delimiter = if buffer.starts_with(b"\r\n\r\n") {
                4
            } else {
                2
            };
            buffer.drain(..delimiter);
            if parse_event(&event, sender, &mut usage, &mut finish_reason)? {
                sender
                    .send(Ok(StreamEvent::Done {
                        usage,
                        finish_reason,
                    }))
                    .map_err(|_| anyhow::anyhow!("stream consumer disconnected"))?;
                return Ok(());
            }
        }
    }

    bail!("provider stream ended before [DONE]")
}

fn find_event_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .or_else(|| buffer.windows(2).position(|window| window == b"\n\n"))
}

fn parse_event(
    event: &[u8],
    sender: &mpsc::UnboundedSender<Result<StreamEvent>>,
    usage: &mut Usage,
    finish_reason: &mut String,
) -> Result<bool> {
    let event = std::str::from_utf8(event).context("provider returned invalid UTF-8")?;
    for line in event.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            return Ok(true);
        }
        let chunk: OpenAIStreamChunk =
            serde_json::from_str(data).context("provider returned an invalid stream event")?;
        if let Some(chunk_usage) = chunk.usage {
            *usage = chunk_usage;
        }
        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason {
                *finish_reason = reason;
            }
            if let Some(content) = choice.delta.content.filter(|content| !content.is_empty()) {
                sender
                    .send(Ok(StreamEvent::Delta(content)))
                    .map_err(|_| anyhow::anyhow!("stream consumer disconnected"))?;
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_delta_finish_and_usage_events() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut usage = Usage::default();
        let mut finish_reason = String::new();

        assert!(
            !parse_event(
                br#"data: {"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#,
                &sender,
                &mut usage,
                &mut finish_reason,
            )
            .unwrap()
        );
        assert!(!parse_event(
            br#"data: {"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
            &sender,
            &mut usage,
            &mut finish_reason,
        )
        .unwrap());
        assert!(
            !parse_event(
                br#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                &sender,
                &mut usage,
                &mut finish_reason,
            )
            .unwrap()
        );
        assert!(parse_event(b"data: [DONE]", &sender, &mut usage, &mut finish_reason,).unwrap());

        match receiver.try_recv().unwrap().unwrap() {
            StreamEvent::Delta(content) => assert_eq!(content, "Hello"),
            StreamEvent::Done { .. } => panic!("expected a delta"),
        }
        assert_eq!(usage.total_tokens, 5);
        assert_eq!(finish_reason, "stop");
    }
}
