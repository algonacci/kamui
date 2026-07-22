use crate::context::read_project_file;
use crate::provider::{ToolCall, ToolDefinition};
use anyhow::{Context, Result};
use serde_json::json;
use std::path::PathBuf;

/// A capability the model can invoke by name. Implementations must be side-effect-safe for the
/// current Phase 3 scope: read-only, contained to the project, and never destructive.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn definition(&self) -> ToolDefinition;
    fn run(&self, arguments: &str) -> Result<String>;
}

/// The set of tools offered to the model, and the dispatcher that runs a requested call.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_defaults(project_root: PathBuf) -> Self {
        Self {
            tools: vec![Box::new(ReadFileTool { root: project_root })],
        }
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    /// Run a requested call. Failures are returned as an `Error: ...` string rather than propagated
    /// so the model can read the problem and recover on the next turn.
    pub fn dispatch(&self, call: &ToolCall) -> String {
        match self.tools.iter().find(|tool| tool.name() == call.name) {
            Some(tool) => match tool.run(&call.arguments) {
                Ok(output) => output,
                Err(error) => format!("Error: {error:#}"),
            },
            None => format!("Error: unknown tool '{}'", call.name),
        }
    }
}

/// Reads a UTF-8 text file from within the project, reusing the shared path-safety checks.
struct ReadFileTool {
    root: PathBuf,
}

impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description:
                "Read a UTF-8 text file from the project. The path must be relative to the \
                          project root, for example src/main.rs."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Project-relative path to the file to read."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn run(&self, arguments: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let path = value
            .get("path")
            .and_then(|path| path.as_str())
            .context("read_file requires a 'path' string argument")?;
        read_project_file(&self.root, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn project_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!("kamui-tools-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path.canonicalize().unwrap()
    }

    #[test]
    fn read_file_returns_file_contents() {
        let root = project_root();
        fs::write(root.join("note.txt"), "hello tools").unwrap();
        let registry = ToolRegistry::with_defaults(root.clone());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"note.txt"}"#.to_string(),
        };

        assert_eq!(registry.dispatch(&call), "hello tools");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_file_rejects_paths_outside_the_project() {
        let root = project_root();
        let registry = ToolRegistry::with_defaults(root.clone());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"../secret.txt"}"#.to_string(),
        };

        assert!(registry.dispatch(&call).starts_with("Error:"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_file_rejects_invalid_json_arguments() {
        let registry = ToolRegistry::with_defaults(std::env::temp_dir());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: "not json".to_string(),
        };

        assert!(registry.dispatch(&call).starts_with("Error:"));
    }

    #[test]
    fn dispatch_reports_unknown_tools() {
        let registry = ToolRegistry::with_defaults(std::env::temp_dir());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "launch_missiles".to_string(),
            arguments: "{}".to_string(),
        };

        assert!(registry.dispatch(&call).contains("unknown tool"));
    }
}
