use super::{
    ChatRequest, ChatResponse, Message, Provider, StreamEvent, ToolCall, ToolDefinition, Usage,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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

// Request wire types. These belong to the provider; the core stays agnostic and never
// serializes its own message types into an OpenAI-shaped payload.

#[derive(Serialize)]
struct OpenAIRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
}

#[derive(Serialize)]
struct OpenAIStreamRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunctionCall<'a>,
}

#[derive(Serialize)]
struct WireFunctionCall<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolFunction<'a>,
}

#[derive(Serialize)]
struct WireToolFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

fn wire_messages(messages: &[Message]) -> Vec<WireMessage<'_>> {
    messages.iter().map(wire_message).collect()
}

fn wire_message(message: &Message) -> WireMessage<'_> {
    // OpenAI expects a null content on an assistant turn that only requests tool calls.
    let content = if message.content.is_empty() && !message.tool_calls.is_empty() {
        None
    } else {
        Some(message.content.as_str())
    };
    WireMessage {
        role: message.role_name(),
        content,
        tool_calls: message
            .tool_calls
            .iter()
            .map(|call| WireToolCall {
                id: &call.id,
                kind: "function",
                function: WireFunctionCall {
                    name: &call.name,
                    arguments: &call.arguments,
                },
            })
            .collect(),
        tool_call_id: message.tool_call_id.as_deref(),
    }
}

fn wire_tools(tools: &[ToolDefinition]) -> Vec<WireTool<'_>> {
    tools
        .iter()
        .map(|tool| WireTool {
            kind: "function",
            function: WireToolFunction {
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.parameters,
            },
        })
        .collect()
}

// Response wire types.

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
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ResponseToolCall>,
}

#[derive(Debug, Deserialize)]
struct ResponseToolCall {
    id: String,
    function: ResponseFunctionCall,
}

#[derive(Debug, Deserialize)]
struct ResponseFunctionCall {
    name: String,
    arguments: String,
}

fn response_into_chat(mut response: OpenAIResponse) -> Result<ChatResponse> {
    let choice = response
        .choices
        .pop()
        .context("provider returned no choices")?;
    let tool_calls = choice
        .message
        .tool_calls
        .into_iter()
        .map(|call| ToolCall {
            id: call.id,
            name: call.function.name,
            arguments: call.function.arguments,
        })
        .collect();
    Ok(ChatResponse {
        content: choice.message.content.unwrap_or_default(),
        tool_calls,
        usage: response.usage,
        finish_reason: choice.finish_reason,
    })
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
        let body = OpenAIRequest {
            model: &request.model,
            messages: wire_messages(&request.messages),
            tools: wire_tools(&request.tools),
        };
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("failed to call provider")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("provider returned {status}: {body}");
        }

        let response: OpenAIResponse = response
            .json()
            .await
            .context("provider returned an invalid response")?;
        response_into_chat(response)
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::UnboundedReceiver<Result<StreamEvent>>> {
        let body = OpenAIStreamRequest {
            model: &request.model,
            messages: wire_messages(&request.messages),
            tools: wire_tools(&request.tools),
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };
        let mut response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
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

    #[test]
    fn serializes_tools_and_tool_messages() {
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![
                Message::user("read the file"),
                Message::tool_request(
                    String::new(),
                    vec![ToolCall {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: r#"{"path":"src/main.rs"}"#.to_string(),
                    }],
                ),
                Message::tool_result("call_1", "fn main() {}"),
            ],
            tools: vec![ToolDefinition {
                name: "read_file".to_string(),
                description: "Read a project file".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } }
                }),
            }],
        };
        let body = OpenAIRequest {
            model: &request.model,
            messages: wire_messages(&request.messages),
            tools: wire_tools(&request.tools),
        };
        let value = serde_json::to_value(&body).unwrap();

        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "read_file");
        // The assistant tool-call turn carries no content but one function call.
        assert!(value["messages"][1]["content"].is_null());
        assert_eq!(value["messages"][1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            value["messages"][1]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        // The tool result is tagged with the id of the call it answers.
        assert_eq!(value["messages"][2]["role"], "tool");
        assert_eq!(value["messages"][2]["tool_call_id"], "call_1");
        assert_eq!(value["messages"][2]["content"], "fn main() {}");
    }

    #[test]
    fn parses_plain_text_response() {
        let json = r#"{
            "choices": [{ "message": { "content": "Hi there" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 2, "completion_tokens": 2, "total_tokens": 4 }
        }"#;
        let response: OpenAIResponse = serde_json::from_str(json).unwrap();
        let chat = response_into_chat(response).unwrap();

        assert_eq!(chat.content, "Hi there");
        assert!(chat.tool_calls.is_empty());
        assert_eq!(chat.finish_reason, "stop");
    }

    #[test]
    fn response_without_choices_is_an_error() {
        let response: OpenAIResponse = serde_json::from_str(r#"{"choices":[]}"#).unwrap();
        assert!(response_into_chat(response).is_err());
    }

    #[test]
    fn omits_tools_and_serializes_plain_messages() {
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![Message::system("be brief"), Message::user("hi")],
            tools: Vec::new(),
        };
        let body = OpenAIRequest {
            model: &request.model,
            messages: wire_messages(&request.messages),
            tools: wire_tools(&request.tools),
        };
        let value = serde_json::to_value(&body).unwrap();

        assert!(value.get("tools").is_none());
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][0]["content"], "be brief");
        assert_eq!(value["messages"][1]["content"], "hi");
    }

    #[test]
    fn invalid_stream_json_is_an_error() {
        let (sender, _receiver) = mpsc::unbounded_channel();
        let mut usage = Usage::default();
        let mut finish_reason = String::new();

        assert!(parse_event(b"data: {not json}", &sender, &mut usage, &mut finish_reason).is_err());
    }

    #[test]
    fn parses_tool_calls_from_response() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"a.rs\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10 }
        }"#;
        let response: OpenAIResponse = serde_json::from_str(json).unwrap();
        let chat = response_into_chat(response).unwrap();

        assert_eq!(chat.finish_reason, "tool_calls");
        assert_eq!(chat.content, "");
        assert_eq!(chat.tool_calls.len(), 1);
        assert_eq!(chat.tool_calls[0].id, "call_9");
        assert_eq!(chat.tool_calls[0].name, "read_file");
        assert_eq!(chat.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(chat.usage.total_tokens, 10);
    }
}
