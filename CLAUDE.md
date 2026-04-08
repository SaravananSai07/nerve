# Nerve

TUI dashboard for monitoring Claude Code sessions across terminal tabs/splits.

## Build & Run

```bash
cargo build              # dev build
cargo run                # launch TUI
cargo run -- --dump      # JSON dump of discovered sessions
cargo run -- --list      # one-line-per-session listing
```

Minimum Rust version: 1.75

## Architecture

```
src/
  main.rs             Entry point, --dump/--list CLI flags
  app.rs              Main loop: poll events, refresh sessions, render TUI
  config.rs           TOML config from ~/.config/nerve/config.toml
  notify.rs           Desktop notifications (macOS)
  detect/
    claude.rs         Session discovery: reads ~/.claude/sessions/*.json,
                      infers state from JSONL logs, deduplicates by PID then TTY
    process.rs        Process table scanning via `ps -eo`
  state/
    session.rs        Session struct, SessionState enum, state machine
    registry.rs       HashMap<session_id, Session> with sort/disambiguate
    prefs.rs          Persistent user preferences
  platform/
    mod.rs            Bridge trait for terminal integration
    ghostty.rs        Ghostty tab navigation and screen capture (macOS only)
    tmux.rs           tmux pane navigation
  tui/
    mod.rs            Theme definitions
    cards.rs          Main grid view rendering
    help.rs           Help overlay
    rename.rs         Rename overlay
    preview.rs        Log/screen preview overlay
    confirm_kill.rs   Kill confirmation dialog
    confirm_preview.rs  Preview confirmation dialog
```

## Key data flow

1. `discover_sessions()` scans `~/.claude/sessions/*.json` + process table
2. Sessions are deduplicated: first by PID (multiple session files per process), then by TTY (multiple processes per terminal)
3. `App::refresh_sessions()` upserts into `SessionRegistry` every tick (~1s)
4. State transitions require 3 consecutive confirmations (`propose_state`) to avoid flicker
5. Sessions not found in discovery are marked Stale, removed after 60s

## Conventions

- macOS-only code (Ghostty) is gated with `#[cfg(target_os = "macos")]`
- Process comm names are compared after `rsplit('/')` to handle full paths
- Session names default to CWD's last directory component; disambiguated with `(1)`, `(2)` suffixes
