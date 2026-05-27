use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::Deserialize;

use crate::state::log_entry::LogEntry;
use crate::state::session::{SessionState, TokenUsage};
use crate::util::sanitize::{Sanitised, strip_ansi};

/// Open a JSONL transcript file with `O_NOFOLLOW`. Defends against
/// an attacker swapping a final-component symlink between our
/// `exists()` check in `find_jsonl` and the actual open. Multi-
/// component directory-level traversal is out of scope —
/// `~/.claude/projects/` is user-owned, so any cross-user attack
/// already presupposes a prior compromise.
fn open_jsonl(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
}

pub(super) fn read_tail_state(path: &Path) -> Option<SessionState> {
    let mut file = open_jsonl(path).ok()?;
    let len = file.metadata().ok()?.len();

    // Claude Code 2.x writes large entries that we skip past:
    // - file-history-snapshot: ~22KB each
    // - assistant entries with extended thinking: 50–100KB+
    // The tail window has to step past at least one of each, so we read a
    // generous slice rather than try to parse the whole file.
    let seek_pos = len.saturating_sub(262_144);
    file.seek(SeekFrom::Start(seek_pos)).ok()?;

    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;

    buf.lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .find_map(parse_jsonl_state)
}

/// Lightweight schema for the subset of JSONL fields we read when
/// inferring a session's state from its tail. Deriving over a thin
/// borrowed struct is ~10× faster than building a full
/// `serde_json::Value` tree per line and avoids the per-line
/// `HashMap<String, Value>` allocation.
#[derive(Deserialize)]
struct StateRow<'a> {
    #[serde(default, borrow, rename = "type")]
    entry_type: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    subtype: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    message: Option<StateMessage<'a>>,
}

#[derive(Deserialize)]
struct StateMessage<'a> {
    #[serde(default, borrow)]
    role: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow, rename = "stop_reason")]
    stop_reason: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    content: Option<Vec<StateContentItem<'a>>>,
}

#[derive(Deserialize)]
struct StateContentItem<'a> {
    #[serde(default, borrow, rename = "type")]
    item_type: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    name: Option<std::borrow::Cow<'a, str>>,
}

fn parse_jsonl_state(line: &str) -> Option<SessionState> {
    let row: StateRow = serde_json::from_str(line).ok()?;
    let entry_type = row.entry_type.as_deref().unwrap_or("");

    if entry_type == "result" {
        if row.subtype.as_deref() == Some("error") {
            return Some(SessionState::Error);
        }
        return Some(SessionState::Idle);
    }

    if entry_type == "system" {
        return None;
    }

    if matches!(entry_type, "progress" | "agent_progress" | "hook_progress") {
        return Some(SessionState::Processing);
    }

    let msg = row.message.as_ref()?;
    let role = msg.role.as_deref().unwrap_or("");

    if role == "assistant" {
        if let Some(content) = msg.content.as_ref() {
            for item in content {
                if item.item_type.as_deref() == Some("tool_use") {
                    let raw_name = item.name.as_deref().unwrap_or("unknown");
                    // JSONL is attacker-influenced; sanitise before any
                    // comparison so a name like "AskUserQuestion\x1b[2J"
                    // doesn't fall through with the escape attached.
                    let name = strip_ansi(raw_name).into_owned();
                    if name == "AskUserQuestion" || name == "ExitPlanMode" {
                        return Some(SessionState::WaitingForInput);
                    }
                    return Some(SessionState::ToolRunning(name));
                }
            }
        }
        return Some(match msg.stop_reason.as_deref().unwrap_or("") {
            "end_turn" | "max_tokens" | "stop_sequence" => SessionState::Idle,
            _ => SessionState::Processing,
        });
    }

    if role == "user" {
        if let Some(content) = msg.content.as_ref() {
            for item in content {
                if item.item_type.as_deref() == Some("tool_result") {
                    return Some(SessionState::Processing);
                }
            }
        }
        return Some(SessionState::Processing);
    }

    None
}

/// Mirror of the `assistant`-row schema used by `parse_token_usage`.
/// Like `StateRow` above, borrowed-Cow fields keep the per-line
/// parse cost dominated by JSON tokenisation rather than allocation.
#[derive(Deserialize)]
struct UsageRow<'a> {
    #[serde(default, borrow, rename = "type")]
    entry_type: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    message: Option<UsageMessage<'a>>,
}

#[derive(Deserialize)]
struct UsageMessage<'a> {
    #[serde(default, borrow)]
    model: Option<std::borrow::Cow<'a, str>>,
    #[serde(default)]
    usage: Option<UsageBlock>,
}

#[derive(Deserialize, Default)]
struct UsageBlock {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Copy, Clone, PartialEq)]
enum ModelClass {
    Opus,
    Haiku,
    Other,
}

impl ModelClass {
    fn classify(model: &str) -> Self {
        if model.contains("opus") {
            Self::Opus
        } else if model.contains("haiku") {
            Self::Haiku
        } else {
            Self::Other
        }
    }

    /// (input, output, cache_read, cache_create) — dollars per
    /// million tokens.
    fn rate(self) -> (f64, f64, f64, f64) {
        match self {
            Self::Opus => (15.0, 75.0, 1.50, 18.75),
            Self::Haiku => (0.80, 4.0, 0.08, 1.0),
            Self::Other => (3.0, 15.0, 0.30, 3.75),
        }
    }
}

pub(super) fn parse_token_usage(path: &Path, from_offset: u64) -> (TokenUsage, u64) {
    let usage = TokenUsage::default();

    let file = match open_jsonl(path) {
        Ok(f) => f,
        Err(_) => return (usage, from_offset),
    };
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

    if file_len < from_offset {
        // File was truncated/rotated under us. Re-parse from offset 0
        // *iteratively* (recursion previously meant an arbitrary stack
        // for a 50 MB JSONL); the call below is safe because we know
        // `file_len >= 0` makes the next `file_len < 0` impossible.
        return parse_token_usage_inner(path, 0);
    }
    parse_token_usage_inner(path, from_offset)
}

fn parse_token_usage_inner(path: &Path, from_offset: u64) -> (TokenUsage, u64) {
    let mut usage = TokenUsage::default();

    let file = match open_jsonl(path) {
        Ok(f) => f,
        Err(_) => return (usage, from_offset),
    };
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

    let mut reader = std::io::BufReader::new(file);
    if from_offset > 0 {
        if reader.seek(SeekFrom::Start(from_offset)).is_err() {
            return (usage, from_offset);
        }
        let mut skip = [0u8; 1];
        loop {
            match std::io::Read::read(&mut reader, &mut skip) {
                Ok(0) => break,
                Ok(_) => {
                    if skip[0] == b'\n' {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    } else if reader.seek(SeekFrom::Start(0)).is_err() {
        return (usage, from_offset);
    }

    // Memoize the per-line model classification: most JSONLs use
    // one model end-to-end, so a single substring scan up front
    // covers every subsequent line.
    let mut last_class: Option<ModelClass> = None;
    let mut last_rate = ModelClass::Other.rate();

    let mut line = String::new();
    loop {
        line.clear();
        match std::io::BufRead::read_line(&mut reader, &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let row: UsageRow = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(_) => continue,
        };

        if row.entry_type.as_deref() != Some("assistant") {
            continue;
        }

        let Some(msg) = row.message.as_ref() else { continue };
        if let Some(u) = msg.usage.as_ref() {
            let input = u.input_tokens.unwrap_or(0);
            let output = u.output_tokens.unwrap_or(0);
            let cache_read = u.cache_read_input_tokens.unwrap_or(0);
            let cache_creation = u.cache_creation_input_tokens.unwrap_or(0);

            let model = msg.model.as_deref().unwrap_or("sonnet");
            let class = ModelClass::classify(model);
            if last_class != Some(class) {
                last_rate = class.rate();
                last_class = Some(class);
            }
            let (rate_in, rate_out, rate_cache_read, rate_cache_create) = last_rate;

            // JSONL is untrusted: cap any single row's token claim so an
            // adversarial line claiming `u64::MAX` can't panic in debug or
            // produce `inf`/`NaN` cost in release.
            const PER_ROW_TOKEN_CAP: u64 = 10_000_000;
            let input = input.min(PER_ROW_TOKEN_CAP);
            let output = output.min(PER_ROW_TOKEN_CAP);
            let cache_read = cache_read.min(PER_ROW_TOKEN_CAP);
            let cache_creation = cache_creation.min(PER_ROW_TOKEN_CAP);

            usage.input_tokens = usage.input_tokens.saturating_add(input);
            usage.output_tokens = usage.output_tokens.saturating_add(output);
            usage.cache_read_tokens = usage.cache_read_tokens.saturating_add(cache_read);
            usage.cache_creation_tokens = usage.cache_creation_tokens.saturating_add(cache_creation);
            usage.cost_usd += (input as f64 * rate_in
                + output as f64 * rate_out
                + cache_read as f64 * rate_cache_read
                + cache_creation as f64 * rate_cache_create)
                / 1_000_000.0;
        }
    }

    let new_offset = reader.stream_position().unwrap_or(file_len);
    (usage, new_offset)
}

fn extract_tool_result_snippet(item: &serde_json::Value) -> String {
    let content = match item.get("content") {
        Some(c) => c,
        None => return String::new(),
    };

    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(arr) = content.as_array() {
        arr.iter()
            .filter_map(|v| {
                if v.get("type").and_then(|t| t.as_str()) == Some("text") {
                    v.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                } else {
                    None
                }
            })
            .next()
            .unwrap_or_default()
    } else {
        return String::new();
    };

    let first_line = text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let trimmed = first_line.trim();
    // Strip ANSI / OSC / C0+C1 controls BEFORE truncating. Truncating
    // first could split a CSI / OSC sequence, and `strip_ansi` would
    // then discard everything past the dangling introducer — silently
    // swallowing the truncated payload and any ellipsis we appended.
    // Strip first means truncate sees only printable text; the
    // grapheme cap is honoured deterministically.
    let cleaned = strip_ansi(trimmed);
    crate::util::text::truncate_graphemes(&cleaned, 80)
}

pub(crate) fn read_tail_entries(path: &Path, max_entries: usize) -> Vec<LogEntry> {
    let mut file = match open_jsonl(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);

    let seek_pos = len.saturating_sub(65536);
    if file.seek(SeekFrom::Start(seek_pos)).is_err() {
        return Vec::new();
    }

    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return Vec::new();
    }

    let mut entries = Vec::new();
    for line in buf.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if entry_type == "progress" || entry_type == "file-history-snapshot" {
            continue;
        }

        if entry_type == "result" {
            let is_error = val.get("subtype").and_then(|s| s.as_str()) == Some("error");
            entries.push(LogEntry::Result { is_error });
            continue;
        }

        let role = val
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("");

        if let Some(content) = val.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
            for item in content {
                let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match item_type {
                    "tool_use" => {
                        let raw_name =
                            item.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                        let name = Sanitised::new(raw_name);
                        let raw_detail: String = item
                            .get("input")
                            .and_then(|i| {
                                i.get("command")
                                    .or_else(|| i.get("file_path"))
                                    .or_else(|| i.get("pattern"))
                                    .or_else(|| i.get("description"))
                                    .or_else(|| {
                                        i.get("questions")
                                            .and_then(|q| q.as_array())
                                            .and_then(|q| q.first())
                                            .and_then(|q| q.get("question"))
                                    })
                                    .or_else(|| i.get("query"))
                                    .or_else(|| i.get("skill"))
                                    .and_then(|v| v.as_str())
                            })
                            .unwrap_or("")
                            .chars()
                            .take(200)
                            .collect();
                        let detail = Sanitised::new(raw_detail);
                        entries.push(LogEntry::ToolUse { name, detail });
                    }
                    "tool_result" => {
                        let is_err = item.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                        let status = Sanitised::new(if is_err { "error" } else { "ok" });
                        let snippet = Sanitised::new(extract_tool_result_snippet(item));
                        entries.push(LogEntry::ToolResult { status, snippet });
                    }
                    "text" => {
                        let raw = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        if !raw.is_empty() {
                            // `Sanitised::new` strips ANSI / OSC / C0+C1 bytes
                            // so the TUI never sees untrusted terminal sequences.
                            let text = Sanitised::new(raw);
                            if role == "user" {
                                entries.push(LogEntry::UserText(text));
                            } else if role == "assistant" {
                                entries.push(LogEntry::AssistantText(text));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let skip = entries.len().saturating_sub(max_entries);
    entries.into_iter().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_with_ansi_is_sanitised_before_storage() {
        // A `tool_use` row whose name carries an OSC-52 clipboard write
        // would propagate raw into the cards view on every tick without
        // sanitisation. The JSON source uses `` so serde_json
        // (which rejects raw control bytes) decodes it cleanly.
        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\\u001b]52;c;ZXZpbA==\\u0007after\"}]}}";
        match parse_jsonl_state(line).expect("parses") {
            SessionState::ToolRunning(name) => assert_eq!(name, "Bashafter"),
            other => panic!("expected ToolRunning, got {other:?}"),
        }
    }

    #[test]
    fn tool_name_ansi_smuggled_keyword_routes_to_correct_state() {
        // `AskUserQuestion<ESC>[2J` — without sanitisation would fall
        // through to ToolRunning with the escape attached; with it, the
        // sanitised name matches the WaitingForInput keyword.
        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"name\":\"AskUserQuestion\\u001b[2J\"}]}}";
        assert!(matches!(
            parse_jsonl_state(line),
            Some(SessionState::WaitingForInput)
        ));
    }

    #[test]
    fn snippet_truncation_does_not_panic_on_utf8_boundary() {
        // 79 ASCII chars + a 4-byte emoji + tail = 89 graphemes total.
        // Byte-slicing at 80 would split the emoji codepoint and panic;
        // grapheme-based truncation is well-defined and produces a
        // valid UTF-8 string with the ellipsis sentinel.
        let text = format!("{}🎉 trailing", "x".repeat(79));
        let item = serde_json::json!({
            "type": "tool_result",
            "content": text,
        });
        let snippet = extract_tool_result_snippet(&item);
        assert!(snippet.starts_with(&"x".repeat(78)));
        assert!(snippet.ends_with('…'));
        // Total grapheme count includes the ellipsis sentinel.
        assert!(snippet.chars().count() <= 80);
    }
}
