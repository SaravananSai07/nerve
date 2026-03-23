use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    Processing,
    ToolRunning(String),
    WaitingForInput,
    WaitingForPermission,
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
            Self::WaitingForPermission => 3,
            Self::Idle => 4,
            Self::Error => 5,
            Self::Stale => 6,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Processing => "Processing".into(),
            Self::ToolRunning(tool) => format!("Tool: {tool}"),
            Self::WaitingForInput => "Waiting".into(),
            Self::WaitingForPermission => "Permission".into(),
            Self::Idle => "Idle".into(),
            Self::Error => "Error".into(),
            Self::Stale => "Stale".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub pid: u32,
    pub cwd: PathBuf,
    pub name: String,
    pub state: SessionState,
    pub state_changed_at: Instant,
    pub tty: Option<String>,
    pub branch: Option<String>,
    pub cpu_percent: f32,
    pub mem_mb: f32,
    pub current_tool: Option<String>,
    pub activity: ActivityHistory,
    pub jsonl_path: Option<PathBuf>,
    pending_state: Option<SessionState>,
    pending_count: u8,
}

const CONFIRM_TICKS: u8 = 3;

impl Session {
    pub fn new(id: String, pid: u32, cwd: PathBuf) -> Self {
        let name = cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into());
        Self {
            id,
            pid,
            cwd,
            name,
            state: SessionState::Processing,
            state_changed_at: Instant::now(),
            tty: None,
            branch: None,
            cpu_percent: 0.0,
            mem_mb: 0.0,
            current_tool: None,
            activity: ActivityHistory::new(),
            jsonl_path: None,
            pending_state: None,
            pending_count: 0,
        }
    }

    pub fn propose_state(&mut self, new_state: SessionState) {
        if new_state == self.state {
            self.pending_state = None;
            self.pending_count = 0;
            return;
        }

        if self.pending_state.as_ref() == Some(&new_state) {
            self.pending_count += 1;
            if self.pending_count >= CONFIRM_TICKS {
                self.state = new_state;
                self.state_changed_at = Instant::now();
                self.pending_state = None;
                self.pending_count = 0;
            }
        } else {
            self.pending_state = Some(new_state);
            self.pending_count = 1;
        }
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

#[derive(Debug, Clone)]
pub struct ActivityHistory {
    buckets: [bool; 10],
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

    fn shift_if_needed(&mut self) {
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
