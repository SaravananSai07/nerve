//! Local rendezvous between Claude's blocking hooks and the sync agent.
//!
//! `nerve sync hook` (run by Claude as a PermissionRequest / PreToolUse
//! hook) writes `req/<id>.json` and polls `resp/<id>.json`. The sync
//! agent publishes pending requests to peers and writes the answer when
//! one arrives. Plain files keep the hook dependency-free and make the
//! state inspectable with `ls`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::protocol::{Answer, PendingRequest, Question};

pub(crate) struct Mailbox {
    dir: PathBuf,
}

impl Mailbox {
    pub(crate) fn new(sync_dir: &Path) -> Self {
        Self { dir: sync_dir.join("mailbox") }
    }

    fn req_dir(&self) -> PathBuf {
        self.dir.join("req")
    }

    fn resp_dir(&self) -> PathBuf {
        self.dir.join("resp")
    }

    fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.req_dir())?;
        std::fs::create_dir_all(self.resp_dir())
    }

    /// Atomic write: readers never see half a file.
    fn put(dir: &Path, id: &str, body: &str) -> std::io::Result<()> {
        let tmp = dir.join(format!(".{id}.tmp"));
        std::fs::write(&tmp, body)?;
        std::fs::rename(tmp, dir.join(format!("{id}.json")))
    }

    /// Hook inputs currently waiting, as protocol requests.
    pub(crate) fn pending(&self) -> Vec<PendingRequest> {
        let Ok(rd) = std::fs::read_dir(self.req_dir()) else {
            return Vec::new();
        };
        let mut out: Vec<PendingRequest> = rd
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let id = e.path().file_stem()?.to_str()?.to_string();
                let input: Value = serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
                Some(to_request(&id, &input, file_age_secs(&e.path())))
            })
            .collect();
        out.sort_by_key(|r| r.created_at);
        out
    }

    /// Deliver a remote answer. Returns false when the request is gone
    /// (answered locally, timed out) — the sender should hear that.
    pub(crate) fn answer(&self, id: &str, answer: &Answer) -> std::io::Result<bool> {
        if !valid_id(id) {
            return Ok(false);
        }
        let req = self.req_dir().join(format!("{id}.json"));
        let Ok(raw) = std::fs::read_to_string(&req) else {
            return Ok(false);
        };
        let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        let out = hook_output(&input, answer);
        Self::put(&self.resp_dir(), id, &out.to_string())?;
        Ok(true)
    }

    /// The hook side: park `input`, wait for an answer. `None` = no
    /// answer (timeout, or resolved at the keyboard): print nothing and
    /// Claude falls back to its normal local prompt.
    pub(crate) fn wait(&self, input: &Value, timeout: Duration, answered_locally: impl Fn() -> bool) -> Option<String> {
        self.ensure().ok()?;
        let id = super::protocol::new_id();
        Self::put(&self.req_dir(), &id, &input.to_string()).ok()?;
        let resp = self.resp_dir().join(format!("{id}.json"));
        let deadline = Instant::now() + timeout;
        let mut result = None;
        while Instant::now() < deadline {
            if let Ok(s) = std::fs::read_to_string(&resp) {
                result = Some(s);
                break;
            }
            if answered_locally() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let _ = std::fs::remove_file(self.req_dir().join(format!("{id}.json")));
        let _ = std::fs::remove_file(resp);
        result
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn file_age_secs(p: &Path) -> u64 {
    let now = super::protocol::now_secs();
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(now)
}

/// One-line description of what the tool wants to do.
fn summarise(tool: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let text = match tool {
        "Bash" => s("command").to_string(),
        "Write" | "Edit" | "MultiEdit" | "Read" | "NotebookEdit" => s("file_path").to_string(),
        "WebFetch" => s("url").to_string(),
        _ => input.to_string(),
    };
    text.chars().take(400).collect()
}

pub(crate) fn to_request(id: &str, input: &Value, created_at: u64) -> PendingRequest {
    let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("?").to_string();
    let tool_input = input.get("tool_input").cloned().unwrap_or(Value::Null);
    let questions: Vec<Question> = tool_input
        .get("questions")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .map(|q| Question {
                    question: q.get("question").and_then(Value::as_str).unwrap_or("").into(),
                    header: q.get("header").and_then(Value::as_str).unwrap_or("").into(),
                    options: q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|o| o.iter().filter_map(|x| x.get("label")?.as_str().map(String::from)).collect())
                        .unwrap_or_default(),
                    multi: q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    let is_question = tool == "AskUserQuestion";
    PendingRequest {
        id: id.to_string(),
        session: input.get("session_id").and_then(Value::as_str).unwrap_or("").into(),
        kind: if is_question { "question" } else { "permission" }.into(),
        summary: if is_question {
            questions.iter().map(|q| q.question.as_str()).collect::<Vec<_>>().join(" / ")
        } else {
            summarise(&tool, &tool_input)
        },
        tool,
        questions,
        created_at,
    }
}

/// The JSON a hook prints to apply `answer` (shapes verified on Claude
/// Code 2.1.288, see research/sync/experiments/RESULTS.md E6).
pub(crate) fn hook_output(input: &Value, answer: &Answer) -> Value {
    let event = input.get("hook_event_name").and_then(Value::as_str).unwrap_or("");
    let tool_input = input.get("tool_input").cloned().unwrap_or(Value::Null);
    match (event, answer) {
        ("PreToolUse", Answer::Choose { labels }) => {
            let questions = tool_input.get("questions").cloned().unwrap_or(json!([]));
            let mut answers = serde_json::Map::new();
            for (q, label) in questions.as_array().into_iter().flatten().zip(labels) {
                if let Some(text) = q.get("question").and_then(Value::as_str) {
                    answers.insert(text.to_string(), json!(label));
                }
            }
            json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "updatedInput": {"questions": questions, "answers": answers}
            }})
        }
        ("PreToolUse", Answer::Deny { reason }) => json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }}),
        ("PreToolUse", _) => json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse", "permissionDecision": "allow"
        }}),
        (_, Answer::Deny { reason }) => json!({"hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": {"behavior": "deny", "message": reason}
        }}),
        _ => json!({"hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": {"behavior": "allow"}
        }}),
    }
}

/// True once the transcript shows a result for the tool call this hook
/// is about — i.e. someone answered at the keyboard. The hook input has
/// no tool_use_id, so match the newest `tool_use` with the same name and
/// input, then look for its `tool_result`.
pub(crate) fn resolved_in_transcript(transcript: &Path, tool: &str, tool_input: &Value) -> bool {
    let Some(tail) = read_tail(transcript, 256 * 1024) else {
        return false;
    };
    let mut want: Option<String> = None;
    for line in tail.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(content) = v.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use")
                    if block.get("name").and_then(Value::as_str) == Some(tool)
                        && block.get("input") == Some(tool_input) =>
                {
                    want = block.get("id").and_then(Value::as_str).map(String::from);
                }
                Some("tool_result")
                    if want.is_some() && block.get("tool_use_id").and_then(Value::as_str) == want.as_deref() =>
                {
                    return true;
                }
                _ => {}
            }
        }
    }
    false
}

fn read_tail(path: &Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let s = String::from_utf8_lossy(&buf).into_owned();
    // Drop the partial first line when we started mid-file.
    Some(if len > max { s.split_once('\n').map(|x| x.1.to_string()).unwrap_or_default() } else { s })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask_input() -> Value {
        json!({"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion", "session_id": "s1",
               "tool_input": {"questions": [{"question": "Which language?", "header": "Lang",
                 "options": [{"label": "Python"}, {"label": "Bash"}], "multiSelect": false}]}})
    }

    #[test]
    fn question_answer_echoes_questions_and_maps_labels() {
        let out = hook_output(&ask_input(), &Answer::Choose { labels: vec!["Bash".into()] });
        let h = &out["hookSpecificOutput"];
        assert_eq!(h["permissionDecision"], "allow");
        assert_eq!(h["updatedInput"]["answers"]["Which language?"], "Bash");
        assert_eq!(h["updatedInput"]["questions"][0]["header"], "Lang");
    }

    #[test]
    fn permission_allow_and_deny_shapes() {
        let input = json!({"hook_event_name": "PermissionRequest", "tool_name": "Write", "tool_input": {"file_path": "/x"}});
        assert_eq!(hook_output(&input, &Answer::Allow)["hookSpecificOutput"]["decision"]["behavior"], "allow");
        let d = hook_output(&input, &Answer::Deny { reason: "no".into() });
        assert_eq!(d["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert_eq!(d["hookSpecificOutput"]["decision"]["message"], "no");
    }

    #[test]
    fn requests_summarise_tools_and_questions() {
        let r = to_request("ab", &ask_input(), 5);
        assert_eq!(r.kind, "question");
        assert_eq!(r.questions[0].options, vec!["Python", "Bash"]);
        let b = to_request("cd", &json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf build"}}), 5);
        assert_eq!((b.kind.as_str(), b.summary.as_str()), ("permission", "rm -rf build"));
    }

    #[test]
    fn wait_returns_remote_answer_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let mb = std::sync::Arc::new(Mailbox::new(dir.path()));
        let mb2 = mb.clone();
        let answerer = std::thread::spawn(move || {
            for _ in 0..100 {
                if let Some(r) = mb2.pending().first() {
                    assert!(mb2.answer(&r.id, &Answer::Allow).unwrap());
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("request never appeared");
        });
        let input = json!({"hook_event_name": "PermissionRequest", "tool_name": "Write", "tool_input": {}});
        let out = mb.wait(&input, Duration::from_secs(5), || false).unwrap();
        answerer.join().unwrap();
        assert!(out.contains("\"allow\""));
        assert!(mb.pending().is_empty(), "request withdrawn after answer");
    }

    #[test]
    fn wait_gives_up_when_answered_locally() {
        let dir = tempfile::tempdir().unwrap();
        let mb = Mailbox::new(dir.path());
        let out = mb.wait(&json!({}), Duration::from_secs(5), || true);
        assert!(out.is_none());
        assert!(mb.pending().is_empty());
        assert!(!mb.answer("ffff", &Answer::Allow).unwrap(), "late answer reports gone");
        assert!(!mb.answer("../x", &Answer::Allow).unwrap(), "ids are validated");
    }

    #[test]
    fn transcript_resolution_matches_tool_use_by_input() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("t.jsonl");
        let input = json!({"file_path": "/a", "content": "x"});
        let lines = [
            json!({"message": {"content": [{"type": "tool_use", "id": "tu1", "name": "Write", "input": input}]}}),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&t, &body).unwrap();
        assert!(!resolved_in_transcript(&t, "Write", &input));
        let result = json!({"message": {"content": [{"type": "tool_result", "tool_use_id": "tu1"}]}});
        std::fs::write(&t, format!("{body}{result}\n")).unwrap();
        assert!(resolved_in_transcript(&t, "Write", &input));
        assert!(!resolved_in_transcript(&t, "Write", &json!({"file_path": "/b"})));
    }
}
