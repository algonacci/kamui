//! Context compaction: when a session's recent history grows large, older messages are folded into
//! a rolling summary so the conversation can continue without overflowing the model's context. The
//! full history is always kept in storage; only the per-request message list is compressed.

use crate::provider::{ChatRequest, Message};

/// Most recent messages always kept verbatim, never summarized.
const KEEP_RECENT: usize = 6;
/// Fallback byte threshold when the active profile has no `context_window`.
const DEFAULT_THRESHOLD_BYTES: usize = 48 * 1024;

/// Byte size at which the recent (un-summarized) history should be compacted. When a context window
/// is known, compact at roughly half of it (assuming ~4 bytes per token); otherwise use a default.
pub fn threshold(context_window: Option<u64>) -> usize {
    match context_window {
        Some(window) => (window as usize).saturating_mul(4) / 2,
        None => DEFAULT_THRESHOLD_BYTES,
    }
}

/// Approximate byte size of the given messages: text content plus any tool-call arguments.
pub fn total_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            message.content.len()
                + message
                    .tool_calls
                    .iter()
                    .map(|call| call.arguments.len())
                    .sum::<usize>()
        })
        .sum()
}

/// Index up to which messages should be folded into the summary, keeping the most recent
/// `KEEP_RECENT` verbatim. Returns `None` when there is nothing new worth summarizing.
pub fn cutoff(total_messages: usize, already_summarized: usize) -> Option<usize> {
    let cutoff = total_messages.saturating_sub(KEEP_RECENT);
    (cutoff > already_summarized).then_some(cutoff)
}

/// Render messages as plain text for the summarizer.
pub fn render(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|message| {
            let who = match message.role_name() {
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                "system" => "System",
                _ => "?",
            };
            if message.content.is_empty() && !message.tool_calls.is_empty() {
                let names: Vec<&str> = message
                    .tool_calls
                    .iter()
                    .map(|call| call.name.as_str())
                    .collect();
                format!("{who} (called tools: {})", names.join(", "))
            } else {
                format!("{who}: {}", message.content)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Build the non-streaming request that folds new messages into the running summary.
pub fn summary_request(model: &str, existing: Option<&str>, rendered: &str) -> ChatRequest {
    let instruction = "You maintain a running summary of a coding conversation so it can continue \
                       after older messages are dropped. Rewrite the summary to capture the user's \
                       goals, key decisions, files and code changed, commands run and their \
                       results, and any open threads. Be concise and factual, and output only the \
                       summary.";
    let prior = existing.unwrap_or("(none yet)");
    ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message::system(instruction),
            Message::user(format!(
                "Prior summary:\n{prior}\n\nNew messages to fold in:\n{rendered}"
            )),
        ],
        tools: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_scales_with_context_window() {
        assert_eq!(threshold(Some(1000)), 2000); // 1000 tokens * 4 bytes / 2
        assert_eq!(threshold(None), DEFAULT_THRESHOLD_BYTES);
    }

    #[test]
    fn total_bytes_counts_content_and_tool_arguments() {
        let messages = vec![Message::user("hello"), Message::assistant("hi there")];
        assert_eq!(total_bytes(&messages), "hello".len() + "hi there".len());
    }

    #[test]
    fn cutoff_keeps_recent_messages_and_advances() {
        assert_eq!(cutoff(4, 0), None); // fewer than KEEP_RECENT + 1
        assert_eq!(cutoff(10, 0), Some(4)); // summarize the first 4, keep 6
        assert_eq!(cutoff(10, 4), None); // nothing new past what is already summarized
        assert_eq!(cutoff(12, 4), Some(6)); // fold two more in
    }

    #[test]
    fn render_labels_speakers_and_tool_calls() {
        use crate::provider::ToolCall;
        let messages = vec![
            Message::user("fix the bug"),
            Message::tool_request(
                "",
                vec![ToolCall {
                    id: "c1".to_string(),
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
            ),
        ];
        let rendered = render(&messages);
        assert!(rendered.contains("User: fix the bug"));
        assert!(rendered.contains("called tools: read_file"));
    }
}
