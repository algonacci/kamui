use crate::context::{list_project_directory, read_project_file, resolve_for_write};
use crate::provider::{ToolCall, ToolDefinition};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

/// Hard limits for the command runner. A command is terminated past the timeout, and its captured
/// output is truncated so a chatty command cannot flood the model's context.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_COMMAND_OUTPUT: usize = 16 * 1024;

/// A capability the model can invoke by name. Read-only tools run without prompting; anything with
/// side effects returns `true` from `requires_confirmation` so the chat loop asks the user first.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn requires_confirmation(&self) -> bool {
        false
    }
    /// A human-readable preview of what this call would do, shown before asking for confirmation.
    fn preview(&self, _arguments: &str) -> Option<String> {
        None
    }
    async fn run(&self, arguments: &str) -> Result<String>;
}

/// The set of tools offered to the model, and the dispatcher that runs a requested call.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// The built-in project tools, plus any externally provided ones (e.g. from MCP servers).
    pub fn with_defaults(project_root: PathBuf, extra: Vec<Box<dyn Tool>>) -> Self {
        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ReadFileTool {
                root: project_root.clone(),
            }),
            Box::new(ListDirectoryTool {
                root: project_root.clone(),
            }),
            Box::new(RunCommandTool {
                root: project_root.clone(),
            }),
            Box::new(PatchFileTool { root: project_root }),
        ];
        tools.extend(extra);
        Self { tools }
    }

    /// A preview of what a confirmation-gated call would do, if the tool provides one.
    pub fn preview(&self, call: &ToolCall) -> Option<String> {
        self.tools
            .iter()
            .find(|tool| tool.name() == call.name)
            .and_then(|tool| tool.preview(&call.arguments))
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    /// Whether a named tool must be confirmed by the user before it runs. Unknown names are treated
    /// as not requiring confirmation; dispatch will report them as an error anyway.
    pub fn requires_confirmation(&self, name: &str) -> bool {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(|tool| tool.requires_confirmation())
            .unwrap_or(false)
    }

    /// Run a requested call. Failures are returned as an `Error: ...` string rather than propagated
    /// so the model can read the problem and recover on the next turn.
    pub async fn dispatch(&self, call: &ToolCall) -> String {
        match self.tools.iter().find(|tool| tool.name() == call.name) {
            Some(tool) => match tool.run(&call.arguments).await {
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

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
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

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let path = value
            .get("path")
            .and_then(|path| path.as_str())
            .context("read_file requires a 'path' string argument")?;
        read_project_file(&self.root, path)
    }
}

/// Lists the entries of a directory within the project, so the model can discover files to read.
struct ListDirectoryTool {
    root: PathBuf,
}

#[async_trait]
impl Tool for ListDirectoryTool {
    fn name(&self) -> &str {
        "list_directory"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "List the entries of a directory in the project. The path must be relative \
                          to the project root; use \".\" for the root. Directories are shown with a \
                          trailing slash."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Project-relative directory path, e.g. src or \".\" for the root."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let path = value
            .get("path")
            .and_then(|path| path.as_str())
            .context("list_directory requires a 'path' string argument")?;
        list_project_directory(&self.root, path)
    }
}

/// Runs a shell command in the project directory. This tool has side effects, so it requires user
/// confirmation (enforced by the chat loop) and is bounded by a timeout and an output cap.
struct RunCommandTool {
    root: PathBuf,
}

/// Whether the external `rtk` binary is available. Detected once per process; RTK is an optional
/// output-compression backend, never a requirement.
fn rtk_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("rtk")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// Decide whether to route a command through `rtk`. Only simple commands are routed: with shell
/// operators the prefix would apply to the first segment only, so those run directly. Commands the
/// model already prefixed with `rtk` are left untouched.
fn route_through_rtk(command: &str, rtk_is_available: bool) -> bool {
    if !rtk_is_available {
        return false;
    }
    let trimmed = command.trim();
    if trimmed == "rtk" || trimmed.starts_with("rtk ") {
        return false;
    }
    const SHELL_OPERATORS: [char; 9] = ['&', '|', ';', '>', '<', '`', '$', '(', '\n'];
    !trimmed.contains(SHELL_OPERATORS)
}

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description:
                "Run a shell command in the project directory and return its exit code and \
                          output. The user must approve each command before it runs. Use it for \
                          builds, tests, and searches."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to run, e.g. cargo test."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let command = value
            .get("command")
            .and_then(|command| command.as_str())
            .context("run_command requires a 'command' string argument")?;

        // Route supported invocations through the optional rtk binary to compress output before it
        // reaches model context; everything else runs the command directly.
        let executed = if route_through_rtk(command, rtk_available()) {
            format!("rtk {}", command.trim())
        } else {
            command.to_string()
        };

        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        let child = tokio::process::Command::new(shell)
            .arg(flag)
            .arg(&executed)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to start the command")?;

        match tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output()).await {
            Ok(result) => {
                let output = format_command_output(&result.context("failed to run the command")?);
                Ok(format!("command: {executed}\n{output}"))
            }
            Err(_) => Ok(format!(
                "Error: command timed out after {} seconds and was terminated",
                COMMAND_TIMEOUT.as_secs()
            )),
        }
    }
}

/// Applies an exact-match text replacement to one project file, or creates a new file. Mutating,
/// so it requires user confirmation, and it previews the change as +/- lines before approval.
struct PatchFileTool {
    root: PathBuf,
}

struct PatchArguments {
    path: String,
    old_text: String,
    new_text: String,
}

fn parse_patch_arguments(arguments: &str) -> Result<PatchArguments> {
    let value: serde_json::Value =
        serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
    let path = value
        .get("path")
        .and_then(|path| path.as_str())
        .context("patch_file requires a 'path' string argument")?;
    let new_text = value
        .get("new_text")
        .and_then(|text| text.as_str())
        .context("patch_file requires a 'new_text' string argument")?;
    let old_text = value
        .get("old_text")
        .and_then(|text| text.as_str())
        .unwrap_or_default();
    Ok(PatchArguments {
        path: path.to_string(),
        old_text: old_text.to_string(),
        new_text: new_text.to_string(),
    })
}

#[async_trait]
impl Tool for PatchFileTool {
    fn name(&self) -> &str {
        "patch_file"
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Modify one project file by replacing old_text (which must match the file \
                          exactly once) with new_text. To create a new file, pass an empty old_text \
                          and the full content as new_text. The user must approve each patch. Read \
                          the file first so old_text matches exactly."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Project-relative path of the file to modify or create."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Exact text to replace. Must appear exactly once. Empty to create a new file."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Replacement text, or the full content of a new file."
                    }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }

    fn preview(&self, arguments: &str) -> Option<String> {
        let patch = parse_patch_arguments(arguments).ok()?;
        let mut lines = vec![format!("    --- {}", patch.path)];
        for line in patch.old_text.lines() {
            lines.push(format!("    - {line}"));
        }
        for line in patch.new_text.lines() {
            lines.push(format!("    + {line}"));
        }
        const MAX_PREVIEW_LINES: usize = 40;
        if lines.len() > MAX_PREVIEW_LINES {
            let hidden = lines.len() - MAX_PREVIEW_LINES;
            lines.truncate(MAX_PREVIEW_LINES);
            lines.push(format!("    … ({hidden} more lines)"));
        }
        Some(lines.join("\n"))
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let patch = parse_patch_arguments(arguments)?;
        let target = resolve_for_write(&self.root, &patch.path)?;

        if patch.old_text.is_empty() {
            if target.exists() {
                anyhow::bail!(
                    "{} already exists; provide old_text to modify it",
                    patch.path
                );
            }
            write_atomic(&target, &patch.new_text)?;
            return Ok(format!(
                "created {} ({} bytes)",
                patch.path,
                patch.new_text.len()
            ));
        }

        let current = read_project_file(&self.root, &patch.path)?;
        // Match line-ending-agnostically: files (especially on Windows) often use CRLF, but models
        // emit LF, so a raw byte match would fail spuriously. Compare in LF space and restore the
        // file's line-ending style on write.
        let uses_crlf = current.contains("\r\n");
        let normalized = current.replace("\r\n", "\n");
        let old_text = patch.old_text.replace("\r\n", "\n");
        let new_text = patch.new_text.replace("\r\n", "\n");

        match normalized.matches(&old_text).count() {
            0 => anyhow::bail!(
                "old_text was not found in {}; read the file again and copy the text exactly",
                patch.path
            ),
            1 => {}
            occurrences => anyhow::bail!(
                "old_text appears {occurrences} times in {}; include more surrounding context so it matches exactly once",
                patch.path
            ),
        }
        let updated = normalized.replacen(&old_text, &new_text, 1);
        let updated = if uses_crlf {
            updated.replace('\n', "\r\n")
        } else {
            updated
        };
        write_atomic(&target, &updated)?;
        Ok(format!("patched {}", patch.path))
    }
}

/// Write through a temporary file in the same directory and rename it into place, so an interrupted
/// write can never leave a half-written file behind.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let mut temp_name = path
        .file_name()
        .context("write target has no file name")?
        .to_os_string();
    temp_name.push(".kamui-tmp");
    let temp = path.with_file_name(temp_name);
    std::fs::write(&temp, content)
        .with_context(|| format!("failed to write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| {
        let _ = std::fs::remove_file(&temp);
        format!("failed to replace {}", path.display())
    })
}

fn format_command_output(output: &Output) -> String {
    let code = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown (terminated by signal)".to_string());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut body = format!("exit code: {code}");
    if !stdout.trim().is_empty() {
        body.push_str("\nstdout:\n");
        body.push_str(stdout.trim_end());
    }
    if !stderr.trim().is_empty() {
        body.push_str("\nstderr:\n");
        body.push_str(stderr.trim_end());
    }
    cap(&body, MAX_COMMAND_OUTPUT)
}

/// Truncate to at most `max` bytes on a char boundary, noting when output was cut.
fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (output truncated)", &text[..end])
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

    #[tokio::test]
    async fn read_file_returns_file_contents() {
        let root = project_root();
        fs::write(root.join("note.txt"), "hello tools").unwrap();
        let registry = ToolRegistry::with_defaults(root.clone(), Vec::new());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"note.txt"}"#.to_string(),
        };

        assert_eq!(registry.dispatch(&call).await, "hello tools");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn read_file_rejects_paths_outside_the_project() {
        let root = project_root();
        let registry = ToolRegistry::with_defaults(root.clone(), Vec::new());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"../secret.txt"}"#.to_string(),
        };

        assert!(registry.dispatch(&call).await.starts_with("Error:"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn read_file_rejects_invalid_json_arguments() {
        let registry = ToolRegistry::with_defaults(std::env::temp_dir(), Vec::new());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: "not json".to_string(),
        };

        assert!(registry.dispatch(&call).await.starts_with("Error:"));
    }

    #[tokio::test]
    async fn dispatch_reports_unknown_tools() {
        let registry = ToolRegistry::with_defaults(std::env::temp_dir(), Vec::new());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "launch_missiles".to_string(),
            arguments: "{}".to_string(),
        };

        assert!(registry.dispatch(&call).await.contains("unknown tool"));
    }

    #[tokio::test]
    async fn list_directory_shows_directories_before_files() {
        let root = project_root();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("README.md"), "x").unwrap();
        let registry = ToolRegistry::with_defaults(root.clone(), Vec::new());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "list_directory".to_string(),
            arguments: r#"{"path":"."}"#.to_string(),
        };

        let output = registry.dispatch(&call).await;
        assert!(output.contains("src/"));
        assert!(output.contains("README.md"));
        assert!(output.find("src/").unwrap() < output.find("README.md").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn list_directory_rejects_a_file_path() {
        let root = project_root();
        fs::write(root.join("note.txt"), "x").unwrap();
        let registry = ToolRegistry::with_defaults(root.clone(), Vec::new());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "list_directory".to_string(),
            arguments: r#"{"path":"note.txt"}"#.to_string(),
        };

        assert!(registry.dispatch(&call).await.starts_with("Error:"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_mutating_tools_require_confirmation() {
        let registry = ToolRegistry::with_defaults(std::env::temp_dir(), Vec::new());
        assert!(registry.requires_confirmation("run_command"));
        assert!(registry.requires_confirmation("patch_file"));
        assert!(!registry.requires_confirmation("read_file"));
        assert!(!registry.requires_confirmation("list_directory"));
        assert!(!registry.requires_confirmation("unknown"));
    }

    #[tokio::test]
    async fn run_command_reports_output_and_exit_code() {
        let registry = ToolRegistry::with_defaults(std::env::temp_dir(), Vec::new());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "run_command".to_string(),
            arguments: r#"{"command":"echo kamui-ok"}"#.to_string(),
        };

        let output = registry.dispatch(&call).await;
        assert!(output.starts_with("command: "));
        assert!(output.contains("exit code: 0"));
        assert!(output.contains("kamui-ok"));
    }

    fn patch_call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_string(),
            name: "patch_file".to_string(),
            arguments: arguments.to_string(),
        }
    }

    #[tokio::test]
    async fn patch_file_replaces_an_exact_match() {
        let root = project_root();
        fs::write(root.join("main.rs"), "fn main() { old(); }\n").unwrap();
        let registry = ToolRegistry::with_defaults(root.clone(), Vec::new());

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"main.rs","old_text":"old();","new_text":"new();"}"#,
            ))
            .await;

        assert_eq!(output, "patched main.rs");
        assert_eq!(
            fs::read_to_string(root.join("main.rs")).unwrap(),
            "fn main() { new(); }\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_matches_across_line_endings() {
        let root = project_root();
        // A CRLF file, but the model's old_text uses LF.
        fs::write(
            root.join("crlf.txt"),
            "line one\r\nline two\r\nline three\r\n",
        )
        .unwrap();
        let registry = ToolRegistry::with_defaults(root.clone(), Vec::new());

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"crlf.txt","old_text":"line one\nline two","new_text":"line one\nLINE TWO"}"#,
            ))
            .await;

        assert_eq!(output, "patched crlf.txt");
        // The edit applies and the CRLF line endings are preserved.
        assert_eq!(
            fs::read_to_string(root.join("crlf.txt")).unwrap(),
            "line one\r\nLINE TWO\r\nline three\r\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_creates_a_new_file_when_old_text_is_empty() {
        let root = project_root();
        let registry = ToolRegistry::with_defaults(root.clone(), Vec::new());

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"NEW.md","old_text":"","new_text":"hello\n"}"#,
            ))
            .await;

        assert!(output.starts_with("created NEW.md"));
        assert_eq!(fs::read_to_string(root.join("NEW.md")).unwrap(), "hello\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_refuses_to_create_over_an_existing_file() {
        let root = project_root();
        fs::write(root.join("a.txt"), "content").unwrap();
        let registry = ToolRegistry::with_defaults(root.clone(), Vec::new());

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"a.txt","old_text":"","new_text":"x"}"#,
            ))
            .await;

        assert!(output.starts_with("Error:"));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "content");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_rejects_missing_and_ambiguous_matches() {
        let root = project_root();
        fs::write(root.join("a.txt"), "one two one").unwrap();
        let registry = ToolRegistry::with_defaults(root.clone(), Vec::new());

        let missing = registry
            .dispatch(&patch_call(
                r#"{"path":"a.txt","old_text":"three","new_text":"x"}"#,
            ))
            .await;
        let ambiguous = registry
            .dispatch(&patch_call(
                r#"{"path":"a.txt","old_text":"one","new_text":"x"}"#,
            ))
            .await;

        assert!(missing.contains("not found"));
        assert!(ambiguous.contains("2 times"));
        // The file is untouched after both failures.
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "one two one"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_rejects_paths_outside_the_project() {
        let root = project_root();
        let registry = ToolRegistry::with_defaults(root.clone(), Vec::new());

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"../escape.txt","old_text":"","new_text":"x"}"#,
            ))
            .await;

        assert!(output.starts_with("Error:"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patch_preview_shows_removed_and_added_lines() {
        let registry = ToolRegistry::with_defaults(std::env::temp_dir(), Vec::new());
        let preview = registry
            .preview(&patch_call(
                r#"{"path":"src/a.rs","old_text":"let a = 1;","new_text":"let a = 2;"}"#,
            ))
            .unwrap();

        assert!(preview.contains("--- src/a.rs"));
        assert!(preview.contains("- let a = 1;"));
        assert!(preview.contains("+ let a = 2;"));
    }

    #[test]
    fn routes_only_simple_commands_when_rtk_is_available() {
        assert!(route_through_rtk("cargo test", true));
        assert!(route_through_rtk("  git status  ", true));

        // Never without rtk.
        assert!(!route_through_rtk("cargo test", false));
        // Never double-prefix a command the model already routed.
        assert!(!route_through_rtk("rtk cargo test", true));
        assert!(!route_through_rtk("rtk", true));
        // Shell operators would leave rtk applied to the first segment only.
        assert!(!route_through_rtk("cargo build && cargo test", true));
        assert!(!route_through_rtk("cargo test | tail -5", true));
        assert!(!route_through_rtk("echo a; echo b", true));
        assert!(!route_through_rtk("cargo test > out.txt", true));
        assert!(!route_through_rtk("echo $HOME", true));
    }
}
