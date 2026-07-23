//! MCP (Model Context Protocol) client. Configured servers are launched as child processes and
//! talked to over stdio via the `rmcp` SDK; each tool they advertise is wrapped as a Kamui `Tool`,
//! so MCP tools flow through the same registry, permission policy, and agent loop as the built-ins.

use crate::config::McpServer;
use crate::provider::ToolDefinition;
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use rmcp::model::CallToolRequestParam;
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::TokioChildProcess;
use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;

pub struct ConnectionStatus {
    pub name: String,
    pub tool_count: usize,
    pub trusted: bool,
    pub error: Option<String>,
}

pub struct Connections {
    pub tools: Vec<Box<dyn Tool>>,
    pub statuses: Vec<ConnectionStatus>,
}

/// Connect to every configured server, returning all tools they advertise. A server that fails to
/// start is reported and skipped rather than preventing Kamui from running.
pub async fn connect_all(servers: &[McpServer]) -> Connections {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut statuses = Vec::new();
    for server in servers {
        match connect(server).await {
            Ok(mut connected) => {
                statuses.push(ConnectionStatus {
                    name: server.name.clone(),
                    tool_count: connected.len(),
                    trusted: server.trusted,
                    error: None,
                });
                tools.append(&mut connected);
            }
            Err(error) => statuses.push(ConnectionStatus {
                name: server.name.clone(),
                tool_count: 0,
                trusted: server.trusted,
                error: Some(format!("{error:#}")),
            }),
        }
    }
    Connections { tools, statuses }
}

async fn connect(server: &McpServer) -> Result<Vec<Box<dyn Tool>>> {
    let mut command = tokio::process::Command::new(&server.command);
    command.args(&server.args);

    // The transport defaults to inheriting stderr, which would interleave the server's own logging
    // with the chat UI, so it is silenced explicitly through the builder.
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start '{}'", server.command))?;
    let service = ().serve(transport).await.context("MCP initialization failed")?;
    let listed = service
        .list_all_tools()
        .await
        .context("could not list tools")?;

    let service = Arc::new(service);
    Ok(listed
        .into_iter()
        .map(|tool| {
            Box::new(McpTool {
                // Qualified so a server's tools cannot collide with the built-ins or each other.
                qualified_name: format!("{}__{}", server.name, tool.name),
                remote_name: tool.name.to_string(),
                description: tool.description.as_deref().unwrap_or_default().to_string(),
                schema: Value::Object(tool.input_schema.as_ref().clone()),
                trusted: server.trusted,
                service: service.clone(),
            }) as Box<dyn Tool>
        })
        .collect())
}

/// One tool advertised by a connected MCP server.
struct McpTool {
    qualified_name: String,
    remote_name: String,
    description: String,
    schema: Value,
    trusted: bool,
    service: Arc<RunningService<RoleClient, ()>>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.qualified_name.clone(),
            description: self.description.clone(),
            parameters: self.schema.clone(),
        }
    }

    /// Third-party servers can do anything, so their tools are confirmed unless marked trusted.
    fn requires_confirmation(&self) -> bool {
        !self.trusted
    }

    fn preview(&self, arguments: &str) -> Option<String> {
        Some(format!(
            "    (mcp) {} {}",
            self.remote_name,
            arguments.trim()
        ))
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let result = self
            .service
            .call_tool(CallToolRequestParam {
                name: self.remote_name.clone().into(),
                arguments: value.as_object().cloned(),
            })
            .await
            .with_context(|| format!("MCP tool '{}' failed", self.qualified_name))?;
        Ok(render_result(&result))
    }
}

/// Flatten an MCP result into text for the model: prefer the text parts, fall back to raw JSON.
fn render_result<T: serde::Serialize>(result: &T) -> String {
    let value = serde_json::to_value(result).unwrap_or(Value::Null);
    let is_error = value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let texts: Vec<&str> = value
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();

    let body = if texts.is_empty() {
        value.to_string()
    } else {
        texts.join("\n")
    };
    if is_error {
        format!("Error: {body}")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_text_content_parts() {
        let result = json!({
            "content": [{"type": "text", "text": "first"}, {"type": "text", "text": "second"}]
        });
        assert_eq!(render_result(&result), "first\nsecond");
    }

    #[test]
    fn marks_error_results_so_the_model_can_recover() {
        let result = json!({
            "content": [{"type": "text", "text": "no such sheet"}],
            "isError": true
        });
        assert_eq!(render_result(&result), "Error: no such sheet");
    }

    #[test]
    fn falls_back_to_json_when_there_is_no_text() {
        let result = json!({ "content": [{"type": "image", "data": "abc"}] });
        assert!(render_result(&result).contains("image"));
    }
}
