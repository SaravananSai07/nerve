//! What this device tells others: its full state, the part peers may
//! see, local.json for a viewing TUI, the notify book, and the
//! notifications a new state calls for.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use nostr_sdk::prelude::PublicKey;

use super::{away_flag, local_file, Agent};
use crate::notify::{self, Alert, Click};
use crate::notify::{NotifyBook, NotifyPrefs};
use crate::state::session::DiscoverySnapshot;
use crate::sync::inject;
use crate::sync::protocol::{self, Body, DeviceState, Envelope, RemoteSession};

const HEARTBEAT: Duration = Duration::from_secs(60);

const MIN_GAP: Duration = Duration::from_secs(2);

/// Which notifications a new state calls for. A device's first state only
/// primes: nothing that was already waiting notifies again on restart.
#[derive(Default)]
pub(super) struct Alerts {
    /// Last state label per (device, session).
    last: HashMap<(String, String), String>,
    /// Prompts already notified about, per (device, request).
    told: HashSet<(String, String)>,
    primed: HashSet<String>,
    /// (device, session) a focused nerve on this device has open: its
    /// finished turns don't notify here. From `Request::Viewing`.
    pub(super) viewing: Option<(String, String)>,
}

impl Alerts {
    /// `asks`: each approval in `state` and what it asks, in words.
    fn update(
        &mut self,
        device: &str,
        state: &DeviceState,
        asks: &[(String, String)],
        me: &str,
        prefs: &NotifyPrefs,
    ) -> Vec<(Alert, String, String)> {
        let first = self.primed.insert(device.to_string());
        let attention = prefs.attention_on(me) && !first;
        let mut due = Vec::new();
        let project = |sid: &str| {
            state
                .sessions
                .iter()
                .find(|s| s.id == sid)
                .map(|s| Path::new(&s.cwd).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                .unwrap_or_default()
        };
        for r in &state.requests {
            if self.told.insert((device.to_string(), r.id.clone())) && attention {
                let (alert, what) = match r.questions.first() {
                    Some(q) if r.kind == "question" => (Alert::Question, q.question.clone()),
                    _ => (Alert::Permission, format!("{} {}", r.tool, r.summary)),
                };
                due.push((alert, r.session.clone(), format!("{}: {what}", project(&r.session))));
            }
        }
        for (id, what) in asks {
            if self.told.insert((device.to_string(), id.clone())) && attention {
                due.push((Alert::Request, id.clone(), what.clone()));
            }
        }
        self.told.retain(|(d, id)| {
            d != device || state.requests.iter().any(|r| r.id == *id) || asks.iter().any(|(a, _)| a == id)
        });
        let busy = |label: &str| label == "Processing" || label.starts_with("Tool");
        for s in &state.sessions {
            let prev = self.last.insert((device.to_string(), s.id.clone()), s.state.clone());
            let Some(prev) = prev.filter(|p| *p != s.state) else { continue };
            let asking = state.requests.iter().any(|r| r.session == s.id);
            let name = project(&s.id);
            let why = s.waiting_for.clone();
            match s.state.as_str() {
                "Waiting" if attention && !asking => {
                    due.push((
                        Alert::waiting(why.as_deref()),
                        s.id.clone(),
                        format!("{name}: {}", why.unwrap_or_else(|| "Needs input".into())),
                    ));
                }
                "Error" if attention => {
                    due.push((
                        Alert::Failed,
                        s.id.clone(),
                        format!("{name}: {}", why.unwrap_or_else(|| "Stopped with an error".into())),
                    ));
                }
                "Idle"
                    if busy(&prev)
                        && notify::tell_finished(
                            &s.notify,
                            me,
                            self.viewing.as_ref().is_some_and(|(d, sid)| d == device && *sid == s.id),
                        ) =>
                {
                    due.push((Alert::Finished, s.id.clone(), format!("{name}: {}", s.name)));
                }
                _ => {}
            }
        }
        self.last.retain(|(d, sid), _| d != device || state.sessions.iter().any(|s| s.id == *sid));
        due
    }
}

/// A state as JSON without what ticks on every scan: the clock and the
/// ages. Counting those as a change would republish every 3 s while any
/// session is open.
fn fingerprint(state: &DeviceState) -> String {
    let mut fp = state.clone();
    fp.generated_at = 0;
    for s in &mut fp.sessions {
        s.active_age_secs = None;
    }
    serde_json::to_string(&fp).unwrap_or_default()
}

/// Transcript tail as display lines (the same parser the preview uses).
pub(super) fn tail_lines(transcript: &Path, n: usize) -> Vec<String> {
    crate::detect::jsonl::read_tail_entries(transcript, n)
        .iter()
        .map(|e| e.to_string().chars().take(240).collect())
        .collect()
}

impl Agent {
    /// Everything about this device, private sessions included; peers get
    /// `for_peers` of it.
    /// `panes`: tmux's panes, to tell which sessions take typed input.
    fn build_state(&self, panes: &[(String, String)]) -> DeviceState {
        let mods = self.hub.as_ref().map(|h| h.sessions()).unwrap_or_default();
        let mut ordered: Vec<&DiscoverySnapshot> = self.snapshots.iter().collect();
        // Stable order across publishes, so remote views don't reshuffle.
        ordered.sort_by(|a, b| a.cwd.cmp(&b.cwd).then_with(|| a.id.as_str().cmp(b.id.as_str())));
        let sessions = ordered
            .into_iter()
            .map(|s| RemoteSession {
                id: s.id.as_str().to_string(),
                name: s.name.as_str().to_string(),
                cwd: s.cwd.to_string_lossy().into_owned(),
                branch: s.branch.as_ref().map(|b| b.as_str().to_string()),
                state: s.detected_state.label(),
                waiting_for: s.waiting_for.as_ref().map(|w| w.as_str().to_string()),
                kind: format!("{:?}", s.kind).to_lowercase(),
                tool: s.current_tool.as_ref().map(|t| t.as_str().to_string()),
                cost_usd: crate::state::session::effective_cost(s.official.as_ref(), &s.usage),
                active_age_secs: s.activity_age_secs,
                injectable: mods.contains_key(s.id.as_str())
                    || (s.kind == crate::state::session::SessionKind::Terminal
                        && (s.tty.as_ref().is_some_and(|t| inject::pane_for_tty(panes, t.as_str()).is_some())
                            || cfg!(target_os = "macos"))),
                via_mod: mods.contains_key(s.id.as_str()),
                last_turn: mods.get(s.id.as_str()).and_then(|(_, t)| t.clone()),
                subagents: s
                    .jsonl_path
                    .as_deref()
                    .map(|p| crate::detect::jsonl::subagent_transcripts(p).len() as u32)
                    .unwrap_or(0),
                tail: s.jsonl_path.as_deref().map(|p| tail_lines(p, protocol::TAIL_LINES)).unwrap_or_default(),
                notify: self.book.finished_to(s.id.as_str()).iter().cloned().collect(),
            })
            .collect();
        DeviceState {
            device: self.ctx.fleet.device.clone(),
            host: crate::util::hostname(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            caps: self.caps.clone(),
            nerve_version: env!("CARGO_PKG_VERSION").into(),
            generated_at: crate::util::now_secs(),
            away: away_flag(&self.ctx.sync_dir).exists(),
            sessions,
            requests: {
                let mut r = self.mailbox.pending();
                if let Some(h) = &self.hub {
                    r.extend(h.pending());
                }
                r
            },
            runs: self.runs.values().map(|(r, _)| r.clone()).collect(),
            notify: self.book.prefs.clone(),
            folders: self.folder_statuses(),
            approvals: self.approvals.clone(),
            linked: self.peers.iter().map(|(_, k)| k.to_hex()).collect(),
            invite: self.invite.as_ref().filter(|(_, until)| crate::util::now_secs() < *until).map(|(c, _)| c.clone()),
        }
    }

    /// What peers may see: private sessions, and their prompts, stay here.
    /// A prompt from a session discovery hasn't seen yet counts as private.
    fn for_peers(&self, mut state: DeviceState) -> DeviceState {
        let visible: HashSet<&str> =
            self.snapshots.iter().filter(|s| !self.private(s)).map(|s| s.id.as_str()).collect();
        state.sessions.retain(|s| visible.contains(s.id.as_str()));
        state.requests.retain(|q| visible.contains(q.session.as_str()));
        // A folder no member has said yes to yet syncs with nobody.
        state.folders.retain(|f| f.mine.as_ref().is_none_or(|v| v.held.len() < v.with.len()));
        for f in &mut state.folders {
            f.mine = None;
        }
        state.invite = None;
        state
    }

    /// Long-lived: hand this device's full state to the view (and, through
    /// local.json, to a viewing TUI), and notify about what changed here.
    /// Viewer: read what the long-lived agent wrote.
    fn show_local(&mut self, state: &DeviceState) {
        let fp = fingerprint(state);
        if fp != self.local_fp {
            if let Err(e) =
                std::fs::write(local_file(&self.ctx.sync_dir), serde_json::to_string(state).unwrap_or_default())
            {
                self.note(format!("could not write local.json: {e}"));
            }
            self.local_fp = fp;
        }
        let me = self.ctx.fleet.device.clone();
        self.alert(&me, state);
        if let Ok(mut v) = self.view.lock() {
            v.local = Some(state.clone());
        }
    }

    fn read_local(&mut self) {
        let path = local_file(&self.ctx.sync_dir);
        let m = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if m.is_none() || m == self.local_mtime {
            return;
        }
        self.local_mtime = m;
        let state = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<DeviceState>(&s).ok());
        if let Ok(mut v) = self.view.lock() {
            v.local = state;
        }
    }

    /// Pick up notify.toml when the long-lived agent (or a hand) changed it.
    pub(super) fn reload_book(&mut self) {
        let dir = self.ctx.paths.config_dir();
        let m = std::fs::metadata(NotifyBook::file(dir)).and_then(|m| m.modified()).ok();
        if m == self.book_mtime {
            return;
        }
        self.book_mtime = m;
        self.book = NotifyBook::load(dir);
        if let Ok(mut v) = self.view.lock() {
            v.notify = self.book.clone();
        }
    }

    pub(super) fn save_book(&mut self) {
        let dir = self.ctx.paths.config_dir();
        if let Err(e) = self.book.save(dir) {
            self.note(format!("could not save notify.toml: {e}"));
        }
        self.book_mtime = std::fs::metadata(NotifyBook::file(dir)).and_then(|m| m.modified()).ok();
        if let Ok(mut v) = self.view.lock() {
            v.notify = self.book.clone();
        }
    }

    /// Show the notifications `device`'s new state calls for here.
    pub(super) fn alert(&mut self, device: &str, state: &DeviceState) {
        let Some(notifier) = &self.ctx.notifier else { return };
        let me = &self.ctx.fleet.device;
        let asks: Vec<(String, String)> = state
            .approvals
            .iter()
            .map(|a| {
                let asker = PublicKey::parse(&a.from)
                    .ok()
                    .and_then(|k| self.peer_name(&k))
                    .unwrap_or_else(|| "A device".into());
                (a.id.clone(), crate::util::sanitize::clean(a.describe(&asker, device).0))
            })
            .collect();
        let due = self.alerts.update(device, state, &asks, me, &self.book.prefs);
        if due.is_empty() {
            return;
        }
        let focus = std::fs::read_to_string(self.ctx.paths.focus_file()).ok().and_then(|s| s.trim().parse().ok());
        for (alert, session, body) in due {
            let click = Click { device: device.to_string(), session, focus: focus.clone() };
            notifier.send(&alert.title(device), &body, &click);
        }
    }

    pub(super) async fn maybe_publish(&mut self) {
        if !self.ctx.long_lived() {
            self.read_local();
        }
        if self.last_build.is_some_and(|t| t.elapsed() < MIN_GAP) && !self.publish_now {
            return;
        }
        self.last_build = Some(Instant::now());
        // tmux is a process to spawn: not on the loop the mod socket shares.
        let panes = tokio::task::spawn_blocking(inject::tmux_panes).await.unwrap_or_default();
        let full = self.build_state(&panes);
        if self.ctx.long_lived() {
            self.show_local(&full);
        }
        if self.peers.is_empty() {
            self.publish_now = false;
            return;
        }
        let state = self.for_peers(full);
        let fp = fingerprint(&state);
        let changed = self.last_state.as_deref() != Some(fp.as_str());
        let due = self.last_publish.is_none_or(|t| t.elapsed() >= HEARTBEAT);
        if !(changed || due || self.publish_now) {
            return;
        }
        self.publish_now = false;
        self.last_publish = Some(Instant::now());
        let env = Envelope::new(&self.ctx.fleet.device, Body::State(Box::new(state)));
        for (name, pk) in self.peers.clone() {
            if let Err(e) = self.t.put_state(&pk, &env).await {
                self.note(format!("publish to {name} failed: {e}"));
            }
        }
        self.last_state = Some(fp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_devices_state_does_not_renotify_a_question_here() {
        let prefs = NotifyPrefs::default();
        let mut mac = DeviceState::default();
        mac.requests.push(protocol::PendingRequest {
            id: "ad71".into(),
            session: "s1".into(),
            kind: "question".into(),
            ..Default::default()
        });
        let pi = DeviceState::default();
        let mut alerts = Alerts::default();
        alerts.update("pi", &pi, &[], "mac", &prefs);
        alerts.update("mac", &DeviceState::default(), &[], "mac", &prefs);
        assert_eq!(alerts.update("mac", &mac, &[], "mac", &prefs).len(), 1, "a new question notifies");
        alerts.update("pi", &pi, &[], "mac", &prefs);
        assert!(alerts.update("mac", &mac, &[], "mac", &prefs).is_empty(), "pi's state must not reset it");
        assert!(alerts.update("mac", &mac, &[("ap1".into(), "join".into())], "mac", &prefs).len() == 1);
        alerts.update("pi", &pi, &[], "mac", &prefs);
        assert!(alerts.update("mac", &mac, &[("ap1".into(), "join".into())], "mac", &prefs).is_empty());
    }
}
