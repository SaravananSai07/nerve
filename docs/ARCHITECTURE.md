# Architecture

## Module map

```
src/
  main.rs             Entry point; CLI flags (--dump, --list, --focus, update)
  app.rs              Main loop: input, draw, tty watchdog
  app/
    discovery.rs      Applies worker snapshots to the registry, dispatches notifications
    input.rs          Key handling per overlay
    overlays.rs       Overlay open/close helpers
  config.rs           TOML config (~/.config/nerve/config.toml)
  paths.rs            Canonical config/data/log paths
  lockfile.rs         Single-instance flock
  log.rs              File-backed debug log
  notify.rs           Desktop notifications (macOS)
  signals.rs          Shutdown flag + SIGCHLD reaper thread
  terminal_guard.rs   Restores the terminal on exit/panic
  updater.rs          Background latest-release check
  detect/
    mod.rs            claude_roots(): $CLAUDE_CONFIG_DIR + ~/.claude
    claude.rs         Session + background-job discovery, PID/TTY dedup, stop/kill
    statusline.rs     `nerve statusline` hook + reader for Claude's own cost/context
    jsonl.rs          Transcript tail → SessionState; token usage; preview entries
    jsonl_cache.rs    Incremental parse cache keyed by (mtime, len, inode)
    process.rs        `ps -eo` process table, cached
    git.rs            Branch lookup, mtime-cached
  state/
    session.rs        Session, SessionId, SessionState, DiscoverySnapshot
    registry.rs       HashMap<SessionId, Session>, versioned
    filtered_view.rs  Sorted + search-filtered view over the registry
    state_machine.rs  N-tick debounce for state transitions
    activity.rs       Sparkline history
    token_usage.rs    Token counts + cost estimate
    log_entry.rs      One preview line from a transcript
    prefs.rs          Persisted user prefs
  platform/
    mod.rs            Bridge enum + auto_detect(); BridgeId for --focus
    ghostty.rs        Tab focus + screen capture via AppleScript (macOS)
    tmux.rs           Pane focus + capture
  tui/                ratatui rendering: cards grid, overlays, theme, status bar
  workers/
    discovery.rs      Background thread: FS-watch + 1 Hz heartbeat → snapshots
  util/               applescript argv runner, focus-arg validation, fs helpers,
                      ANSI sanitising, grapheme-aware truncation
```

## Data flow

1. The discovery worker wakes on an FS event in any root's `sessions/` dir
   or on a 1 Hz heartbeat and calls `discover_sessions_with()`. Roots are
   `$CLAUDE_CONFIG_DIR` (if absolute) and `~/.claude`.
2. For each `<root>/sessions/<pid>.json` it:
   - checks that the PID is a live `claude` process (`ps -o ucomm`);
   - classifies the kind from `kind` / `entrypoint`: terminal, desktop,
     or background. Only terminal sessions need a tty or a shell parent;
   - resolves the session id, using `--resume <id>` from argv when present;
   - finds the transcript `<root>/projects/<cwd, non-alnum → ->/<id>.jsonl`;
   - takes the state from Claude's `status` field when present
     (authoritative), else from the transcript tail refined with mtime
     and a `caffeinate` child;
   - attaches Claude's own cost and context % from `nerve statusline`
     records, if any.
3. `<root>/jobs/*/state.json` adds parked background jobs (`blocked` /
   `working`) that have no process.
4. Snapshots are deduplicated by PID, then by TTY, and sent to the UI thread.
5. `App` upserts them into `SessionRegistry`. Authoritative states commit
   immediately; heuristic ones only stick after 3 consecutive matching
   ticks (`observe_state`).
6. A session that is missing from discovery goes to `Vanished` and is evicted
   after a grace period. A session waiting for 48 h or more goes to `Dormant`
   and is kept.

## State model

`Processing` → `ToolRunning(tool)` → `WaitingForInput` → `Idle` → `Error` →
`Dormant` → `Vanished`, listed in sort priority. Notifications fire on
committed transitions into `WaitingForInput`, `Error`, and (with
`on_complete`) `Idle`.

## CPU budget

With six sessions a healthy instance uses about 0.15 s of CPU per minute and
about 12 MB of RSS. The main loop is floored at 50 ms per iteration, the
process table is cached, and transcripts are re-parsed only when
`(mtime, len, inode)` changes.

Watch for crossterm's `try_read`: it spins on `read() == 0` once the tty
hangs up, which is why `spawn_tty_watchdog` exists. Don't remove it.

Don't poll `claude agents --json` for background jobs: each call costs
about 90 ms of CPU. The job state files carry the same data.
