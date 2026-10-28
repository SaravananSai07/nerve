//! The bridge between the sync agent and Claude sessions running the
//! nerve mod (`mod/nerve`, Claude Code ≥ 2.1.287).
//!
//! A mod runs inside the Claude process, so it can do what the tmux /
//! hook path can only approximate:
//! - submit a prompt with `$.prompt.submit` (queued while a turn runs;
//!   works for `claude --bg` sessions, which have no terminal);
//! - answer a permission prompt from `classic.PermissionRequest` and
//!   learn at once when the user answered locally (`next.signal`);
//! - answer `AskUserQuestion` while its dialog stays on screen, so
//!   questions no longer need away mode;
//! - report each turn's start, end and token usage as it happens.
//!
//! Each mod instance talks HTTP/1.1 over a Unix socket owned by this
//! device's agent (mode 0600 in a 0700 directory: only this user).
//! Endpoints, all JSON:
//!
//! | call | body / query | answer |
//! |---|---|---|
//! | `POST /v1/hello` | `{sid, cwd, surface, version}` | `{ok}` |
//! | `GET /v1/poll?sid=` | long-poll, up to 8 s | `{cmds: [...], status}` |
//! | `POST /v1/event` | `{sid, kind, ...}` turn.start / turn.complete | `{ok}` |
//! | `POST /v1/request` | `{sid, id, kind, tool, input}` | `{ok}` |
//! | `GET /v1/wait?sid=&id=` | long-poll, up to 2 s | `{answer}`, `{pending}` or `{gone}` |
//! | `POST /v1/resolved` | `{sid, id}` answered locally / timed out | `{ok}` |
//! | `POST /v1/ack` | `{sid, cmd, ok, msg}` a command's outcome | `{ok}` |
//!
//! Commands a poll returns: `{t:"prompt", id, text}`.
//!
//! A hook waiting for a remote answer must wait inside a `$` call: a
//! mod hook gets 10 s of its *own* time, `$.clock.sleep` counts against
//! it and an in-flight `$.http.fetch` doesn't (E21). Hence `/v1/wait`
//! rather than a sleep loop. A hook Claude abandons (the user answered
//! at the keyboard) can't make further `$` calls, so it can't say so:
//! a request whose hook stops calling `/v1/wait` for `WAIT_GONE` is
//! taken as resolved locally and withdrawn.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

use super::protocol::{self, Answer, PendingRequest, Question, TurnInfo};

/// A mod that hasn't polled for this long is treated as gone (session
/// closed, or Claude restarted without the mod).
const STALE: Duration = Duration::from_secs(20);
const POLL_WAIT: Duration = Duration::from_secs(8);
const ANSWER_WAIT: Duration = Duration::from_secs(2);
const WAIT_GONE: Duration = Duration::from_secs(6);
const MAX_BODY: usize = 1024 * 1024;

#[derive(Debug, Default)]
struct ModSession {
    cwd: String,
    surface: String,
    version: String,
    last_seen: Option<Instant>,
    queue: VecDeque<Value>,
    running: bool,
    turn_started: Option<Instant>,
    /// The status line this session's mod last received.
    status_sent: String,
    last_turn: Option<TurnInfo>,
}

#[derive(Debug, Clone)]
struct ModRequest {
    req: PendingRequest,
    /// Set by a remote answer; the hook collects it with `/v1/wait`.
    answer: Option<Answer>,
    /// The hook's last `/v1/wait`: proof it is still waiting.
    last_wait: Instant,
}

/// Things that happened on the socket, for the agent to act on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HubEvent {
    /// A prompt or answer the agent queued was carried out (or failed).
    Ack { cmd: String, ok: bool, msg: String },
    /// Progress worth republishing for: a turn started or ended, a
    /// request appeared or went away.
    Changed,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, ModSession>,
    requests: HashMap<String, ModRequest>,
    events: Vec<HubEvent>,
    status: String,
}

#[derive(Default)]
pub(crate) struct ModHub {
    inner: Mutex<Inner>,
    wake: Notify,
}

impl ModHub {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn find_sid(inner: &Inner, session: &str) -> Option<String> {
        if session.is_empty() {
            return None;
        }
        inner
            .sessions
            .iter()
            .filter(|(_, s)| s.last_seen.is_some_and(|t| t.elapsed() < STALE))
            .find(|(sid, _)| sid.as_str() == session || sid.starts_with(session))
            .map(|(sid, _)| sid.clone())
    }

    /// Is a live mod attached to this session?
    pub(crate) fn connected(&self, session: &str) -> bool {
        Self::find_sid(&self.lock(), session).is_some()
    }

    /// Queue a prompt; `cmd` comes back in a `HubEvent::Ack`.
    pub(crate) fn submit(&self, session: &str, cmd: &str, text: &str) -> bool {
        let mut g = self.lock();
        let Some(sid) = Self::find_sid(&g, session) else { return false };
        let text = super::inject::clean_text(text);
        if let Some(s) = g.sessions.get_mut(&sid) {
            s.queue.push_back(json!({"t": "prompt", "id": cmd, "text": text}));
        }
        drop(g);
        self.wake.notify_waiters();
        true
    }

    pub(crate) fn has_request(&self, id: &str) -> bool {
        self.lock().requests.contains_key(id)
    }

    /// Deliver a remote answer; false when the request is gone or was
    /// already answered.
    pub(crate) fn answer(&self, id: &str, answer: &Answer) -> bool {
        let mut g = self.lock();
        let Some(r) = g.requests.get_mut(id).filter(|r| r.answer.is_none()) else { return false };
        r.answer = Some(answer.clone());
        g.events.push(HubEvent::Changed);
        drop(g);
        self.wake.notify_waiters();
        true
    }

    pub(crate) fn pending(&self) -> Vec<PendingRequest> {
        let mut v: Vec<PendingRequest> =
            self.lock().requests.values().filter(|r| r.answer.is_none()).map(|r| r.req.clone()).collect();
        v.sort_by_key(|r| r.created_at);
        v
    }

    /// Live mod sessions: id → (running a turn now, last finished turn).
    pub(crate) fn sessions(&self) -> HashMap<String, (bool, Option<TurnInfo>)> {
        self.lock()
            .sessions
            .iter()
            .filter(|(_, s)| s.last_seen.is_some_and(|t| t.elapsed() < STALE))
            .map(|(id, s)| (id.clone(), (s.running, s.last_turn.clone())))
            .collect()
    }

    /// Events since the last call. Also withdraws requests whose hook
    /// stopped waiting (answered at the keyboard, or the session ended).
    pub(crate) fn drain_events(&self) -> Vec<HubEvent> {
        let mut g = self.lock();
        let before = g.requests.len();
        g.requests.retain(|_, r| r.last_wait.elapsed() < WAIT_GONE);
        if g.requests.len() != before {
            g.events.push(HubEvent::Changed);
        }
        std::mem::take(&mut g.events)
    }

    /// One line the mod shows under Claude's prompt (`$.ui.status`).
    pub(crate) fn set_status(&self, s: String) {
        let changed = {
            let mut g = self.lock();
            let changed = g.status != s;
            g.status = s;
            changed
        };
        if changed {
            self.wake.notify_waiters();
        }
    }

    /// Handle one request. Pure apart from the shared state, so the
    /// protocol is unit-testable without a socket.
    async fn route(&self, method: &str, path: &str, body: &Value) -> Value {
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let sid = body
            .get("sid")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| query.split('&').find_map(|kv| kv.strip_prefix("sid=").map(String::from)))
            .filter(|s| valid_sid(s));
        let Some(sid) = sid else {
            return json!({"ok": false, "error": "missing or bad sid"});
        };
        match (method, route) {
            ("POST", "/v1/hello") => {
                let mut g = self.lock();
                let s = g.sessions.entry(sid).or_default();
                s.cwd = str_field(body, "cwd", 4096);
                s.surface = str_field(body, "surface", 32);
                s.version = str_field(body, "version", 32);
                s.last_seen = Some(Instant::now());
                g.events.push(HubEvent::Changed);
                json!({"ok": true})
            }
            ("GET", "/v1/poll") => {
                let deadline = Instant::now() + POLL_WAIT;
                loop {
                    let wake = self.wake.notified();
                    {
                        let mut g = self.lock();
                        let status = g.status.clone();
                        let s = g.sessions.entry(sid.clone()).or_default();
                        s.last_seen = Some(Instant::now());
                        // Answer at once for a command or a changed status
                        // line, else at the deadline.
                        if !s.queue.is_empty() || s.status_sent != status || Instant::now() >= deadline {
                            let cmds: Vec<Value> = s.queue.drain(..).collect();
                            s.status_sent = status.clone();
                            return json!({"cmds": cmds, "status": status});
                        }
                    }
                    let _ = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), wake).await;
                }
            }
            ("POST", "/v1/event") => {
                let mut g = self.lock();
                let s = g.sessions.entry(sid).or_default();
                s.last_seen = Some(Instant::now());
                match body.get("kind").and_then(Value::as_str) {
                    Some("turn.start") => {
                        s.running = true;
                        s.turn_started = Some(Instant::now());
                    }
                    Some("turn.complete") => {
                        s.running = false;
                        let u = body.get("usage").cloned().unwrap_or(Value::Null);
                        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                        s.last_turn = Some(TurnInfo {
                            ended_at: protocol::now_secs(),
                            ms: body.get("ms").and_then(Value::as_u64).unwrap_or(0),
                            reason: str_field(body, "reason", 32),
                            model: u.get("model").and_then(Value::as_str).unwrap_or("").chars().take(64).collect(),
                            input_tokens: n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens"),
                            output_tokens: n("output_tokens"),
                        });
                    }
                    _ => {}
                }
                g.events.push(HubEvent::Changed);
                json!({"ok": true})
            }
            ("POST", "/v1/request") => {
                let id = str_field(body, "id", 32);
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return json!({"ok": false, "error": "bad id"});
                }
                let req = to_request(&id, &sid, body);
                let mut g = self.lock();
                g.requests.insert(id, ModRequest { req, answer: None, last_wait: Instant::now() });
                g.events.push(HubEvent::Changed);
                json!({"ok": true})
            }
            ("GET", "/v1/wait") => {
                let id: String = query.split('&').find_map(|kv| kv.strip_prefix("id=")).unwrap_or("").chars().take(32).collect();
                let deadline = Instant::now() + ANSWER_WAIT;
                loop {
                    let wake = self.wake.notified();
                    {
                        let mut g = self.lock();
                        if let Some(s) = g.sessions.get_mut(&sid) {
                            s.last_seen = Some(Instant::now());
                        }
                        if let Some(r) = g.requests.get_mut(&id) {
                            r.last_wait = Instant::now();
                        }
                        match g.requests.get(&id) {
                            None => return json!({"gone": true}),
                            Some(r) if r.answer.is_some() => {
                                let a = g.requests.remove(&id).and_then(|r| r.answer);
                                return json!({"answer": a});
                            }
                            Some(_) if Instant::now() >= deadline => return json!({"pending": true}),
                            Some(_) => {}
                        }
                    }
                    let _ = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), wake).await;
                }
            }
            ("POST", "/v1/resolved") => {
                let id = str_field(body, "id", 32);
                let mut g = self.lock();
                if g.requests.remove(&id).is_some() {
                    g.events.push(HubEvent::Changed);
                }
                json!({"ok": true})
            }
            ("POST", "/v1/ack") => {
                let mut g = self.lock();
                g.events.push(HubEvent::Ack {
                    cmd: str_field(body, "cmd", 64),
                    ok: body.get("ok").and_then(Value::as_bool).unwrap_or(false),
                    msg: str_field(body, "msg", 400),
                });
                json!({"ok": true})
            }
            _ => json!({"ok": false, "error": "unknown route"}),
        }
    }
}

fn valid_sid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn str_field(v: &Value, k: &str, max: usize) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").chars().take(max).collect()
}

/// The mod sends Claude's own tool input; summarise it the way the hook
/// path does so both look the same on other devices.
fn to_request(id: &str, sid: &str, body: &Value) -> PendingRequest {
    let tool = str_field(body, "tool", 128);
    let input = body.get("input").cloned().unwrap_or(Value::Null);
    let hook_like = json!({"tool_name": tool, "tool_input": input, "session_id": sid});
    let mut r = super::mailbox::to_request(id, &hook_like, protocol::now_secs());
    r.questions.iter_mut().for_each(|q: &mut Question| q.options.truncate(9));
    r
}

/// Where this device's agent listens. Inside the sync dir when the path
/// fits a sockaddr (104 bytes on macOS), else a per-user name in /tmp.
pub(crate) fn socket_path(sync_dir: &Path) -> PathBuf {
    let p = sync_dir.join("agent.sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    let tag = super::chunks::id_of(sync_dir.as_os_str().as_encoded_bytes());
    PathBuf::from(format!("/tmp/nerve-{}-{}.sock", nix::unistd::getuid(), &tag[..12]))
}

/// Serve until the runtime stops. A stale socket file is replaced.
pub(crate) async fn serve(path: PathBuf, hub: Arc<ModHub>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    loop {
        let (stream, _) = listener.accept().await?;
        let hub = hub.clone();
        tokio::spawn(async move {
            let _ = handle(stream, hub).await;
        });
    }
}

async fn handle(mut s: UnixStream, hub: Arc<ModHub>) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut tmp)).await.map_err(std::io::Error::other)??;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 16 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, path) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    let len: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Ok(());
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut tmp)).await.map_err(std::io::Error::other)??;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    let json_body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let out = hub.route(&method, &path, &json_body).await.to_string();
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
        out.len()
    );
    s.write_all(resp.as_bytes()).await?;
    s.shutdown().await
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    const SID: &str = "1ae53e2c-e818-48e5-822b-3f4816e500dd";

    #[test]
    fn prompt_round_trip_through_poll_and_ack() {
        rt().block_on(async {
            let hub = ModHub::default();
            hub.route("POST", "/v1/hello", &json!({"sid": SID, "cwd": "/w", "surface": "terminal"})).await;
            assert!(hub.connected("1ae53e2c"), "prefix of the id is enough");
            assert!(hub.submit("1ae53e2c", "c1", "run the tests\x1b[A"));
            let got = hub.route("GET", &format!("/v1/poll?sid={SID}"), &Value::Null).await;
            assert_eq!(got["cmds"][0]["t"], "prompt");
            assert_eq!(got["cmds"][0]["text"], "run the tests[A", "control keys are stripped");
            hub.route("POST", "/v1/ack", &json!({"sid": SID, "cmd": "c1", "ok": true, "msg": "submitted"})).await;
            assert!(hub.drain_events().contains(&HubEvent::Ack { cmd: "c1".into(), ok: true, msg: "submitted".into() }));
        });
    }

    #[test]
    fn requests_are_listed_answered_once_and_withdrawn() {
        rt().block_on(async {
            let hub = ModHub::default();
            let q = json!({"sid": SID, "id": "ab12", "kind": "question", "tool": "AskUserQuestion",
                "input": {"questions": [{"question": "Pick", "header": "H", "options": [{"label": "Red"}, {"label": "Blue"}]}]}});
            hub.route("POST", "/v1/request", &q).await;
            let p = hub.pending();
            assert_eq!((p[0].kind.as_str(), p[0].questions[0].options.len()), ("question", 2));
            let wait = format!("/v1/wait?sid={SID}&id=ab12");
            assert_eq!(hub.route("GET", &wait, &Value::Null).await["pending"], true, "times out unanswered");
            assert!(hub.answer("ab12", &Answer::Choose { labels: vec!["Blue".into()] }));
            assert!(!hub.answer("ab12", &Answer::Allow), "second answer finds nothing");
            assert!(hub.pending().is_empty(), "answered: no longer listed");
            let got = hub.route("GET", &wait, &Value::Null).await;
            assert_eq!(got["answer"]["labels"][0], "Blue");
            assert_eq!(hub.route("GET", &wait, &Value::Null).await["gone"], true, "collected once");

            hub.route("POST", "/v1/request", &json!({"sid": SID, "id": "cd34", "tool": "Write", "input": {"file_path": "/x"}})).await;
            assert_eq!(hub.pending()[0].summary, "/x");
            hub.route("POST", "/v1/resolved", &json!({"sid": SID, "id": "cd34"})).await;
            assert!(hub.pending().is_empty(), "a local answer withdraws it");
        });
    }

    #[test]
    fn a_request_whose_hook_stops_waiting_is_withdrawn() {
        rt().block_on(async {
            let hub = ModHub::default();
            hub.route("POST", "/v1/request", &json!({"sid": SID, "id": "ef56", "tool": "Bash", "input": {"command": "ls"}})).await;
            hub.lock().requests.get_mut("ef56").unwrap().last_wait = Instant::now() - WAIT_GONE;
            assert!(hub.drain_events().contains(&HubEvent::Changed));
            assert!(hub.pending().is_empty());
        });
    }

    #[test]
    fn a_status_change_ends_the_long_poll_early() {
        rt().block_on(async {
            let hub = Arc::new(ModHub::default());
            let poll = format!("/v1/poll?sid={SID}");
            hub.route("GET", &poll, &Value::Null).await; // first poll hands over the (empty) status
            hub.set_status("⇄ nerve · pi 1s · 1 waiting on another device".into());
            let h = hub.clone();
            let t = Instant::now();
            let got = tokio::spawn(async move { h.route("GET", &format!("/v1/poll?sid={SID}"), &Value::Null).await }).await.unwrap();
            assert!(t.elapsed() < Duration::from_secs(1), "returned at once");
            assert!(got["status"].as_str().unwrap().contains("1 waiting"));
            let h = hub.clone();
            let waiter = tokio::spawn(async move { h.route("GET", &format!("/v1/poll?sid={SID}"), &Value::Null).await });
            tokio::time::sleep(Duration::from_millis(100)).await;
            hub.set_status("⇄ nerve · pi 2s".into());
            let got = tokio::time::timeout(Duration::from_secs(1), waiter).await.expect("woken by the change").unwrap();
            assert_eq!(got["status"], "⇄ nerve · pi 2s");
        });
    }

    #[test]
    fn turn_events_record_usage() {
        rt().block_on(async {
            let hub = ModHub::default();
            hub.route("POST", "/v1/event", &json!({"sid": SID, "kind": "turn.start"})).await;
            assert!(hub.sessions()[SID].0);
            hub.route("POST", "/v1/event", &json!({"sid": SID, "kind": "turn.complete", "ms": 4115, "reason": "answer",
                "usage": {"input_tokens": 2, "output_tokens": 4, "cache_read_input_tokens": 100, "model": "claude-haiku-5-5"}})).await;
            let (running, turn) = hub.sessions()[SID].clone();
            let t = turn.unwrap();
            assert!(!running);
            assert_eq!((t.ms, t.input_tokens, t.output_tokens, t.model.as_str()), (4115, 102, 4, "claude-haiku-5-5"));
        });
    }

    #[test]
    fn bad_input_is_refused() {
        rt().block_on(async {
            let hub = ModHub::default();
            assert_eq!(hub.route("POST", "/v1/hello", &json!({"sid": "../x"})).await["ok"], false);
            assert_eq!(hub.route("POST", "/v1/request", &json!({"sid": SID, "id": "zz/.."})).await["ok"], false);
            assert!(!hub.submit("nobody", "c", "hi"));
        });
    }

    // The real socket, as the mod reaches it.
    #[test]
    fn serves_http_over_a_unix_socket() {
        rt().block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = socket_path(dir.path());
            let hub = Arc::new(ModHub::default());
            tokio::spawn(serve(path.clone(), hub.clone()));
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut s = UnixStream::connect(&path).await.unwrap();
            let body = json!({"sid": SID, "cwd": "/w"}).to_string();
            let req = format!("POST /v1/hello HTTP/1.1\r\nHost: nerve\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            s.write_all(req.as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            assert!(out.starts_with("HTTP/1.1 200") && out.ends_with("{\"ok\":true}"), "{out}");
            assert!(hub.connected(SID));
            let mode = std::fs::metadata(&path).unwrap().permissions();
            assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777, 0o600);
        });
    }
}
