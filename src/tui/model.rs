//! What the screens draw: every session nerve can see, here and on the
//! other devices, in one shape. Rebuilt each frame from discovery and the
//! sync agent's view. Pure, so the rules are testable without a terminal.

use crate::detect::jsonl::LogEntry;
use crate::state::session::{fuzzy_match, Session, SessionState};
use crate::util::sanitize::{clean, Sanitised};

/// A session's identity: its device (None for this one) and id. The
/// cursor holds one, so a re-sort never swaps the session under it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RowKey {
    pub(crate) device: Option<String>,
    pub(crate) id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Asking,
    Permission,
    Failed,
    /// Not a session: a device or folder waiting for a person's yes.
    Request,
    Working,
    Idle,
    /// Waiting for days; still alive.
    Dormant,
    /// Its process went away; shown briefly before it is dropped.
    Gone,
    /// Its device stopped reporting.
    Offline,
}

/// The three groups of the session list, in screen order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Band {
    Need,
    Working,
    Idle,
}

impl Band {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Need => "Needs you",
            Self::Working => "Working",
            Self::Idle => "Idle",
        }
    }
}

impl Status {
    pub(crate) fn band(self) -> Band {
        match self {
            Self::Asking | Self::Permission | Self::Failed | Self::Request => Band::Need,
            Self::Working => Band::Working,
            Self::Idle | Self::Dormant | Self::Gone | Self::Offline => Band::Idle,
        }
    }

    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Asking => "Asking",
            Self::Permission => "Needs permission",
            Self::Failed => "Failed",
            Self::Request => "Waiting for a yes",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Dormant => "Dormant",
            Self::Gone => "Gone",
            Self::Offline => "Offline",
        }
    }

    /// Waiting for a person: a permission prompt or a question, by Claude's own words.
    fn waiting(why: Option<&str>) -> Self {
        match crate::notify::Alert::waiting(why) {
            crate::notify::Alert::Permission => Self::Permission,
            _ => Self::Asking,
        }
    }

    fn of_ask(a: &Ask) -> Self {
        if a.permission {
            Self::Permission
        } else {
            Self::Asking
        }
    }

    fn of_local(state: &SessionState, why: Option<&str>) -> Self {
        match state {
            SessionState::Processing | SessionState::ToolRunning(_) => Self::Working,
            SessionState::WaitingForInput => Self::waiting(why),
            SessionState::Error => Self::Failed,
            SessionState::Idle => Self::Idle,
            SessionState::Dormant => Self::Dormant,
            SessionState::Vanished => Self::Gone,
        }
    }
}

/// A question or permission prompt that can be answered from nerve.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Ask {
    pub(crate) request: String,
    pub(crate) text: String,
    /// What a permission prompt wants to run or touch.
    pub(crate) command: Option<String>,
    /// Choices, in the order 1–9 picks them.
    pub(crate) options: Vec<String>,
    pub(crate) permission: bool,
}

/// A transcript as the panel shows it. Claude writes one text block into
/// several entries as it streams, so a repeat of anything already shown
/// is dropped.
pub(crate) fn conversation(entries: &[LogEntry]) -> Vec<Said> {
    let mut out: Vec<Said> = Vec::new();
    for s in entries.iter().filter_map(Said::from_entry) {
        let repeat = matches!(s, Said::Claude(_) | Said::You(_)) && out.contains(&s);
        if !repeat {
            out.push(s);
        }
    }
    out
}

/// One line of a transcript.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Said {
    You(String),
    Claude(String),
    Tool(String),
}

impl Said {
    /// Successful tool results and finished turns stay out: the panel
    /// reads as a conversation, with one quiet line per tool call.
    fn from_entry(e: &LogEntry) -> Option<Self> {
        Some(match e {
            LogEntry::UserText(t) => return Self::typed(t.as_str()),
            LogEntry::AssistantText(t) => Self::Claude(t.as_str().to_string()),
            LogEntry::ToolUse { name, detail } => {
                Self::Tool(format!("{} {}", name.as_str(), detail.as_str()).trim().to_string())
            }
            LogEntry::ToolResult { is_error: true, snippet } => Self::Tool(format!("Failed: {}", snippet.as_str())),
            LogEntry::Result { is_error: true } => Self::Tool("Turn failed".into()),
            LogEntry::ToolResult { .. } | LogEntry::Result { .. } => return None,
        })
    }

    /// A peer's tail line (`› you`, `● claude`, `▸ tool`), sanitised here
    /// since it came off the network.
    fn from_tail(line: &str) -> Option<Self> {
        let line = Sanitised::new(line);
        let line = line.as_str();
        let rest = |p: &str| line.strip_prefix(p).map(|r| r.trim().to_string());
        if let Some(t) = rest("›") {
            return Self::typed(&t);
        }
        rest("●")
            .map(Self::Claude)
            .or_else(|| rest("▸").map(Self::Tool))
            .or_else(|| line.starts_with('✗').then(|| Self::Tool("Turn failed".into())))
    }

    /// What a person typed, without the text Claude Code adds to a user
    /// turn: reminders, command output, a background task's report (one
    /// line from its summary). A peer's tail cuts lines short, so an
    /// unclosed tag runs to the end.
    fn typed(t: &str) -> Option<Self> {
        let mut t = t.to_string();
        while let Some(at) = t.find("<system-reminder>") {
            let end = t[at..].find("</system-reminder>").map_or(t.len(), |e| at + e + "</system-reminder>".len());
            t.replace_range(at..end, "");
        }
        let t = t.trim();
        let tag = |name: &str| {
            let open = format!("<{name}>");
            let at = t.find(&open)? + open.len();
            let end = t[at..].find(&format!("</{name}>")).map_or(t.len(), |e| at + e);
            Some(t[at..end].trim().to_string())
        };
        if t.starts_with("<task-notification>") {
            return Some(Self::Tool(tag("summary").unwrap_or_else(|| "A background task finished".into())));
        }
        if t.starts_with("<command-") {
            let cmd = tag("command-name")?;
            return Some(Self::You(
                format!("{cmd} {}", tag("command-args").unwrap_or_default()).trim_end().to_string(),
            ));
        }
        if t.starts_with("<bash-input>") {
            return tag("bash-input").map(|c| Self::You(format!("! {c}")));
        }
        if t.starts_with("[Request interrupted") {
            return Some(Self::Tool("Interrupted".into()));
        }
        if t.starts_with("This session is being continued from a previous conversation") {
            return Some(Self::Tool("Compacted: continued from a summary of the earlier conversation".into()));
        }
        let noise = ["<local-command-", "<bash-std", "Caveat: The messages below were generated by the user"];
        (!t.is_empty() && !noise.iter().any(|n| t.starts_with(n))).then(|| Self::You(t.to_string()))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub(crate) key: RowKey,
    pub(crate) title: String,
    pub(crate) project: String,
    pub(crate) path: String,
    pub(crate) branch: Option<String>,
    /// Claude's own figure when known, else nerve's estimate.
    pub(crate) cost: f64,
    pub(crate) status: Status,
    /// Seconds in this status.
    pub(crate) since: u64,
    /// Claude's own words for why it waits.
    pub(crate) why: Option<String>,
    pub(crate) ask: Option<Ask>,
    pub(crate) private: bool,
    pub(crate) notify: bool,
    /// Its folder is synced with other devices.
    pub(crate) synced: bool,
    /// A prompt typed here can reach it (mod, pane or tab).
    pub(crate) can_send: bool,
    /// A peer's recent transcript; local transcripts come from discovery.
    pub(crate) tail: Vec<Said>,
}

impl Row {
    pub(crate) fn is_local(&self) -> bool {
        self.key.device.is_none()
    }
}

/// Facts about local sessions that discovery doesn't carry, supplied by
/// the sync agent when it runs.
pub(crate) trait LocalFacts {
    fn ask(&self, id: &str) -> Option<Ask>;
    fn private(&self, id: &str, cwd: &std::path::Path) -> bool;
    fn notify(&self, id: &str) -> bool;
    fn can_send(&self, id: &str) -> bool;
    fn synced(&self, cwd: &std::path::Path) -> bool;
}

/// How long a session has been as it is. Discovery knows when it last
/// wrote to its transcript, which survives nerve restarting; while it
/// works, the time since nerve saw it start.
fn since_local(s: &Session, status: Status) -> u64 {
    match (status, s.activity_age_secs) {
        (Status::Working, _) | (_, None) => s.state_duration().as_secs(),
        (_, Some(age)) => age.max(0.0) as u64,
    }
}

pub(crate) fn local_row(s: &Session, facts: &dyn LocalFacts) -> Row {
    let why = s.waiting_for.as_ref().map(|w| w.as_str().to_string());
    let ask = facts.ask(s.id.as_str());
    let mut status = Status::of_local(s.state(), why.as_deref());
    if let Some(a) = &ask {
        status = Status::of_ask(a);
    }
    let path = Sanitised::new(s.cwd.to_string_lossy());
    Row {
        key: RowKey { device: None, id: s.id.as_str().to_string() },
        title: s.name().as_str().to_string(),
        project: s.project_dir().to_string(),
        private: facts.private(s.id.as_str(), &s.cwd),
        notify: facts.notify(s.id.as_str()),
        can_send: facts.can_send(s.id.as_str()),
        synced: facts.synced(&s.cwd),
        branch: s.branch.as_ref().map(|b| b.as_str().to_string()),
        cost: s.cost_usd(),
        since: since_local(s, status),
        path: clean(crate::paths::tilde(path.as_str())),
        status,
        why,
        ask,
        tail: Vec::new(),
    }
}

/// Seconds after which a quiet device counts as offline. Devices
/// republish at least once a minute.
pub(crate) const OFFLINE_AFTER_SECS: u64 = 180;

/// A peer's answerable prompt, sanitised.
pub(crate) fn ask_of(r: &crate::sync::protocol::PendingRequest) -> Ask {
    match r.questions.first() {
        Some(q) if r.kind == "question" => Ask {
            request: r.id.clone(),
            text: clean(&q.question),
            command: None,
            options: q.options.iter().take(9).map(clean).collect(),
            permission: false,
        },
        _ => Ask {
            request: r.id.clone(),
            text: format!("Allow {}?", clean(&r.tool)),
            command: Some(clean(&r.summary)).filter(|c| !c.is_empty()),
            options: vec!["Allow".into(), "Deny".into()],
            permission: true,
        },
    }
}

/// Every session a peer reported. `age` is how old that report is.
pub(crate) fn remote_rows(device: &str, age: u64, state: &crate::sync::protocol::DeviceState) -> Vec<Row> {
    let offline = age >= OFFLINE_AFTER_SECS;
    state
        .sessions
        .iter()
        .map(|s| {
            let ask = state.requests.iter().find(|r| r.session == s.id).map(ask_of);
            let why = s.waiting_for.as_deref().map(clean);
            let status = match (offline, &ask, s.state.as_str()) {
                (true, _, _) => Status::Offline,
                (_, Some(a), _) => Status::of_ask(a),
                (_, _, "Waiting") => Status::waiting(why.as_deref()),
                (_, _, "Error") => Status::Failed,
                (_, _, "Processing") => Status::Working,
                (_, _, l) if l.starts_with("Tool") => Status::Working,
                (_, _, "Dormant") => Status::Dormant,
                (_, _, "Gone") => Status::Gone,
                _ => Status::Idle,
            };
            let since = s.active_age_secs.map(|a| a as u64).unwrap_or(0) + age;
            Row {
                key: RowKey { device: Some(device.to_string()), id: s.id.clone() },
                title: clean(&s.name),
                project: crate::state::session::project_dir(std::path::Path::new(&s.cwd)).to_string(),
                path: clean(&s.cwd),
                branch: s.branch.as_deref().map(clean),
                cost: s.cost_usd,
                status,
                since,
                why,
                ask,
                private: false,
                notify: !s.notify.is_empty(),
                can_send: s.injectable && !offline,
                synced: state.folders.iter().any(|f| std::path::Path::new(&s.cwd).starts_with(&f.path)),
                tail: s.tail.iter().filter_map(|l| Said::from_tail(l)).collect(),
            }
        })
        .collect()
}

/// An approval as a row of Needs you: held on `holder` (None: here),
/// asked by `asker`, both as this device names them.
pub(crate) fn approval_row(
    a: &crate::sync::protocol::Approval,
    asker: &str,
    holder: Option<&str>,
    me: &str,
    now: u64,
) -> Row {
    use crate::sync::protocol::Asked;
    let mut a = a.clone();
    if let Asked::Folder { lands, .. } = &mut a.what {
        *lands = crate::paths::tilde(lands);
    }
    let (title, detail) = a.describe(asker, holder.unwrap_or(me));
    let (project, path) = match &a.what {
        Asked::Join { name, .. } => (name.clone(), String::new()),
        Asked::Folder { set, lands, .. } => (set.name.clone(), lands.clone()),
    };
    Row {
        key: RowKey { device: holder.map(String::from), id: a.id.clone() },
        title: clean(&title),
        project: clean(&project),
        path: clean(&path),
        branch: None,
        cost: 0.0,
        status: Status::Request,
        since: now.saturating_sub(a.at),
        why: Some(clean(&detail)),
        ask: None,
        private: false,
        notify: false,
        synced: false,
        can_send: false,
        tail: Vec::new(),
    }
}

/// The list as shown: grouped, ordered, filtered by `query` (letters in
/// order, case-insensitive, against title, project, path and device).
/// Needs you: longest waiting first. Working: longest running first.
/// Idle: most recent first, offline ones last.
pub(crate) fn arrange<'a>(rows: &'a [Row], query: Option<&str>) -> Vec<(Band, Vec<&'a Row>)> {
    let query = query.map(str::to_lowercase).filter(|q| !q.is_empty());
    let shown = |r: &&Row| {
        query.as_ref().is_none_or(|q| {
            let dev = r.key.device.as_deref().unwrap_or("");
            [r.title.as_str(), &r.project, &r.path, dev].iter().any(|t| fuzzy_match(q, t))
        })
    };
    [Band::Need, Band::Working, Band::Idle]
        .into_iter()
        .filter_map(|band| {
            let mut list: Vec<&Row> = rows.iter().filter(shown).filter(|r| r.status.band() == band).collect();
            match band {
                Band::Need | Band::Working => {
                    list.sort_by(|a, b| b.since.cmp(&a.since).then_with(|| a.key.id.cmp(&b.key.id)))
                }
                Band::Idle => list.sort_by(|a, b| {
                    (a.status == Status::Offline)
                        .cmp(&(b.status == Status::Offline))
                        .then(a.since.cmp(&b.since))
                        .then_with(|| a.key.id.cmp(&b.key.id))
                }),
            }
            (!list.is_empty()).then_some((band, list))
        })
        .collect()
}

const NUMBERS: [&str; 11] = ["No", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten"];

fn count(n: usize) -> String {
    NUMBERS.get(n).map(|s| s.to_string()).unwrap_or_else(|| n.to_string())
}

/// The bold line at the top and the dim line under it.
pub(crate) fn headline(rows: &[Row]) -> (String, String) {
    let need: Vec<&Row> = rows.iter().filter(|r| r.status.band() == Band::Need).collect();
    if let Some(oldest) = need.iter().max_by_key(|r| r.since) {
        let asks = need.iter().filter(|r| r.status == Status::Request).count();
        let some = |n: usize, one: &str, many: &str| format!("{} {}", count(n), if n == 1 { one } else { many });
        let head = match (need.len() - asks, asks) {
            (s, 0) => some(s, "session", "sessions"),
            (0, a) => some(a, "request", "requests"),
            (s, a) => {
                format!("{} and {}", some(s, "session", "sessions"), some(a, "request", "requests").to_lowercase())
            }
        };
        let on = match (&oldest.key.device, oldest.status) {
            (_, Status::Request) => String::new(),
            (Some(d), _) => format!(" on {d}"),
            (None, _) => String::new(),
        };
        return (
            format!("{head} {} you.", if need.len() == 1 { "needs" } else { "need" }),
            format!("Waiting longest: {}{on}, {}.", oldest.project, long_age(oldest.since)),
        );
    }
    let working = rows.iter().filter(|r| r.status == Status::Working).count();
    let sub = match working {
        0 if rows.is_empty() => "Sessions appear here as soon as Claude Code starts.".to_string(),
        0 => "Every session is idle.".to_string(),
        1 => "One session is working.".to_string(),
        n => format!("{} sessions are working.", count(n)),
    };
    ("Nothing needs you.".to_string(), sub)
}

/// `now`, `45s`, `3m`, `2h`, `4d`: the time column.
pub(crate) fn short_age(secs: u64) -> String {
    match secs {
        0..10 => "now".into(),
        10..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `3 minutes`, `1 hour`: for sentences.
fn long_age(secs: u64) -> String {
    let plural = |n: u64, unit: &str| if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") };
    match secs {
        0..60 => "under a minute".into(),
        60..3600 => plural(secs / 60, "minute"),
        3600..86_400 => plural(secs / 3600, "hour"),
        _ => plural(secs / 86_400, "day"),
    }
}

#[cfg(test)]
fn row_for_test(id: &str, device: Option<&str>, status: Status, since: u64) -> Row {
    Row {
        key: RowKey { device: device.map(String::from), id: id.into() },
        title: format!("task {id}"),
        project: id.into(),
        path: format!("/w/{id}"),
        branch: None,
        cost: 0.0,
        status,
        since,
        why: None,
        ask: None,
        private: false,
        notify: false,
        can_send: true,
        synced: false,
        tail: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids<'a>(bands: &[(Band, Vec<&'a Row>)]) -> Vec<(Band, Vec<&'a str>)> {
        bands.iter().map(|(b, rows)| (*b, rows.iter().map(|r| r.key.id.as_str()).collect())).collect()
    }

    #[test]
    fn groups_by_what_needs_you_and_orders_each_group() {
        let rows = vec![
            row_for_test("idle-old", None, Status::Idle, 900),
            row_for_test("ask-new", None, Status::Asking, 10),
            row_for_test("off", Some("pi"), Status::Offline, 5),
            row_for_test("work", None, Status::Working, 60),
            row_for_test("ask-old", Some("studio"), Status::Permission, 300),
            row_for_test("idle-new", None, Status::Idle, 30),
        ];
        assert_eq!(
            ids(&arrange(&rows, None)),
            vec![
                (Band::Need, vec!["ask-old", "ask-new"]),
                (Band::Working, vec!["work"]),
                (Band::Idle, vec!["idle-new", "idle-old", "off"]),
            ]
        );
    }

    #[test]
    fn find_matches_title_project_and_device_and_drops_empty_groups() {
        let rows =
            vec![row_for_test("api", None, Status::Working, 1), row_for_test("site", Some("studio"), Status::Idle, 1)];
        assert_eq!(ids(&arrange(&rows, Some("STU"))), vec![(Band::Idle, vec!["site"])]);
        assert!(arrange(&rows, Some("nothing")).is_empty());
    }

    #[test]
    fn headline_counts_in_words_and_names_the_longest_wait() {
        let rows = vec![
            row_for_test("site", Some("studio"), Status::Asking, 200),
            row_for_test("billing", None, Status::Permission, 64),
        ];
        assert_eq!(
            headline(&rows),
            ("Two sessions need you.".into(), "Waiting longest: site on studio, 3 minutes.".into())
        );
        let mut ask = row_for_test("app-runs", Some("mini"), Status::Request, 400);
        ask.title = "mini wants to sync app-runs".into();
        let both = [rows.clone(), vec![ask.clone()]].concat();
        assert_eq!(
            headline(&both),
            ("Two sessions and one request need you.".into(), "Waiting longest: app-runs, 6 minutes.".into())
        );
        assert_eq!(headline(&[ask]).0, "One request needs you.");
        let calm = vec![row_for_test("api", None, Status::Working, 1)];
        assert_eq!(headline(&calm), ("Nothing needs you.".into(), "One session is working.".into()));
        assert_eq!(headline(&[]).1, "Sessions appear here as soon as Claude Code starts.");
    }

    #[test]
    fn ages() {
        assert_eq!(
            (short_age(3), short_age(45), short_age(200), short_age(7200), short_age(200_000)),
            ("now".into(), "45s".into(), "3m".into(), "2h".into(), "2d".into())
        );
        assert_eq!(
            (long_age(30), long_age(60), long_age(3700)),
            ("under a minute".into(), "1 minute".into(), "1 hour".into())
        );
    }

    #[test]
    fn peer_rows_take_their_prompt_and_go_offline_when_quiet() {
        use crate::sync::protocol::{DeviceState, PendingRequest, Question, RemoteSession};
        let state = DeviceState {
            sessions: vec![RemoteSession {
                id: "s1".into(),
                name: "Spacing\u{1b}[2J".into(),
                cwd: "/w/site".into(),
                state: "Waiting".into(),
                injectable: true,
                tail: vec!["› hi".into(), "  ⎿ ok".into()],
                ..Default::default()
            }],
            requests: vec![PendingRequest {
                id: "r1".into(),
                session: "s1".into(),
                kind: "question".into(),
                questions: vec![Question {
                    question: "Where?".into(),
                    options: vec!["Header".into(), "All".into()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let rows = remote_rows("studio", 5, &state);
        assert_eq!(rows[0].status, Status::Asking);
        assert_eq!(rows[0].title, "Spacing", "network strings are sanitised");
        assert_eq!(rows[0].ask.as_ref().unwrap().options, ["Header", "All"]);
        assert_eq!(rows[0].tail, [Said::You("hi".into())]);
        let stale = remote_rows("studio", OFFLINE_AFTER_SECS, &state);
        assert_eq!(stale[0].status, Status::Offline);
        assert!(!stale[0].can_send);
    }

    #[test]
    fn a_transcript_keeps_what_people_typed() {
        let you = |t: &str| Said::typed(t);
        assert_eq!(you("fix it<system-reminder>be careful</system-reminder>"), Some(Said::You("fix it".into())));
        assert_eq!(you("<system-reminder>only this"), None, "a cut-off reminder is still a reminder");
        assert_eq!(
            you("<task-notification><task-id>a1</task-id><summary>Agent \"sweep\" completed</summary><result>long</result></task-notification>"),
            Some(Said::Tool("Agent \"sweep\" completed".into()))
        );
        assert_eq!(you("<command-message>review</command-message><command-name>/review</command-name><command-args>42</command-args>"), Some(Said::You("/review 42".into())));
        assert_eq!(you("<local-command-stdout>ok</local-command-stdout>"), None);
        assert_eq!(you("[Request interrupted by user]"), Some(Said::Tool("Interrupted".into())));
        assert!(
            matches!(you("This session is being continued from a previous conversation that ran out of context."), Some(Said::Tool(t)) if t.starts_with("Compacted"))
        );
        assert_eq!(
            Said::from_tail("› <task-notification><task-id>a1</task-id><sum"),
            Some(Said::Tool("A background task finished".into()))
        );
    }
}
