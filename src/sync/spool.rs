//! CLI → running agent hand-off: when a long-lived agent (the TUI or
//! `nerve sync agent`) holds this device's identity, one-shot CLI
//! commands drop requests in `<sync>/spool/` instead of starting a
//! second agent that would race it for the mailbox and relays.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::agent::Request;
use super::protocol::{Body, JobSpec, RunSpec, XferOffer};

pub(crate) fn encode(r: &Request) -> Value {
    match r {
        Request::To { device, body } => json!({"t": "to", "device": device, "body": body}),
        Request::SyncNow => json!({"t": "sync"}),
        Request::Handoff { session_id, cwd, device, continue_with, place, force } => json!({
            "t": "handoff", "session_id": session_id, "cwd": cwd, "device": device,
            "continue_with": continue_with, "place": place, "force": force
        }),
        Request::Job { spec, device } => json!({"t": "job", "spec": spec, "device": device}),
        Request::Run { spec, device } => json!({"t": "run", "spec": spec, "device": device}),
        Request::Xfer { id, device, offer, pull } => json!({"t": "xfer", "id": id, "device": device, "offer": offer, "pull": pull}),
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
            place: s("place"),
            force: v.get("force").and_then(Value::as_bool).unwrap_or(false),
        },
        "job" => Request::Job { spec: serde_json::from_value::<JobSpec>(v.get("spec")?.clone()).ok()?, device: s("device") },
        "run" => Request::Run { spec: serde_json::from_value::<RunSpec>(v.get("spec")?.clone()).ok()?, device: s("device")? },
        "xfer" => Request::Xfer {
            id: s("id")?,
            device: s("device")?,
            offer: serde_json::from_value::<XferOffer>(v.get("offer")?.clone()).ok()?,
            pull: v.get("pull")?.as_bool()?,
        },
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
            Request::Xfer {
                id: "x1".into(),
                device: "pi".into(),
                offer: XferOffer { place: "h".into(), what: crate::sync::protocol::XferWhat::Files { roots: vec!["a".into()], delete: true }, for_req: None },
                pull: true,
            },
            Request::Run { spec: RunSpec { id: "r1".into(), argv: vec!["ls".into()], ..Default::default() }, device: "pi".into() },
            Request::Handoff { session_id: "s".into(), cwd: "/x".into(), device: "pi".into(), continue_with: None, place: Some("h".into()), force: true },
        ];
        for (i, r) in reqs.iter().enumerate() {
            std::fs::write(spool.join(format!("{i}.json")), encode(r).to_string()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        let got = drain(dir.path());
        assert_eq!(got.len(), 5);
        assert!(matches!(&got[0], Request::To { device, .. } if device == "pi"));
        assert!(matches!(got[1], Request::SetAway(true)));
        assert!(matches!(&got[2], Request::Xfer { id, pull: true, offer, .. } if id == "x1" && offer.place == "h"));
        assert!(matches!(&got[3], Request::Run { spec, .. } if spec.argv == ["ls"]));
        assert!(matches!(&got[4], Request::Handoff { place: Some(p), force: true, .. } if p == "h"));
        assert!(drain(dir.path()).is_empty(), "drained files are removed");
    }
}
