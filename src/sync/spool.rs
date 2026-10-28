//! CLI → running agent hand-off: when a long-lived agent (the TUI or
//! `nerve sync agent`) holds this device's identity, one-shot CLI
//! commands drop requests in `<sync>/spool/` instead of starting a
//! second agent that would race it for the mailbox and relays.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::agent::Request;
use super::protocol::{Body, JobSpec};

pub(crate) fn encode(r: &Request) -> Value {
    match r {
        Request::To { device, body } => json!({"t": "to", "device": device, "body": body}),
        Request::SyncNow => json!({"t": "sync"}),
        Request::Handoff { session_id, cwd, device, continue_with } => {
            json!({"t": "handoff", "session_id": session_id, "cwd": cwd, "device": device, "continue_with": continue_with})
        }
        Request::Job { spec, device } => json!({"t": "job", "spec": spec, "device": device}),
        Request::SetAway(on) => json!({"t": "away", "on": on}),
    }
}

pub(crate) fn decode(v: &Value) -> Option<Request> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    Some(match v.get("t")?.as_str()? {
        "to" => Request::To { device: s("device")?, body: serde_json::from_value::<Body>(v.get("body")?.clone()).ok()? },
        "sync" => Request::SyncNow,
        "handoff" => Request::Handoff {
            session_id: s("session_id")?,
            cwd: PathBuf::from(s("cwd")?),
            device: s("device")?,
            continue_with: s("continue_with"),
        },
        "job" => Request::Job { spec: serde_json::from_value::<JobSpec>(v.get("spec")?.clone()).ok()?, device: s("device") },
        "away" => Request::SetAway(v.get("on")?.as_bool()?),
        _ => return None,
    })
}

/// Take every queued request (oldest first), deleting the files.
pub(crate) fn drain(sync_dir: &Path) -> Vec<Request> {
    let spool = sync_dir.join("spool");
    let Ok(rd) = std::fs::read_dir(&spool) else {
        return Vec::new();
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| (std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), p))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|(_, p)| {
            let raw = std::fs::read_to_string(&p).ok();
            let _ = std::fs::remove_file(&p);
            decode(&serde_json::from_str(&raw?).ok()?)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::protocol::Answer;

    #[test]
    fn requests_round_trip_through_the_spool() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("spool");
        std::fs::create_dir_all(&spool).unwrap();
        let reqs = [
            Request::To { device: "pi".into(), body: Body::Answer { request: "ab".into(), answer: Answer::Allow } },
            Request::SetAway(true),
        ];
        for (i, r) in reqs.iter().enumerate() {
            std::fs::write(spool.join(format!("{i}.json")), encode(r).to_string()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        let got = drain(dir.path());
        assert_eq!(got.len(), 2);
        assert!(matches!(&got[0], Request::To { device, .. } if device == "pi"));
        assert!(matches!(got[1], Request::SetAway(true)));
        assert!(drain(dir.path()).is_empty(), "drained files are removed");
    }
}
