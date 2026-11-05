use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::state::session::SessionState;
use crate::state::session::TokenUsage;
use crate::util::sanitize::{strip_ansi, Sanitised};

/// Open a JSONL transcript with `O_NOFOLLOW` (see `util::fs`).
fn open_jsonl(path: &Path) -> std::io::Result<std::fs::File> {
    crate::util::fs::open_nofollow(path)
}

pub(super) fn read_tail_state(path: &Path) -> Option<SessionState> {
    // Claude Code 2.x writes large entries that we skip past:
    // - file-history-snapshot: ~22KB each
    // - assistant entries with extended thinking: 50–100KB+
    // The tail window has to step past at least one of each, so we read a
    // generous slice rather than try to parse the whole file.
    let buf = crate::util::fs::read_tail_nofollow(path, 262_144).ok()?;
    buf.lines().rev().filter(|l| !l.trim().is_empty()).find_map(parse_jsonl_state)
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

/// Sum token usage from `from_offset` (a line start) to the last complete line.
/// Returns the usage and the offset after that line; a partial last line waits
/// for the next call.
pub(super) fn parse_token_usage(path: &Path, from_offset: u64) -> (TokenUsage, u64) {
    let mut usage = TokenUsage::default();

    let Ok(file) = open_jsonl(path) else { return (usage, from_offset) };
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut off = if file_len < from_offset { 0 } else { from_offset };

    let mut reader = std::io::BufReader::new(file);
    if reader.seek(SeekFrom::Start(off)).is_err() {
        return (usage, from_offset);
    }

    // Memoize the per-line model classification: most JSONLs use
    // one model end-to-end, so a single substring scan up front
    // covers every subsequent line.
    let mut last_class: Option<ModelClass> = None;
    let mut last_rate = ModelClass::Other.rate();

    let mut line = Vec::new();
    loop {
        line.clear();
        match std::io::BufRead::read_until(&mut reader, b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if !line.ends_with(b"\n") {
            break;
        }
        off += line.len() as u64;
        // A line that is not UTF-8 is skipped, but still counted past.
        let Ok(text) = std::str::from_utf8(&line) else { continue };
        let trimmed = text.trim();
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

            usage += TokenUsage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_creation_tokens: cache_creation,
                cost_usd: (input as f64 * rate_in
                    + output as f64 * rate_out
                    + cache_read as f64 * rate_cache_read
                    + cache_creation as f64 * rate_cache_create)
                    / 1_000_000.0,
            };
        }
    }

    (usage, off)
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

    let first_line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
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

/// Initial and maximum tail window for the log. Claude writes
/// large `attachment` records (often 100 KB+ at session start) that yield
/// no entries, so a fixed 64 KB window can show nothing for a session
/// that has a prompt. Double until we have enough entries.
const ENTRIES_WINDOW_START: u64 = 64 * 1024;
const ENTRIES_WINDOW_MAX: u64 = 4 * 1024 * 1024;

/// One transcript line, as the panel and peers show it.
#[derive(Debug, Clone)]
pub(crate) enum LogEntry {
    UserText(Sanitised),
    AssistantText(Sanitised),
    ToolUse { name: Sanitised, detail: Sanitised },
    ToolResult { is_error: bool, snippet: Sanitised },
    Result { is_error: bool },
}

/// One line, as peers and `nerve sync fetch log` show it.
impl std::fmt::Display for LogEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserText(t) => write!(f, "› {t}"),
            Self::AssistantText(t) => write!(f, "● {t}"),
            Self::ToolUse { name, detail } => write!(f, "▸ {name} {detail}"),
            Self::ToolResult { is_error, snippet } => {
                write!(f, "  ⎿ {} {snippet}", if *is_error { "error" } else { "ok" })
            }
            Self::Result { is_error: true } => f.write_str("✗ turn failed"),
            Self::Result { is_error: false } => f.write_str("✓ turn done"),
        }
    }
}

/// The transcripts of a session's subagents, in name order.
pub(crate) fn subagent_transcripts(transcript: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(transcript.with_extension("").join("subagents")) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> =
        rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect();
    files.sort();
    files
}

/// What a subagent transcript's `.meta.json` calls it, if anything.
pub(crate) fn subagent_description(subagent: &Path) -> String {
    std::fs::read_to_string(subagent.with_extension("meta.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("description").or(v.get("agentType")).and_then(|d| d.as_str()).map(String::from))
        .unwrap_or_default()
}

pub(crate) fn read_tail_entries(path: &Path, max_entries: usize) -> Vec<LogEntry> {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut window = ENTRIES_WINDOW_START;
    loop {
        let Ok(buf) = crate::util::fs::read_tail_nofollow(path, window) else {
            return Vec::new();
        };
        let entries = parse_entries(&buf);
        if entries.len() >= max_entries || window >= len || window >= ENTRIES_WINDOW_MAX {
            let skip = entries.len().saturating_sub(max_entries);
            return entries.into_iter().skip(skip).collect();
        }
        window *= 2;
    }
}

fn parse_entries(buf: &str) -> Vec<LogEntry> {
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

        let role = val.get("message").and_then(|m| m.get("role")).and_then(|r| r.as_str()).unwrap_or("");
        // Claude Code's own text in a user turn: a skill's or command's
        // expanded prompt, a subagent's report. Nobody typed it.
        if role == "user" && val.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
            continue;
        }

        let content = val.get("message").and_then(|m| m.get("content"));
        // A typed prompt is stored as a bare string, not a content array.
        if let Some(raw) = content.and_then(|c| c.as_str()) {
            if role == "user" && !raw.is_empty() {
                entries.push(LogEntry::UserText(Sanitised::new(raw)));
            }
            continue;
        }

        if let Some(content) = content.and_then(|c| c.as_array()) {
            for item in content {
                let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match item_type {
                    "tool_use" => {
                        let raw_name = item.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
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
                        let is_error = item.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                        let snippet = Sanitised::new(extract_tool_result_snippet(item));
                        entries.push(LogEntry::ToolResult { is_error, snippet });
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

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression: a fresh session's transcript is mostly large attachment
    // rows; the prompt must still be found beyond the first 64 KB window.
    #[test]
    fn entries_found_behind_large_attachment_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let mut body = String::from(r#"{"type":"user","message":{"role":"user","content":"hello"}}"#);
        body.push('\n');
        let pad = "x".repeat(20_000);
        for _ in 0..10 {
            body.push_str(&format!(r#"{{"type":"attachment","data":"{pad}"}}"#));
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        let entries = read_tail_entries(&path, 50);
        assert!(matches!(entries.as_slice(), [LogEntry::UserText(t)] if t.as_str() == "hello"));
    }

    // Regression: Claude 2.1.x writes a typed prompt as a plain string;
    // the log showed "No log entries" for a session that had one.
    #[test]
    fn text_claude_code_adds_to_a_user_turn_is_no_entry() {
        let buf = concat!(
            r#"{"type":"user","message":{"role":"user","content":"/loop check the build"}}"#,
            "\n",
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"/loop: schedule a prompt"}]}}"#,
            "\n",
        );
        let entries = parse_entries(buf);
        assert!(matches!(entries.as_slice(), [LogEntry::UserText(t)] if t.as_str() == "/loop check the build"));
    }

    #[test]
    fn string_content_user_prompt_is_an_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"attachment"}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":"ask me tea or coffee"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let entries = read_tail_entries(&path, 50);
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], LogEntry::UserText(t) if t.as_str() == "ask me tea or coffee"));
    }

    #[test]
    fn usage_is_counted_incrementally_without_losing_a_line() {
        let row = |n: u64| format!("{{\"type\":\"assistant\",\"message\":{{\"usage\":{{\"input_tokens\":{n}}}}}}}\n");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        std::fs::write(&path, format!("{}{}", row(1), row(2))).unwrap();
        let (first, off) = parse_token_usage(&path, 0);
        assert_eq!(first.input_tokens, 3);

        // A line still being written is left for the next call.
        let partial = row(8);
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut f, format!("{}{}{}", row(4), row(5), &partial[..10]).as_bytes()).unwrap();
        let (second, off2) = parse_token_usage(&path, off);
        assert_eq!(second.input_tokens, 9);

        std::io::Write::write_all(&mut f, &partial.as_bytes()[10..]).unwrap();
        let (third, _) = parse_token_usage(&path, off2);
        assert_eq!(third.input_tokens, 8);

        // A shorter file at the same path starts over.
        std::fs::write(&path, row(6)).unwrap();
        assert_eq!(parse_token_usage(&path, off2).0.input_tokens, 6);
    }

    #[test]
    fn a_line_that_is_not_utf8_is_skipped_but_not_stuck_on() {
        let row = |n: u64| format!("{{\"type\":\"assistant\",\"message\":{{\"usage\":{{\"input_tokens\":{n}}}}}}}\n");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let mut bytes = row(1).into_bytes();
        bytes.extend_from_slice(b"{\"type\":\"assistant\",\"x\":\"\xff\xfe\"}\n");
        bytes.extend_from_slice(row(2).as_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let (usage, off) = parse_token_usage(&path, 0);
        assert_eq!(usage.input_tokens, 3);
        assert_eq!(off, bytes.len() as u64);
    }

    #[test]
    fn tool_name_with_ansi_is_sanitised_before_storage() {
        // A `tool_use` row whose name carries an OSC-52 clipboard write
        // would propagate raw into the session list on every tick without
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
        assert!(matches!(parse_jsonl_state(line), Some(SessionState::WaitingForInput)));
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
