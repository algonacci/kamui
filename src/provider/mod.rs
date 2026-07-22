use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

pub mod openai;

#[derive(Clone, Debug)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Tool calls requested by an assistant turn. Empty for ordinary messages.
    pub tool_calls: Vec<ToolCall>,
    /// The call this message answers. Set only on tool-result messages.
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant turn that requests one or more tool calls.
    pub fn tool_request(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// The result of executing a tool, fed back to the model.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub fn role_name(&self) -> &'static str {
        match self.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub fn from_parts(role: &str, content: String) -> Result<Self> {
        let role = match role {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            _ => anyhow::bail!("unknown message role: {role}"),
        };

        Ok(Self {
            role,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
        })
    }
}

/// A provider-independent tool the model may call.
#[derive(Clone, Debug)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the tool's parameters.
    pub parameters: Value,
}

/// A tool invocation requested by the model. Serializable so it can be persisted with its message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments exactly as produced by the model.
    pub arguments: String,
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug)]
pub struct ChatResponse {
    pub content: String,
    // Parsed and tested now; the agent loop that consumes these lands in the next Phase 3 increment.
    #[allow(dead_code)]
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: String,
}

#[derive(Debug)]
pub enum StreamEvent {
    Delta(String),
    Done {
        usage: Usage,
        finish_reason: String,
        tool_calls: Vec<ToolCall>,
    },
}

#[derive(Debug, Default, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::UnboundedReceiver<Result<StreamEvent>>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_round_trips_known_roles() {
        for role in ["system", "user", "assistant", "tool"] {
            let message = Message::from_parts(role, "body".to_string()).unwrap();
            assert_eq!(message.role_name(), role);
            assert_eq!(message.content, "body");
            assert!(message.tool_calls.is_empty());
            assert!(message.tool_call_id.is_none());
        }
    }

    #[test]
    fn from_parts_rejects_unknown_roles() {
        assert!(Message::from_parts("wizard", "body".to_string()).is_err());
    }

    #[test]
    fn tool_request_carries_calls_on_an_assistant_turn() {
        let message = Message::tool_request(
            "",
            vec![ToolCall {
                id: "c1".to_string(),
                name: "read_file".to_string(),
                arguments: "{}".to_string(),
            }],
        );

        assert_eq!(message.role_name(), "assistant");
        assert_eq!(message.tool_calls.len(), 1);
        assert!(message.tool_call_id.is_none());
    }

    #[test]
    fn tool_result_records_the_answered_call() {
        let message = Message::tool_result("c1", "file body");

        assert_eq!(message.role_name(), "tool");
        assert_eq!(message.tool_call_id.as_deref(), Some("c1"));
        assert!(message.tool_calls.is_empty());
    }
}
