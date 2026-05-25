/// Per-line preview entry parsed from a session JSONL transcript.
/// Lives in `state::` rather than `detect::` so the TUI's preview
/// overlay can consume the type without a downward dependency from
/// the presentation layer into the detection layer.
#[derive(Debug, Clone)]
pub(crate) enum LogEntry {
    UserText(String),
    AssistantText(String),
    ToolUse { name: String, detail: String },
    ToolResult { status: String, snippet: String },
    Result { is_error: bool },
}
