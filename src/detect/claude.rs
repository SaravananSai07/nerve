use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::detect::git::BranchCache;
use crate::detect::jsonl_cache::JsonlCache;
use crate::detect::statusline::StatuslineCache;
use crate::detect::{jsonl, process};
use crate::state::session::{DiscoverySnapshot, SessionId, SessionKind, SessionState};
use crate::state::token_usage::TokenUsage;
use crate::util::fs::{read_capped_nofollow, read_tail_nofollow};
use crate::util::sanitize::Sanitised;

/// Cap on the size of a per-pid session JSON file. The format is small
/// (under 1 KiB in practice). Anything bigger is almost certainly garbage
/// or hostile content planted in `~/.claude/sessions/`.
const SESSION_JSON_MAX_BYTES: u64 = 64 * 1024;

/// Background-job `state.json` carries the job's opening prompt, so it runs
/// larger than a session file; still bounded.
const JOB_STATE_MAX_BYTES: u64 = 512 * 1024;

/// Caps on `read_dir` enumeration of `~/.claude/{sessions,projects,jobs}/`.
/// Real users have at most a few hundred entries; tens of thousands is
/// a runaway-state signal (claude-code bug, accidental script, junk in
/// the dir). Hitting the cap is logged so the source can be
/// investigated.
const MAX_SESSION_ENTRIES: usize = 10_000;
const MAX_PROJECT_ENTRIES: usize = 10_000;
const MAX_JOB_ENTRIES: usize = 10_000;

/// A session waiting this long is assumed abandoned rather than about to
/// be answered: 48 h.
const DORMANT_AFTER_SECS: f64 = 172_800.0;

/// Per-pid session file. Claude Code ≥ 2.1.145 also writes its own
/// `status` (`busy` / `waiting` / `idle`), the `waitingFor` reason, and
/// the session's display name; older versions only have the first three
/// fields, so everything else is optional.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionFile {
    pid: u32,
    session_id: String,
    cwd: String,
    status: Option<String>,
    waiting_for: Option<String>,
    status_updated_at: Option<u64>,
    name: Option<String>,
    kind: Option<String>,
    entrypoint: Option<String>,
}

/// Classify a session from its file's `kind` / `entrypoint`. Only kinds
/// known to mean a background job count as one: anything unrecognised
/// stays `Terminal`, so it still has to pass the tty / shell-parent filter
/// rather than slipping past it as tty-less background work.
fn kind_from_fields(kind: Option<&str>, entrypoint: Option<&str>) -> SessionKind {
    match (kind, entrypoint) {
        // The pid file writes `bg`; `claude agents` reports `background`.
        (Some("bg" | "background"), _) => SessionKind::Background,
        (_, Some("claude-desktop")) => SessionKind::Desktop,
        _ => SessionKind::Terminal,
    }
}

/// The id `claude attach|stop|logs` take: the session id's first 8 chars.
pub(crate) fn job_id(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}

/// argv that attaches a terminal to a background job.
pub(crate) fn attach_argv(session_id: &str) -> [&str; 3] {
    ["claude", "attach", job_id(session_id)]
}

/// `<root>/jobs/<id>/state.json` for a `claude --bg` session. Parked jobs
/// (blocked on the user, no live process) only exist here.
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct JobState {
    state: String,
    session_id: String,
    cwd: String,
    name: Option<String>,
    needs: Option<String>,
    detail: Option<String>,
}

impl JobState {
    /// What a blocked job is waiting on.
    fn reason(self) -> Option<String> {
        self.needs.or(self.detail)
    }
}

/// Map a background job's `state` onto nerve's states. `None` for values
/// nerve doesn't know, so other signals take over.
fn state_from_job(state: &str, tail: Option<&SessionState>) -> Option<SessionState> {
    match state {
        "working" => Some(match tail {
            Some(SessionState::ToolRunning(tool)) => SessionState::ToolRunning(tool.clone()),
            _ => SessionState::Processing,
        }),
        "blocked" => Some(SessionState::WaitingForInput),
        "done" | "stopped" => Some(SessionState::Idle),
        "failed" => Some(SessionState::Error),
        _ => None,
    }
}

fn read_job_state(root: &Root, job: &str, cache: &mut JobStateCache) -> Option<JobState> {
    let job = process::valid_session_id(job)?;
    cache.read(&root.path.join("jobs").join(job).join("state.json"))
}

/// `state.json` per job dir, re-parsed only when its (mtime, len, inode)
/// changes — the same key `JsonlCache` uses, because mtime alone misses a
/// rewrite within the filesystem's timestamp resolution. Finished jobs
/// pile up in `jobs/` and are read every scan; the files carry the job's
/// whole opening prompt, so parsing them each tick adds up.
#[derive(Default)]
pub(crate) struct JobStateCache {
    entries: HashMap<PathBuf, (FileKey, Option<JobState>)>,
}

type FileKey = (SystemTime, u64, u64);

fn file_key(path: &Path) -> Option<FileKey> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len(), m.ino()))
}

impl JobStateCache {
    fn read(&mut self, path: &Path) -> Option<JobState> {
        let Some(key) = file_key(path) else {
            self.entries.remove(path);
            return None;
        };
        if let Some((cached_key, job)) = self.entries.get(path) {
            if *cached_key == key {
                return job.clone();
            }
        }
        let job = read_capped_nofollow(path, JOB_STATE_MAX_BYTES)
            .ok()
            .and_then(|raw| serde_json::from_str::<JobState>(&raw).ok());
        self.entries.insert(path.to_path_buf(), (key, job.clone()));
        job
    }

    fn retain_present(&mut self) {
        self.entries.retain(|p, _| p.exists());
    }
}

/// One Claude config root (`~/.claude`, `$CLAUDE_CONFIG_DIR`) with its
/// project dirs listed once per scan instead of once per session.
struct Root {
    path: PathBuf,
    project_dirs: Vec<PathBuf>,
}

impl Root {
    fn new(path: PathBuf) -> Self {
        let project_dirs = list_project_dirs(&path.join("projects"));
        Self { path, project_dirs }
    }
}

/// Caches the worker keeps across scans.
pub(crate) struct ScanCaches {
    pub(crate) jsonl: JsonlCache,
    pub(crate) branch: BranchCache,
    pub(crate) statusline: StatuslineCache,
    pub(crate) jobs: JobStateCache,
}

/// One discovery pass.
pub(crate) struct Scan {
    pub(crate) sessions: Vec<DiscoverySnapshot>,
    /// Pids named by a session file but missing from the process table.
    /// A new one usually means a session started after the table was
    /// cached; the worker decides whether to re-scan processes.
    pub(crate) unseen_pids: HashSet<u32>,
}

impl ScanCaches {
    pub(crate) fn new(statusline_dir: Option<PathBuf>) -> Self {
        Self {
            jsonl: JsonlCache::new(),
            branch: BranchCache::new(),
            statusline: StatuslineCache::new(statusline_dir),
            jobs: JobStateCache::default(),
        }
    }
}

/// CLI / one-shot path. Returns snapshots directly — `--dump` and
/// `--list` work with the worker's observation, not a synthesised
/// `Session` that would carry meaningless registry-owned fields
/// (state-machine confirmation counter, empty activity history,
/// zero-duration since "now").
pub(crate) fn discover_sessions(statusline_dir: Option<PathBuf>) -> Vec<DiscoverySnapshot> {
    let table = process::ProcessTable::refreshed();
    let mut caches = ScanCaches::new(statusline_dir);
    discover_sessions_with(&table, &mut caches).sessions
}

/// Discover sessions against the cached process table, JSONL cache,
/// and branch cache. On an idle session this collapses to one
/// `stat(2)` per JSONL plus one `stat(2)` per HEAD — no reads, no
/// re-walks.
pub(crate) fn discover_sessions_with(
    table: &process::ProcessTable,
    caches: &mut ScanCaches,
) -> Scan {
    let roots: Vec<Root> = super::claude_roots()
        .into_iter()
        .filter(|r| r.exists())
        .map(Root::new)
        .collect();

    let mut sessions = Vec::new();
    let mut unseen_pids = HashSet::new();
    for root in &roots {
        scan_sessions_dir(root, table, caches, &mut sessions, &mut unseen_pids);
    }
    // Parked background jobs have no pid file; add the ones a live
    // process didn't already account for.
    let mut known: HashSet<String> = sessions.iter().map(|s| s.id.as_str().to_string()).collect();
    for root in &roots {
        scan_jobs_dir(root, caches, &mut known, &mut sessions);
    }

    // Evict cache entries whose JSONLs / cwds / ids are no longer
    // referenced — bounds the caches to the live working set
    // instead of growing forever across days of uptime.
    let seen_jsonls: HashSet<&Path> = sessions.iter().filter_map(|s| s.jsonl_path.as_deref()).collect();
    let seen_cwds: HashSet<&Path> = sessions.iter().map(|s| s.cwd.as_path()).collect();
    caches.jsonl.retain_present(|p| seen_jsonls.contains(p));
    caches.branch.retain_present(|p| seen_cwds.contains(p));
    caches.statusline.retain_present(|id| known.contains(id));
    caches.jobs.retain_present();

    Scan {
        sessions: deduplicate_sessions(sessions),
        unseen_pids,
    }
}

fn scan_sessions_dir(
    root: &Root,
    table: &process::ProcessTable,
    caches: &mut ScanCaches,
    out: &mut Vec<DiscoverySnapshot>,
    unseen_pids: &mut HashSet<u32>,
) {
    let entries = match fs::read_dir(root.path.join("sessions")) {
        Ok(e) => e,
        Err(_) => return,
    };
    for (i, entry) in entries.enumerate() {
        if i >= MAX_SESSION_ENTRIES {
            crate::log_warn!(
                "discovery: sessions dir exceeded {MAX_SESSION_ENTRIES} entries; stopping enumeration"
            );
            break;
        }
        // Log per-entry IO errors but don't bail the scan — one
        // corrupt per-pid JSON shouldn't blank the dashboard.
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                crate::log_warn!("discovery: read_dir entry error: {e}");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            if let Some(session) = load_session(&path, root, table, caches, unseen_pids) {
                out.push(session);
            }
        }
    }
}

fn list_project_dirs(projects_dir: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(projects_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut result: Vec<PathBuf> = Vec::new();
    for (i, entry) in entries.enumerate() {
        if i >= MAX_PROJECT_ENTRIES {
            crate::log_warn!(
                "discovery: projects dir exceeded {MAX_PROJECT_ENTRIES} entries; stopping enumeration"
            );
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            result.push(entry.path());
        }
    }
    result
}

fn has_tty(proc: &process::ProcessInfo) -> bool {
    proc.tty != "??" && proc.tty != "?"
}

fn load_session(
    path: &Path,
    root: &Root,
    table: &process::ProcessTable,
    caches: &mut ScanCaches,
    unseen_pids: &mut HashSet<u32>,
) -> Option<DiscoverySnapshot> {
    let sf = read_session_file(path)?;

    // O(1) lookups via the cached pid index, rather than repeated
    // linear scans through the full process table.
    let Some(proc) = table.find_by_pid(sf.pid) else {
        unseen_pids.insert(sf.pid);
        return None;
    };
    if process::comm_name(&proc.comm) != "claude" {
        return None;
    }

    let kind = kind_from_fields(sf.kind.as_deref(), sf.entrypoint.as_deref());
    // Filter out daemon-spawned claude processes (e.g. background Go
    // binaries that invoke `claude` repeatedly). A real terminal session
    // has a tty or is launched from a shell / multiplexer. Desktop and
    // background sessions are tty-less by design and identify themselves
    // through `kind` / `entrypoint`.
    if kind == SessionKind::Terminal && !has_tty(proc) {
        let parent_is_shell = table.find_by_pid(proc.ppid).is_some_and(|p| {
            matches!(process::comm_name(&p.comm), "zsh" | "bash" | "fish" | "sh" | "dash" | "csh" | "tcsh"
                         | "nu" | "tmux" | "screen")
        });
        if !parent_is_shell {
            return None;
        }
    }

    // Claude Code rewrites the per-PID session file's sessionId after a
    // --resume, but the actual transcript JSONL keeps the original id. Trust
    // the command line as the source of truth — both for finding the JSONL and
    // as nerve's canonical session id, so the registry isn't churned every
    // time Claude rewrites sf.session_id mid-run.
    //
    // Both inputs are validated through the same charset/traversal gate.
    // An attacker who can plant a session JSON file with `"sessionId":
    // "../../etc/passwd"` would otherwise drive `find_jsonl` into a
    // traversal probe; drop the session instead.
    let resolved_id = process::resume_session_id(&proc.args)
        .or_else(|| process::valid_session_id(sf.session_id.as_str()))?
        .to_string();

    let cwd = PathBuf::from(&sf.cwd);
    let name = display_name(sf.name.as_deref(), &cwd);
    let branch = caches.branch.read_or_refresh(&cwd).map(Sanitised::new);
    let jsonl_path = find_jsonl(&resolved_id, &cwd, root);

    let (tail_state, usage, jsonl_age_secs) = match &jsonl_path {
        Some(jp) => {
            let (tail, usage) = caches.jsonl.read_or_refresh(
                jp,
                |p| jsonl::read_tail_state(p).unwrap_or(SessionState::Idle),
                jsonl::parse_token_usage,
            );
            (Some(tail), usage, Some(file_age_secs(jp)))
        }
        None => (None, TokenUsage::default(), None),
    };

    // A background session's pid file reports `status: "shell"` for its
    // whole life (observed on 2.1.292), even after the job finishes. Its
    // job state.json is what `claude agents` trusts, so use that first.
    let job = (kind == SessionKind::Background)
        .then(|| read_job_state(root, job_id(&resolved_id), &mut caches.jobs))
        .flatten();
    let status_age_secs = sf.status_updated_at.map(age_from_epoch_ms).unwrap_or(0.0);
    let from_job = job
        .as_ref()
        .and_then(|j| state_from_job(&j.state, tail_state.as_ref()));
    let from_status = from_job.or_else(|| {
        sf.status
            .as_deref()
            .and_then(|s| state_from_status(s, tail_state.as_ref(), status_age_secs))
    });
    let state_authoritative = from_status.is_some();
    let detected = match (from_status, tail_state, &jsonl_path) {
        (Some(state), _, _) => state,
        (None, Some(tail), Some(jp)) => refine_with_runtime(tail, jp, sf.pid, table),
        // No status and no transcript: nothing to go on.
        _ => SessionState::Idle,
    };

    let waiting_for = match detected {
        SessionState::WaitingForInput => job
            .and_then(JobState::reason)
            .or(sf.waiting_for)
            .filter(|w| !w.is_empty())
            .map(Sanitised::new),
        _ => None,
    };
    let current_tool = match &detected {
        SessionState::ToolRunning(tool) => Some(Sanitised::new(tool.clone())),
        _ => None,
    };
    // The statusline payload is keyed by Claude's current session id,
    // which differs from `resolved_id` after a --resume.
    let official = process::valid_session_id(&sf.session_id)
        .and_then(|id| caches.statusline.read(id))
        .or_else(|| caches.statusline.read(&resolved_id));
    let tty = (kind == SessionKind::Terminal && has_tty(proc))
        .then(|| Sanitised::new(proc.tty.clone()));

    Some(DiscoverySnapshot {
        id: SessionId::new(resolved_id),
        cwd,
        name,
        tty,
        branch,
        cpu_percent: proc.cpu,
        pid: Some(sf.pid),
        detected_state: detected,
        current_tool,
        usage,
        jsonl_path,
        jsonl_age_secs,
        kind,
        waiting_for,
        official,
        state_authoritative,
    })
}

/// Claude's session name (`/rename`, auto-titles) when it set one,
/// otherwise the cwd's last component.
fn display_name(claude_name: Option<&str>, cwd: &Path) -> Sanitised {
    let name = claude_name.map(Sanitised::new).filter(|n| !n.is_empty());
    name.unwrap_or_else(|| {
        Sanitised::new(
            cwd.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".into()),
        )
    })
}

/// Map Claude's own status onto nerve's states. `busy` keeps the tool
/// name from the transcript tail when there is one; `idle` keeps a tail
/// error. Unknown values return `None` so the heuristics take over.
fn state_from_status(
    status: &str,
    tail: Option<&SessionState>,
    status_age_secs: f64,
) -> Option<SessionState> {
    match status {
        "busy" => Some(match tail {
            Some(SessionState::ToolRunning(tool)) => SessionState::ToolRunning(tool.clone()),
            _ => SessionState::Processing,
        }),
        "waiting" if status_age_secs > DORMANT_AFTER_SECS => Some(SessionState::Dormant),
        "waiting" => Some(SessionState::WaitingForInput),
        "idle" => Some(match tail {
            Some(SessionState::Error) => SessionState::Error,
            _ => SessionState::Idle,
        }),
        _ => None,
    }
}

fn age_from_epoch_ms(ms: u64) -> f64 {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now_ms.saturating_sub(ms) as f64 / 1000.0
}

fn scan_jobs_dir(
    root: &Root,
    caches: &mut ScanCaches,
    known: &mut HashSet<String>,
    out: &mut Vec<DiscoverySnapshot>,
) {
    let entries = match fs::read_dir(root.path.join("jobs")) {
        Ok(e) => e,
        Err(_) => return,
    };
    for (i, entry) in entries.enumerate() {
        if i >= MAX_JOB_ENTRIES {
            crate::log_warn!("discovery: jobs dir exceeded {MAX_JOB_ENTRIES} entries; stopping enumeration");
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                crate::log_warn!("discovery: jobs read_dir entry error: {e}");
                continue;
            }
        };
        let state_path = entry.path().join("state.json");
        if let Some(snap) = load_parked_job(&state_path, root, caches, known) {
            known.insert(snap.id.as_str().to_string());
            out.push(snap);
        }
    }
}

/// A background job that's still live (working or blocked on the user)
/// but has no process of its own — the daemon parks blocked jobs.
fn load_parked_job(
    path: &Path,
    root: &Root,
    caches: &mut ScanCaches,
    known: &HashSet<String>,
) -> Option<DiscoverySnapshot> {
    let job = caches.jobs.read(path)?;
    let id = process::valid_session_id(&job.session_id)?.to_string();
    if known.contains(&id) {
        return None;
    }
    // A parked job that's done / failed / stopped has nothing left to
    // monitor; only live ones get a card.
    if !matches!(job.state.as_str(), "blocked" | "working") {
        return None;
    }
    let detected = state_from_job(&job.state, None)?;
    let cwd = PathBuf::from(&job.cwd);
    let name = display_name(job.name.as_deref(), &cwd);
    let waiting_for = (detected == SessionState::WaitingForInput)
        .then(|| job.reason())
        .flatten()
        .filter(|w| !w.is_empty())
        .map(Sanitised::new);

    let jsonl_path = find_jsonl(&id, &cwd, root);
    let (usage, jsonl_age_secs) = match &jsonl_path {
        Some(jp) => {
            let (_, usage) = caches.jsonl.read_or_refresh(
                jp,
                |p| jsonl::read_tail_state(p).unwrap_or(SessionState::Idle),
                jsonl::parse_token_usage,
            );
            (usage, Some(file_age_secs(jp)))
        }
        None => (TokenUsage::default(), None),
    };
    let official = caches.statusline.read(&id);

    Some(DiscoverySnapshot {
        name,
        branch: caches.branch.read_or_refresh(&cwd).map(Sanitised::new),
        id: SessionId::new(id),
        cwd,
        tty: None,
        cpu_percent: 0.0,
        pid: None,
        detected_state: detected,
        current_tool: None,
        usage,
        jsonl_path,
        jsonl_age_secs,
        kind: SessionKind::Background,
        waiting_for,
        official,
        state_authoritative: true,
    })
}

/// Timeline entries are one line of state + detail each; the tail we
/// show is a few KiB.
const TIMELINE_TAIL_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
struct TimelineEntry {
    at: String,
    state: String,
    #[serde(default)]
    detail: String,
}

/// Last `max` state changes of a background job as `time  state  detail`
/// lines, from `<root>/jobs/<job>/timeline.jsonl`. Survives the daemon
/// being down and the transcript being pruned.
pub(crate) fn job_timeline(job: &str, max: usize) -> Option<String> {
    let job = process::valid_session_id(job)?;
    let path = super::claude_roots()
        .into_iter()
        .map(|r| r.join("jobs").join(job).join("timeline.jsonl"))
        .find(|p| p.exists())?;
    format_timeline(&read_tail_nofollow(&path, TIMELINE_TAIL_BYTES).ok()?, max)
}

fn format_timeline(raw: &str, max: usize) -> Option<String> {
    let lines: Vec<String> = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<TimelineEntry>(l).ok())
        .map(|e| {
            // "2026-08-03T13:49:41.690Z" → "08-03 13:49"
            let when = Sanitised::new(e.at.get(5..16).unwrap_or(&e.at).replace('T', " "));
            let detail = Sanitised::new(e.detail.replace('\n', " "));
            format!("{when}  {:<8} {detail}", Sanitised::new(e.state).as_str())
        })
        .collect();
    let start = lines.len().saturating_sub(max);
    (!lines.is_empty()).then(|| lines[start..].join("\n"))
}

fn read_session_file(path: &Path) -> Option<SessionFile> {
    let buf = read_capped_nofollow(path, SESSION_JSON_MAX_BYTES).ok()?;
    serde_json::from_str(&buf).ok()
}

/// Claude Code's transcript dir name: every non-alphanumeric character of
/// the cwd becomes `-` (so `/a/.claude/wt` → `-a--claude-wt`).
fn encode_project_dir(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn find_jsonl(session_id: &str, cwd: &Path, root: &Root) -> Option<PathBuf> {
    let jsonl_name = format!("{session_id}.jsonl");

    // Exact match: try the path Claude Code would naturally write to
    // before walking the fallback list.
    let exact = root
        .path
        .join("projects")
        .join(encode_project_dir(cwd))
        .join(&jsonl_name);
    if exact.exists() {
        return Some(exact);
    }

    // Fallback for hashed long paths and `CLAUDE_CODE_PROJECT_DIR_NAME`:
    // the dirs were listed once per scan, just stat each candidate.
    root.project_dirs
        .iter()
        .map(|dir| dir.join(&jsonl_name))
        .find(|candidate| candidate.exists())
}

/// Apply the runtime conditions (CPU%, caffeinate child, mtime age)
/// to a tail-parse result. Only used for sessions whose file carries no
/// `status` (Claude Code < 2.1.145). Pure: doesn't touch the file system
/// aside from the cheap `mtime` call.
fn refine_with_runtime(
    tail_parse: SessionState,
    path: &Path,
    pid: u32,
    table: &process::ProcessTable,
) -> SessionState {
    let mtime_age = file_age_secs(path);
    let cpu = table.find_by_pid(pid).map(|p| p.cpu).unwrap_or(0.0);
    let state = tail_parse;

    if matches!(state, SessionState::Idle | SessionState::Error) {
        return state;
    }
    if state == SessionState::WaitingForInput {
        if mtime_age <= DORMANT_AFTER_SECS || cpu > 1.0 {
            return state;
        }
        return SessionState::Dormant;
    }
    // Stick with the tail-derived Processing state only when there's
    // independent evidence the session is still doing work: a recent JSONL
    // write (Claude writes on every roundtrip / tool result) or a
    // caffeinate child (long-running task holding the system awake).
    // CPU is deliberately NOT used here — Claude's TUI burns CPU on
    // keystroke rendering, which would otherwise flip an idle session to
    // Processing whenever the user is typing.
    if mtime_age <= 300.0
        || process::has_child_named(table.procs(), table.child_map(), pid, "caffeinate")
    {
        return state;
    }
    SessionState::Idle
}

fn file_age_secs(path: &Path) -> f64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|mt| SystemTime::now().duration_since(mt).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(f64::MAX)
}

/// `claude stop` a background job; its conversation stays resumable.
/// Blocks on the CLI, so callers keep it off the UI thread.
pub(crate) fn stop_background_job(session_id: &str) -> Result<String, String> {
    let job = job_id(session_id);
    let out = std::process::Command::new("claude")
        .args(["stop", job])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("claude stop: {e}"))?;
    if out.status.success() {
        Ok(format!("stopped background job {job}"))
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        Err(Sanitised::new(msg.trim().to_string()).to_string())
    }
}

/// SIGTERM a terminal session, resolved by session id AND re-validated as
/// a `claude` process in the same function: between a separate
/// resolve-then-kill the kernel could recycle the pid to an unrelated
/// user process.
pub(crate) fn kill_by_session_id(session_id: &str) -> Result<u32, String> {
    for root in super::claude_roots() {
        let Ok(entries) = fs::read_dir(root.join("sessions")) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let sf = match read_session_file(&path) {
                Some(s) => s,
                None => continue,
            };
            if sf.session_id != session_id {
                continue;
            }

            if !is_claude_process(sf.pid) {
                return Err(format!(
                    "pid {} is no longer a claude process (likely already exited)",
                    sf.pid
                ));
            }
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(sf.pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            )
            .map_err(|e| format!("kill pid {}: {e}", sf.pid))?;
            return Ok(sf.pid);
        }
    }
    Err("session id not found in any Claude sessions dir".to_string())
}

fn is_claude_process(pid: u32) -> bool {
    process::scan_processes()
        .iter()
        .any(|p| p.pid == pid && process::comm_name(&p.comm) == "claude")
}

fn should_replace(existing: &DiscoverySnapshot, candidate: &DiscoverySnapshot) -> bool {
    candidate.detected_state.sort_priority() < existing.detected_state.sort_priority()
        || (candidate.detected_state.sort_priority() == existing.detected_state.sort_priority()
            && candidate.pid > existing.pid)
}

fn deduplicate_sessions(sessions: Vec<DiscoverySnapshot>) -> Vec<DiscoverySnapshot> {
    // Phase 1: Deduplicate by PID — multiple session files for the same
    // Claude process collapse into the one with the best state.
    let mut by_pid: HashMap<u32, DiscoverySnapshot> = HashMap::new();
    let mut no_pid: Vec<DiscoverySnapshot> = Vec::new();
    for snap in sessions {
        if let Some(pid) = snap.pid {
            by_pid
                .entry(pid)
                .and_modify(|existing| {
                    if should_replace(existing, &snap) {
                        *existing = snap.clone();
                    }
                })
                .or_insert(snap);
        } else {
            no_pid.push(snap);
        }
    }

    let pid_deduped: Vec<DiscoverySnapshot> = by_pid.into_values().chain(no_pid).collect();

    // Phase 2: Deduplicate by TTY — multiple processes on the same terminal
    // collapse into the most active one.
    let mut by_tty: HashMap<String, DiscoverySnapshot> = HashMap::new();
    for snap in pid_deduped {
        let tty = match &snap.tty {
            Some(t) if t.as_str() != "??" && t.as_str() != "?" => t.as_str().to_string(),
            _ => format!("__notty_{}", snap.id),
        };
        by_tty
            .entry(tty)
            .and_modify(|existing| {
                if should_replace(existing, &snap) {
                    *existing = snap.clone();
                }
            })
            .or_insert(snap);
    }

    by_tty.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_formats_recent_entries() {
        let raw = concat!(
            r#"{"at":"2026-08-03T13:49:41.690Z","state":"working","detail":"start"}"#, "\n",
            r#"{"at":"2026-08-03T14:00:00.000Z","state":"blocked","detail":"needs\nanswers"}"#, "\n",
        );
        let out = format_timeline(raw, 30).unwrap();
        assert_eq!(out, "08-03 13:49  working  start\n08-03 14:00  blocked  needs answers");
        assert_eq!(format_timeline(raw, 1).unwrap().lines().count(), 1);
        assert!(format_timeline("junk", 30).is_none());
    }

    #[test]
    fn project_dir_encoding_matches_claude() {
        assert_eq!(
            encode_project_dir(Path::new("/Users/me/SS/nyan/.claude/worktrees/wt-1")),
            "-Users-me-SS-nyan--claude-worktrees-wt-1"
        );
        assert_eq!(encode_project_dir(Path::new("/a/b_c d")), "-a-b-c-d");
    }

    #[test]
    fn status_busy_keeps_tail_tool_name() {
        let tail = SessionState::ToolRunning("Bash".into());
        assert_eq!(
            state_from_status("busy", Some(&tail), 0.0),
            Some(SessionState::ToolRunning("Bash".into()))
        );
        assert_eq!(
            state_from_status("busy", Some(&SessionState::Idle), 0.0),
            Some(SessionState::Processing)
        );
    }

    #[test]
    fn kind_from_session_file_fields() {
        assert_eq!(kind_from_fields(Some("interactive"), Some("cli")), SessionKind::Terminal);
        assert_eq!(kind_from_fields(None, None), SessionKind::Terminal);
        assert_eq!(kind_from_fields(Some("interactive"), Some("claude-desktop")), SessionKind::Desktop);
        assert_eq!(kind_from_fields(Some("bg"), None), SessionKind::Background);
        assert_eq!(kind_from_fields(Some("background"), None), SessionKind::Background);
        // An unknown future kind must not bypass the daemon filter.
        assert_eq!(kind_from_fields(Some("sdk"), Some("cli")), SessionKind::Terminal);
    }

    #[test]
    fn job_id_is_session_id_prefix() {
        assert_eq!(job_id("abb1a2d1-a9bc-422f-bd86-ea979ed2ccd8"), "abb1a2d1");
        assert_eq!(job_id("short"), "short");
        // A char boundary that isn't at byte 8 falls back to the whole id
        // instead of panicking.
        assert_eq!(job_id("abcdefgé-x"), "abcdefgé-x");
    }

    #[test]
    fn job_state_mapping() {
        // Regression: a live bg session whose pid file says "shell" must
        // follow the job, which finished.
        assert_eq!(state_from_job("done", None), Some(SessionState::Idle));
        assert_eq!(state_from_job("failed", None), Some(SessionState::Error));
        assert_eq!(state_from_job("blocked", None), Some(SessionState::WaitingForInput));
        assert_eq!(
            state_from_job("working", Some(&SessionState::ToolRunning("Bash".into()))),
            Some(SessionState::ToolRunning("Bash".into()))
        );
        assert_eq!(state_from_job("mystery", None), None);
        assert_eq!(state_from_status("shell", None, 0.0), None);
    }

    #[test]
    fn status_waiting_and_idle() {
        assert_eq!(state_from_status("waiting", None, 10.0), Some(SessionState::WaitingForInput));
        assert_eq!(
            state_from_status("waiting", None, DORMANT_AFTER_SECS + 1.0),
            Some(SessionState::Dormant)
        );
        assert_eq!(state_from_status("idle", None, 0.0), Some(SessionState::Idle));
        assert_eq!(
            state_from_status("idle", Some(&SessionState::Error), 0.0),
            Some(SessionState::Error)
        );
        assert_eq!(state_from_status("something-new", None, 0.0), None);
    }

    #[test]
    fn session_file_parses_new_and_legacy_shapes() {
        let new: SessionFile = serde_json::from_str(
            r#"{"pid":1,"sessionId":"s","cwd":"/x","status":"waiting","waitingFor":"permission prompt",
                "statusUpdatedAt":1,"name":"n","kind":"interactive","entrypoint":"claude-desktop","extra":true}"#,
        )
        .unwrap();
        assert_eq!(new.waiting_for.as_deref(), Some("permission prompt"));
        assert_eq!(new.entrypoint.as_deref(), Some("claude-desktop"));
        let legacy: SessionFile =
            serde_json::from_str(r#"{"pid":1,"sessionId":"s","cwd":"/x"}"#).unwrap();
        assert!(legacy.status.is_none() && legacy.name.is_none());
    }

    #[test]
    fn display_name_prefers_claude_name() {
        let cwd = Path::new("/a/proj");
        assert_eq!(display_name(Some("My task"), cwd).as_str(), "My task");
        assert_eq!(display_name(Some(""), cwd).as_str(), "proj");
        assert_eq!(display_name(None, cwd).as_str(), "proj");
    }

    /// Lay out a Claude root with one session file (and optional job
    /// state) and run discovery against a fixture process table.
    fn discover_fixture(session_json: &str, job_state: Option<&str>) -> Vec<DiscoverySnapshot> {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        std::fs::write(root.join("sessions/4242.json"), session_json).unwrap();
        if let Some(state) = job_state {
            std::fs::create_dir_all(root.join("jobs/abcd1234")).unwrap();
            std::fs::write(root.join("jobs/abcd1234/state.json"), state).unwrap();
        }
        let table = process::ProcessTable::from_procs(vec![process::ProcessInfo {
            pid: 4242,
            ppid: 1,
            tty: "??".into(),
            comm: "claude".into(),
            cpu: 0.0,
            args: "claude".into(),
        }]);
        let root = Root::new(root);
        let mut caches = ScanCaches::new(None);
        let mut out = Vec::new();
        scan_sessions_dir(&root, &table, &mut caches, &mut out, &mut HashSet::new());
        out
    }

    // Regression (seen live on 2.1.292): a background session's pid file
    // says `status: "shell"` for its whole life. The job's state.json
    // must win, or a finished/blocked job shows as Idle forever.
    #[test]
    fn background_session_follows_job_state_not_pid_status() {
        let session = r#"{"pid":4242,"sessionId":"abcd1234-0000","cwd":"/x","status":"shell","kind":"bg","entrypoint":"cli"}"#;
        let blocked = r#"{"state":"blocked","sessionId":"abcd1234-0000","cwd":"/x","needs":"pick A or B"}"#;
        let snaps = discover_fixture(session, Some(blocked));
        assert_eq!(snaps.len(), 1, "tty-less bg session must not be filtered");
        assert_eq!(snaps[0].kind, SessionKind::Background);
        assert_eq!(snaps[0].detected_state, SessionState::WaitingForInput);
        assert_eq!(snaps[0].waiting_for.as_ref().map(Sanitised::as_str), Some("pick A or B"));
        assert!(snaps[0].state_authoritative);
    }

    // Desktop sessions are tty-less with a non-shell parent; only
    // `entrypoint` distinguishes them from daemon noise.
    #[test]
    fn desktop_session_kept_daemon_noise_dropped() {
        let desktop = r#"{"pid":4242,"sessionId":"d-1","cwd":"/x","status":"busy","kind":"interactive","entrypoint":"claude-desktop"}"#;
        let snaps = discover_fixture(desktop, None);
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].kind, SessionKind::Desktop);
        assert_eq!(snaps[0].detected_state, SessionState::Processing);
        assert!(snaps[0].tty.is_none());

        let noise = r#"{"pid":4242,"sessionId":"n-1","cwd":"/x"}"#;
        assert!(discover_fixture(noise, None).is_empty());
    }

    #[test]
    fn parked_job_only_when_live() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Root { path: tmp.path().to_path_buf(), project_dirs: Vec::new() };
        let mut caches = ScanCaches::new(None);
        let known = HashSet::new();
        let write = |state: &str| {
            let p = tmp.path().join("state.json");
            std::fs::write(
                &p,
                format!(r#"{{"state":"{state}","sessionId":"abb1a2d1-x","cwd":"/a/p","name":"Plan","needs":"Answer 3 questions"}}"#),
            )
            .unwrap();
            p
        };
        let snap = load_parked_job(&write("blocked"), &root, &mut caches, &known).unwrap();
        assert_eq!(snap.kind, SessionKind::Background);
        assert_eq!(snap.detected_state, SessionState::WaitingForInput);
        assert_eq!(snap.waiting_for.as_ref().map(Sanitised::as_str), Some("Answer 3 questions"));
        assert!(load_parked_job(&write("done"), &root, &mut caches, &known).is_none());
    }
}
