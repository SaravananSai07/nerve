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
  lockfile.rs         Single-instance enforcement via flock
  log.rs              File-backed debug logging
  notify.rs           Desktop notifications (macOS)
  paths.rs            Canonical path resolution for config/data dirs
  signals.rs          UNIX signal handling (SIGINT, SIGCHLD, SIGHUP, …)
  terminal_guard.rs   Restore terminal state on panic or exit
  updater.rs          Background update-check against latest GitHub release
  detect/
    claude.rs         Session discovery: reads ~/.claude/sessions/*.json,
                      infers state from JSONL logs, deduplicates by PID then TTY
    git.rs            Git branch detection with mtime-based caching
    jsonl_cache.rs    Incremental JSONL parse cache keyed by path + mtime
    process.rs        Process table scanning via `ps -eo`
  state/
    session.rs        Session struct, SessionState enum, state machine
    registry.rs       HashMap<session_id, Session> with sort/disambiguate
    filtered_view.rs  Versioned sorted+filtered view over the registry
    log_entry.rs      Per-line preview entry parsed from a JSONL transcript
    prefs.rs          Persistent user preferences
    state_machine.rs  Generic N-tick debounce for state transitions
  platform/
    mod.rs            Bridge enum (Ghostty/Tmux variants) + auto_detect()
    ghostty.rs        Ghostty tab navigation and screen capture (macOS only)
    tmux.rs           tmux pane navigation
  tui/
    mod.rs            Module declarations + centered() overlay helper
    theme.rs          Theme struct and built-in theme palette
    cards.rs          Main grid view rendering
    help.rs           Help overlay
    rename.rs         Rename overlay
    preview.rs        Log/screen preview overlay
    status.rs         Status-bar message vocabulary
    confirm_kill.rs   Kill confirmation dialog
    confirm_preview.rs  Preview confirmation dialog
  workers/
    discovery.rs      Background thread that runs session discovery on a timer
  util/
    applescript.rs    Safe AppleScript runner via argv (macOS only)
    focus_arg.rs      Validation for --focus CLI argument
    sanitize.rs       ANSI/control-byte stripping for terminal output
    text.rs           Grapheme-aware text truncation
```

## Key data flow

1. `discover_sessions()` scans `~/.claude/sessions/*.json` + process table
2. Sessions are deduplicated: first by PID (multiple session files per process), then by TTY (multiple processes per terminal)
3. `App::refresh_sessions()` upserts into `SessionRegistry` every tick (~1s)
4. State transitions require 3 consecutive confirmations (`propose_state`) to avoid flicker
5. Sessions not found in discovery transition to `Vanished` and are evicted after a grace period; long-idle live sessions become `Dormant` and are kept indefinitely

## Conventions

- macOS-only code (Ghostty) is gated with `#[cfg(target_os = "macos")]`
- Process comm names are compared after `rsplit('/')` to handle full paths
- Session names default to CWD's last directory component; disambiguated with `(1)`, `(2)` suffixes
