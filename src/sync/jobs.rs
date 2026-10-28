//! Distributed jobs: a prompt that runs headless (`claude -p`) on the
//! device that has what it needs, then a verify command decides success.
//! The worker streams progress from `--output-format stream-json` (event
//! shapes recorded in research/sync/experiments E8) into `JobStatus`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use serde_json::Value;

use super::protocol::{now_secs, JobSpec, JobStatus};

const TAIL: usize = 6;

/// Fold one stream-json line into the status. Returns true when the
/// status visibly changed (worth republishing).
pub(crate) fn apply_stream_line(st: &mut JobStatus, line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let push = |st: &mut JobStatus, s: String| {
        st.tail.push(s.chars().take(160).collect());
        if st.tail.len() > TAIL {
            st.tail.remove(0);
        }
    };
    match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            st.session_id = v.get("session_id").and_then(Value::as_str).map(String::from);
            true
        }
        Some("assistant") => {
            let mut changed = false;
            for b in v.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                match b.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                        let input = b.get("input").cloned().unwrap_or(Value::Null);
                        let arg = ["command", "file_path", "pattern", "description"]
                            .iter()
                            .find_map(|k| input.get(*k).and_then(Value::as_str))
                            .unwrap_or("");
                        push(st, format!("▸ {name} {arg}"));
                        changed = true;
                    }
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            if let Some(first) = t.lines().find(|l| !l.trim().is_empty()) {
                                push(st, first.to_string());
                                changed = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            changed
        }
        Some("result") => {
            st.turns = v.get("num_turns").and_then(Value::as_u64).unwrap_or(0) as u32;
            st.cost_usd = v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
            st.result = v.get("result").and_then(Value::as_str).map(|s| s.chars().take(2000).collect());
            if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                st.phase = "failed".into();
            }
            true
        }
        _ => false,
    }
}

fn job_dir(spec: &JobSpec, root: &Path) -> PathBuf {
    match &spec.cwd {
        Some(c) => PathBuf::from(c),
        None => root.join(format!("job-{}", &spec.id)),
    }
}

/// Run a job to completion on this thread, sending every status change.
pub(crate) fn run(spec: JobSpec, device: &str, work_root: &Path, claude_root: Option<&Path>, tx: &Sender<JobStatus>) {
    let mut st = JobStatus {
        id: spec.id.clone(),
        title: spec.title.clone(),
        device: device.to_string(),
        phase: "running".into(),
        updated_at: now_secs(),
        ..Default::default()
    };
    let send = |st: &mut JobStatus| {
        st.updated_at = now_secs();
        let _ = tx.send(st.clone());
    };
    send(&mut st);
    let dir = job_dir(&spec, work_root);
    if let Err(e) = prepare_dir(&dir, spec.repo.as_deref()) {
        st.phase = "failed".into();
        st.result = Some(e);
        send(&mut st);
        return;
    }
    let claude = match super::bins::find("claude") {
        Ok(c) => c,
        Err(e) => {
            st.phase = "failed".into();
            st.result = Some(e);
            send(&mut st);
            return;
        }
    };
    let mut cmd = Command::new(claude);
    cmd.args(["-p", "--output-format", "stream-json", "--verbose", "--permission-mode"])
        .arg(if spec.permission_mode.is_empty() { "acceptEdits" } else { spec.permission_mode.as_str() })
        // Nobody is at a headless job's keyboard: anything that would
        // still prompt is denied instead of hanging the worker.
        .args(["--permission-prompts", "none"]);
    if let Some(m) = &spec.model {
        cmd.args(["--model", m]);
    }
    if !spec.allowed_tools.is_empty() {
        cmd.arg("--allowedTools").args(&spec.allowed_tools);
    }
    if let Some(root) = claude_root {
        cmd.env("CLAUDE_CONFIG_DIR", root);
    }
    // The prompt goes in on stdin: `--allowedTools` is variadic and would
    // swallow a trailing positional prompt.
    cmd.current_dir(&dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(spec.prompt.as_bytes());
            }
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if apply_stream_line(&mut st, &line) {
                        send(&mut st);
                    }
                }
            }
            let ok = child.wait().is_ok_and(|s| s.success());
            if !ok && st.phase != "failed" {
                st.phase = "failed".into();
            }
        }
        Err(e) => {
            st.phase = "failed".into();
            st.result = Some(format!("could not start claude: {e}"));
        }
    }
    if st.phase != "failed" {
        if let Some(v) = &spec.verify {
            st.phase = "verifying".into();
            send(&mut st);
            let out = Command::new("sh").args(["-c", v]).current_dir(&dir).stdin(Stdio::null()).output();
            let passed = out.as_ref().is_ok_and(|o| o.status.success());
            st.verified = Some(passed);
            if let Ok(o) = out {
                let text = String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).into_owned();
                for l in text.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev() {
                    st.tail.push(format!("verify: {l}"));
                }
                while st.tail.len() > TAIL {
                    st.tail.remove(0);
                }
            }
            st.phase = if passed { "done" } else { "failed" }.into();
        } else {
            st.phase = "done".into();
        }
    }
    send(&mut st);
}

fn prepare_dir(dir: &Path, repo: Option<&str>) -> Result<(), String> {
    if dir.exists() {
        return Ok(());
    }
    match repo {
        Some(url) => {
            let ok = Command::new("git")
                .args(["clone", "-q", url, &dir.to_string_lossy()])
                .stdin(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                Ok(())
            } else {
                Err(format!("git clone {url} failed"))
            }
        }
        None => std::fs::create_dir_all(dir).map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shapes taken from a real `claude -p --output-format stream-json`
    // run on Linux arm64 (2.1.288).
    #[test]
    fn stream_json_drives_status() {
        let mut st = JobStatus::default();
        assert!(apply_stream_line(&mut st, r#"{"type":"system","subtype":"init","session_id":"abc"}"#));
        assert_eq!(st.session_id.as_deref(), Some("abc"));
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"python3 test_calc.py"}}]}}"#;
        assert!(apply_stream_line(&mut st, tool));
        assert_eq!(st.tail.last().unwrap(), "▸ Bash python3 test_calc.py");
        assert!(!apply_stream_line(&mut st, r#"{"type":"rate_limit_event"}"#));
        assert!(!apply_stream_line(&mut st, "not json"));
        let res = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":8,"total_cost_usd":0.0048,"result":"Done."}"#;
        assert!(apply_stream_line(&mut st, res));
        assert_eq!((st.turns, st.result.as_deref()), (8, Some("Done.")));
    }

    #[test]
    fn tail_is_bounded() {
        let mut st = JobStatus::default();
        for i in 0..20 {
            apply_stream_line(&mut st, &format!(r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"line {i}"}}]}}}}"#));
        }
        assert_eq!(st.tail.len(), TAIL);
        assert_eq!(st.tail.last().unwrap(), "line 19");
    }
}
