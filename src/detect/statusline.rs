//! `nerve statusline`: a Claude Code `statusLine` command that records the
//! JSON Claude pipes to it, so the dashboard can show Claude's own cost and
//! context-window figures instead of estimating them from the transcript.
//!
//! Wire it up in Claude's settings.json:
//! `"statusLine": {"type": "command", "command": "nerve statusline"}`, or
//! wrap an existing statusline with `nerve statusline -- <your command>`.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::state::token_usage::OfficialUsage;
use crate::util::sanitize::Sanitised;

/// Claude's statusline payload is a few KiB; anything far bigger is junk.
const INPUT_MAX_BYTES: u64 = 1024 * 1024;
/// Records older than this belong to long-gone sessions.
const RECORD_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// Claude runs the statusline on every render; sweep for stale records at
/// most this often (tracked by the marker file's mtime).
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
const PRUNE_MARKER: &str = ".last-prune";

/// Entry point for `nerve statusline [-- <command>...]`.
pub(crate) fn run(statusline_dir: &Path, wrapped: &[String]) -> io::Result<()> {
    let mut input = Vec::new();
    io::stdin().take(INPUT_MAX_BYTES).read_to_end(&mut input)?;
    let parsed: Option<Value> = serde_json::from_slice(&input).ok();

    let session_id = parsed
        .as_ref()
        .and_then(|v| v.get("session_id"))
        .and_then(Value::as_str)
        .and_then(super::process::valid_session_id);
    if let Some(id) = session_id {
        // Never let bookkeeping break the user's status line.
        if let Err(e) = record(statusline_dir, id, &input) {
            crate::log_warn!("statusline: cannot record {id}: {e}");
        }
    }

    if !wrapped.is_empty() {
        return run_wrapped(wrapped, &input);
    }
    if let Some(v) = parsed {
        println!("{}", default_line(&parse_usage(&v)));
    }
    Ok(())
}

/// Pipe the payload to the user's own statusline command, inheriting
/// stdout so its output is what Claude displays. A single argument is run
/// through `sh -c`, matching how Claude runs the `command` string itself.
fn run_wrapped(wrapped: &[String], input: &[u8]) -> io::Result<()> {
    let mut cmd = match wrapped {
        [one] => {
            let mut c = Command::new("sh");
            c.args(["-c", one]);
            c
        }
        [program, args @ ..] => {
            let mut c = Command::new(program);
            c.args(args);
            c
        }
        [] => return Ok(()),
    };
    let mut child = cmd.stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A command that ignores stdin closes the pipe early; that's fine.
        let _ = stdin.write_all(input);
    }
    child.wait()?;
    Ok(())
}

fn record(dir: &Path, session_id: &str, payload: &[u8]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{session_id}.json"));
    // Write-then-rename so the discovery worker never reads a torn file.
    let tmp = dir.join(format!(".{session_id}.{}.tmp", std::process::id()));
    fs::write(&tmp, payload)?;
    fs::rename(&tmp, &path)?;
    prune_stale(dir);
    Ok(())
}

fn prune_stale(dir: &Path) {
    let marker = dir.join(PRUNE_MARKER);
    let now = SystemTime::now();
    let recently_pruned = fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|mt| now.duration_since(mt).ok())
        .is_some_and(|age| age < PRUNE_EVERY);
    if recently_pruned {
        return;
    }
    let _ = fs::write(&marker, b"");
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mt| now.duration_since(mt).ok())
            .is_some_and(|age| age > RECORD_TTL);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn parse_usage(v: &Value) -> OfficialUsage {
    let model = v
        .pointer("/model/display_name")
        .or_else(|| v.pointer("/model/id"))
        .and_then(Value::as_str)
        .map(Sanitised::new);
    OfficialUsage {
        cost_usd: v.pointer("/cost/total_cost_usd").and_then(Value::as_f64),
        context_pct: v
            .pointer("/context_window/used_percentage")
            .and_then(Value::as_f64),
        model,
    }
}

fn default_line(u: &OfficialUsage) -> String {
    let mut parts = Vec::with_capacity(3);
    if let Some(m) = &u.model {
        parts.push(m.as_str().to_string());
    }
    if let Some(pct) = u.context_pct {
        parts.push(format!("ctx {pct:.0}%"));
    }
    if let Some(cost) = u.cost_usd {
        parts.push(format!("${cost:.2}"));
    }
    parts.join(" · ")
}

/// Discovery-side reader for the records `run` writes, keyed by mtime so
/// an unchanged record costs one `stat(2)` per scan.
pub(crate) struct StatuslineCache {
    dir: Option<PathBuf>,
    entries: HashMap<String, (SystemTime, OfficialUsage)>,
}

/// Upper bound on a record we're willing to parse.
const RECORD_MAX_BYTES: u64 = 256 * 1024;

impl StatuslineCache {
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            entries: HashMap::new(),
        }
    }

    pub(crate) fn read(&mut self, session_id: &str) -> Option<OfficialUsage> {
        let path = self.dir.as_ref()?.join(format!("{session_id}.json"));
        let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) else {
            self.entries.remove(session_id);
            return None;
        };
        if let Some((cached_at, usage)) = self.entries.get(session_id) {
            if *cached_at == mtime {
                return Some(usage.clone());
            }
        }
        let usage = crate::util::fs::read_capped(&path, RECORD_MAX_BYTES)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .map(|v| parse_usage(&v))?;
        self.entries
            .insert(session_id.to_string(), (mtime, usage.clone()));
        Some(usage)
    }

    pub(crate) fn retain_present(&mut self, keep: impl Fn(&str) -> bool) {
        self.entries.retain(|id, _| keep(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &str = r#"{
        "session_id": "abc-123",
        "model": {"id": "claude-opus-5-5", "display_name": "Opus 5.5"},
        "cost": {"total_cost_usd": 1.234},
        "context_window": {"used_percentage": 41.6}
    }"#;

    #[test]
    fn parses_official_figures() {
        let v: Value = serde_json::from_str(PAYLOAD).unwrap();
        let u = parse_usage(&v);
        assert_eq!(u.cost_usd, Some(1.234));
        assert_eq!(u.context_pct, Some(41.6));
        assert_eq!(u.model.as_ref().map(Sanitised::as_str), Some("Opus 5.5"));
        assert_eq!(default_line(&u), "Opus 5.5 · ctx 42% · $1.23");
    }

    #[test]
    fn missing_fields_are_none() {
        let u = parse_usage(&serde_json::json!({"session_id": "x"}));
        assert_eq!(u, OfficialUsage::default());
        assert_eq!(default_line(&u), "");
    }

    #[test]
    fn recorded_payload_round_trips_through_cache() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "abc-123", PAYLOAD.as_bytes()).unwrap();
        let mut cache = StatuslineCache::new(Some(tmp.path().to_path_buf()));
        let u = cache.read("abc-123").unwrap();
        assert_eq!(u.context_pct, Some(41.6));
        // Second read is served from the mtime cache.
        assert_eq!(cache.read("abc-123"), Some(u));
        assert_eq!(cache.read("missing"), None);
    }

    #[test]
    fn hostile_model_name_is_sanitised() {
        let v = serde_json::json!({"model": {"display_name": "Opus\u{1b}[2J"}});
        let u = parse_usage(&v);
        assert_eq!(u.model.as_ref().map(Sanitised::as_str), Some("Opus"));
    }
}
