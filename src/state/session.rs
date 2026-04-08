use std::path::PathBuf;
use std::time::Instant;

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_usd: f64,
    #[serde(skip)]
    pub last_file_offset: u64,
}

impl TokenUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }

    pub fn compact_display(&self) -> String {
        let total_k = self.total_tokens() as f64 / 1000.0;
        if self.cost_usd >= 0.01 {
            format!("{:.0}k/${:.2}", total_k, self.cost_usd)
        } else {
            format!("{:.0}k", total_k)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Processing,
    ToolRunning(String),
    WaitingForInput,
    Idle,
    Error,
    Stale,
}

impl SessionState {
    pub fn sort_priority(&self) -> u8 {
        match self {
            Self::Processing => 0,
            Self::ToolRunning(_) => 1,
            Self::WaitingForInput => 2,
            Self::Idle => 3,
            Self::Error => 5,
            Self::Stale => 6,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Processing => "Processing".into(),
            Self::ToolRunning(tool) => format!("Tool: {tool}"),
            Self::WaitingForInput => "Waiting".into(),
            Self::Idle => "Idle".into(),
            Self::Error => "Error".into(),
            Self::Stale => "Stale".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    pub name: String,
    pub state: SessionState,
    pub state_changed_at: Instant,
    pub tty: Option<String>,
    pub branch: Option<String>,
    pub cpu_percent: f32,
    pub current_tool: Option<String>,
    pub activity: ActivityHistory,
    pub jsonl_path: Option<PathBuf>,
    pub renamed: bool,
    pub usage: TokenUsage,
    pub pid: Option<u32>,
    pub jsonl_age_secs: Option<f64>,
    pub last_notified_state: Option<SessionState>,
    pending_state: Option<SessionState>,
    pending_count: u8,
}

const CONFIRM_TICKS: u8 = 3;

impl Session {
    pub fn new(id: String, cwd: PathBuf) -> Self {
        let name = cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into());
        Self {
            id,
            cwd,
            name,
            state: SessionState::Processing,
            state_changed_at: Instant::now(),
            tty: None,
            branch: None,
            cpu_percent: 0.0,
            current_tool: None,
            activity: ActivityHistory::new(),
            jsonl_path: None,
            renamed: false,
            usage: TokenUsage::default(),
            pid: None,
            jsonl_age_secs: None,
            last_notified_state: None,
            pending_state: None,
            pending_count: 0,
        }
    }

    pub fn propose_state(&mut self, new_state: SessionState) -> bool {
        if new_state == self.state {
            self.pending_state = None;
            self.pending_count = 0;
            return false;
        }

        if self.pending_state.as_ref() == Some(&new_state) {
            self.pending_count += 1;
            if self.pending_count >= CONFIRM_TICKS {
                self.state = new_state;
                self.state_changed_at = Instant::now();
                self.pending_state = None;
                self.pending_count = 0;
                return true;
            }
        } else {
            self.pending_state = Some(new_state);
            self.pending_count = 1;
        }
        false
    }

    pub fn set_state(&mut self, new_state: SessionState) {
        if self.state != new_state {
            self.state = new_state;
            self.state_changed_at = Instant::now();
            self.pending_state = None;
            self.pending_count = 0;
        }
    }

    pub fn state_duration(&self) -> std::time::Duration {
        self.state_changed_at.elapsed()
    }

    pub fn format_duration(&self) -> String {
        let secs = self.state_duration().as_secs();
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m {:02}s", secs / 60, secs % 60)
        } else {
            format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

impl Serialize for Session {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("Session", 14)?;
        s.serialize_field("id", &self.id)?;
        s.serialize_field("cwd", &self.cwd)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("state", &self.state)?;
        s.serialize_field("tty", &self.tty)?;
        s.serialize_field("branch", &self.branch)?;
        s.serialize_field("cpu_percent", &self.cpu_percent)?;
        s.serialize_field("current_tool", &self.current_tool)?;
        s.serialize_field("activity", &self.activity)?;
        s.serialize_field("jsonl_path", &self.jsonl_path)?;
        s.serialize_field("jsonl_age_secs", &self.jsonl_age_secs)?;
        s.serialize_field("renamed", &self.renamed)?;
        s.serialize_field("usage", &self.usage)?;
        s.serialize_field("pid", &self.pid)?;
        s.end()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivityHistory {
    buckets: [bool; 10],
    #[serde(skip)]
    last_update: Instant,
}

impl ActivityHistory {
    pub fn new() -> Self {
        Self {
            buckets: [false; 10],
            last_update: Instant::now(),
        }
    }

    pub fn record_activity(&mut self) {
        self.shift_if_needed();
        self.buckets[9] = true;
        self.last_update = Instant::now();
    }

    pub fn shift_if_needed(&mut self) {
        let elapsed = self.last_update.elapsed().as_secs();
        let shifts = (elapsed / 30).min(10) as usize;
        if shifts > 0 {
            self.buckets.rotate_left(shifts);
            for b in &mut self.buckets[(10 - shifts)..] {
                *b = false;
            }
            self.last_update = Instant::now();
        }
    }

    pub fn sparkline(&self) -> String {
        self.buckets
            .iter()
            .map(|&active| if active { '▓' } else { '░' })
            .collect()
    }
}
