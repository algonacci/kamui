//! The agentic system prompt sent on every chat request. It teaches the model how to work as a
//! terminal coding agent and is combined with any project instructions (`KAMUI.md`/`AGENTS.md`).

const BASE: &str = "\
You are Kamui, an AI coding assistant working in a terminal inside the user's project repository. \
The working directory is the project root.

Be concise and direct. Base your answers on the actual code, not assumptions, and keep responses \
short unless the user asks for detail. Match the existing conventions of the codebase. If you are \
unsure, say so rather than inventing details.";

const TOOLS: &str = "\
You can call tools to work in the repository:
- list_directory: see what a folder contains.
- read_file: read a file's contents.
- run_command: run a shell command (the user must approve it before it runs).
- patch_file: create or edit one file by exact-text replacement (the user must approve it).

Use tools to gather real information instead of guessing. Read a file before you answer questions \
about it or edit it. To change code, read the target first, then call patch_file with an old_text \
that occurs exactly once; prefer the smallest correct change and keep the surrounding style. Use \
run_command for builds, tests, and searches. Never claim you read, ran, or edited something unless \
you actually called the matching tool. If a tool returns an error, read it and adjust instead of \
repeating the same call.";

/// Build the system prompt. The tool guidance is included only when the active profile offers tools,
/// and any project instructions are appended after it.
pub fn build(tools_enabled: bool, project_instructions: Option<&str>) -> String {
    let mut prompt = String::from(BASE);
    if tools_enabled {
        prompt.push_str("\n\n");
        prompt.push_str(TOOLS);
    }
    if let Some(instructions) = project_instructions {
        prompt.push_str("\n\n");
        prompt.push_str(instructions);
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_guidance_is_present_only_when_tools_are_enabled() {
        let with_tools = build(true, None);
        assert!(with_tools.contains("patch_file"));
        assert!(with_tools.contains("Kamui"));

        let without_tools = build(false, None);
        assert!(!without_tools.contains("patch_file"));
        assert!(without_tools.contains("Kamui"));
    }

    #[test]
    fn project_instructions_are_appended() {
        let prompt = build(true, Some("Always use tabs, never spaces."));
        assert!(prompt.contains("Always use tabs, never spaces."));
        // Instructions come after the tool guidance.
        assert!(prompt.find("patch_file").unwrap() < prompt.find("tabs").unwrap());
    }
}
