# nerve

TUI dashboard for monitoring and switching between Claude Code sessions across terminal tabs.

## What it does

- Discovers every Claude Code session on your machine: terminal CLI sessions, Claude desktop-app sessions, and `claude --bg` background jobs (including parked ones waiting on you)
- Reads Claude's own `~/.claude` and `$CLAUDE_CONFIG_DIR` (both, if they differ)
- Shows state (processing, tool running, waiting, idle, error) straight from Claude's session status, plus *why* a session is waiting (e.g. "permission prompt", a background job's open questions)
- Shows git branch, CPU, activity sparkline, and cost — Claude's own cost and context-window % when the statusline hook is set up, otherwise an estimate from the transcript
- Jump to a session with one keypress: its terminal tab (Ghostty, tmux), the Claude app, or `claude attach` for a background job
- Preview session logs or capture terminal screen content
- Kill terminal sessions / stop background jobs with confirmation
- Desktop notifications when sessions need input or hit errors
- Filters out daemon-spawned helper processes and deduplicates by PID and TTY to prevent ghost cards

## Keybindings

Press `?` inside nerve for the live, in-app version.

| Key | Action |
|-----|--------|
| `j/k` `↑/↓` | Navigate rows |
| `h/l` `←/→` | Navigate columns |
| `1-9` | Jump to nth session |
| `Enter` | Go to the session: terminal tab, Claude app, or `claude attach` for background jobs (a tmux split, else copied to the clipboard) |
| `p` | Preview session log (a background job's timeline when it has no transcript) |
| `Shift+P` | Preview live terminal capture (Ghostty/tmux; other sessions show the log) |
| `n` | Rename session |
| `x` | Kill a terminal session / `claude stop` a background job (Enter / Esc cancel; `y` confirms) |
| `s` | Cycle sort: stable → state → name → age |
| `t` | Cycle theme |
| `/` | Search (fuzzy, case-insensitive) |
| `Esc` | Clear search filter |
| `m` | Toggle notification mute |
| `u` | Dismiss update banner (until next version) |
| `?` | Toggle help |
| `q` | Close current overlay (or quit from main view) |
| `Ctrl+C` | Quit nerve unconditionally |

### Preview overlay scroll keys

| Key | Action |
|-----|--------|
| `j/k` `↑/↓` | Scroll one line |
| `PgUp/PgDn` | Scroll one page |
| `g` / `G` | Top / bottom |
| `p` / `Esc` / `q` | Close preview |

## Notifications

Nerve sends desktop notifications when a session transitions to **Waiting for input** or **Error**. When Claude reports why it's waiting, the notification says so ("my-app: permission prompt"). On macOS, if [`terminal-notifier`](https://github.com/julienXX/terminal-notifier) is installed, clicking the notification activates your terminal app directly.

```toml
# ~/.config/nerve/config.toml
[notifications]
on_waiting = true   # notify when session needs input (default: true)
on_error = true     # notify on errors (default: true)
sound = true        # play sound with notification (default: true)
```

Mute at runtime with `m`. State is persisted across restarts.

## Themes

Six built-ins: nightfox, tokyonight, catppuccin, gruvbox, dracula, rosepine.
Cycle with `t` or set in config.

### Custom themes

Drop a TOML file at `~/.config/nerve/themes/<name>.toml` and it
appears in the cycle after the built-ins:

```toml
# ~/.config/nerve/themes/inkpot.toml
name = "inkpot"               # optional, defaults to file stem
border = "#71839b"
text = "#cdcecf"
processing = "#81b29a"
waiting = "#dbc074"
idle = "#63717f"
muted = "#9098a4"             # chrome text (status hints, log nums)
error = "#c94f6d"
stale = "#50565b"
selected_bg = "#2a313a"
selected_text = "#eaebec"
```

#### Color keys

| Key | Used for |
|-----|----------|
| `border` | Card borders, dividers |
| `text` | Primary text (card titles, message bodies) |
| `processing` | Active session colour; success status messages |
| `waiting` | Waiting-for-input session colour; section accents |
| `idle` | Idle session state colour. **Reserved** — don't use for chrome text |
| `muted` | Chrome text: status hints, log line numbers, secondary card text, modal hints. *Optional; falls back to `idle` with a warning* |
| `error` | Error session state colour; destructive-action hints |
| `stale` | Dormant / vanished sessions |
| `selected_bg` | Background of the currently-selected card |
| `selected_text` | Foreground text on the selected card |

## Config

```toml
# ~/.config/nerve/config.toml

[general]
refresh_interval_ms = 1000        # UI refresh tick
process_scan_interval_ms = 5000   # how often the `ps` snapshot refreshes

[appearance]
theme = "nightfox"                # any built-in or user theme name

[notifications]
on_waiting = true
on_error = true
sound = true

[updates]
check_on_launch = true            # quiet daily check vs crates.io

# Override session display names by CWD
[session_names]
"/Users/you/projects/my-app" = "my-app"
```

A loud parse error on this file is surfaced as a status-bar
message; defaults fill in for any missing field. The schema-
versioned `prefs.toml` (mute state, "don't ask again" flags) is
migrated automatically across upgrades.

## Exact cost and context %

Claude Code hands its own figures (cost, context-window fill, model) to
its statusline command. Point the statusline at nerve and the cards show
those instead of estimates — context % turns amber at 80%:

```jsonc
// ~/.claude/settings.json (or $CLAUDE_CONFIG_DIR/settings.json)
"statusLine": { "type": "command", "command": "nerve statusline" }
```

Already have a statusline? Wrap it — nerve records the payload and
passes it through, so your line is unchanged:

```jsonc
"statusLine": { "type": "command", "command": "nerve statusline -- ~/.claude/statusline.sh" }
```

Without a wrapped command, `nerve statusline` prints a compact
`model · ctx 42% · $1.23` line. Records live in
`~/.config/nerve/statusline/` and are pruned after 7 days.

## Files and lifecycle

| Path | Purpose |
|------|---------|
| `~/.config/nerve/config.toml` (or `~/Library/Application Support/nerve/` on macOS) | User config |
| `~/.config/nerve/prefs.toml` | Persistent UI prefs (mute, "don't ask again") |
| `~/.config/nerve/themes/*.toml` | User-provided themes |
| `~/.config/nerve/nerve.log` | Rolling warn/error log (~1 MiB max, rotated to `nerve.log.1`) |
| `~/.config/nerve/nerve.lock` | Exclusive `flock(2)` — a second `nerve` invocation refuses to start |
| `~/.config/nerve/update_cache.json` | Throttled update-check state |
| `~/.config/nerve/statusline/` | Claude's statusline payloads, written by `nerve statusline` |
| `~/.claude/{sessions,projects,jobs}/` | Read-only — Claude Code's own data (also under `$CLAUDE_CONFIG_DIR`) |

Override the config directory via the `NERVE_CONFIG_DIR`
environment variable (intended for tests and unusual layouts).

## Behaviour on signals and terminal close

- `q` / `Ctrl-C` inside the TUI: clean shutdown, terminal restored.
- `SIGHUP`, `SIGTERM`, `SIGINT`, `SIGQUIT`: handled cooperatively.
  The main loop checks a shared atomic at the top of every
  iteration and exits within ≤ 1 s.
- Host terminal tab closed without `q`: a `tcgetpgrp(stdin)`
  canary in the main loop detects the lost controlling tty and
  exits cleanly even when Ghostty fails to deliver `SIGHUP`. A
  watchdog thread force-exits within ~2.5 s if the main loop is
  stuck in crossterm's read (which spins on a hung-up tty).
- Panic: a panic hook restores cooked mode + leaves the alt-screen
  before printing the panic, so the user's terminal is never left
  in a frozen state.

## Watched files

Each Claude `sessions/` dir (`~/.claude` and `$CLAUDE_CONFIG_DIR`)
is watched via FSEvents (macOS) / inotify (Linux). Idle nerve sleeps until either a session file changes
or the fallback 1 Hz heartbeat fires — CPU usage on a quiet
machine is near zero.

## Install

```bash
# One-liner (installs from crates.io)
curl -fsSL https://raw.githubusercontent.com/SaravananSai07/nerve/master/install.sh | bash

# From a cloned repo (builds from source)
./install.sh

# Or manually via cargo (--locked keeps dependencies on the tested versions)
cargo install nerve-tui --locked
```

Every method installs the command as `nerve` (the crate is called
`nerve-tui` because `nerve` was taken on crates.io). The install script places the binary at `~/.cargo/bin/nerve` and, on macOS, offers to install optional extras like `terminal-notifier`. First-time install takes a few minutes (Rust toolchain if missing, plus crate compilation).

### Updating

```bash
nerve update                             # in-app: re-runs cargo install nerve-tui --locked --force
cargo install nerve-tui --locked --force # equivalent, manual form
```

By default, nerve checks crates.io once a day and shows a quiet banner at the top of the TUI when a newer version is available. The installer prompts to opt out; or set `check_on_launch = false` under `[updates]` in `~/.config/nerve/config.toml`.

## CLI

```
nerve          # launch TUI
nerve update   # upgrade to the latest crates.io release
nerve --list   # print sessions to stdout
nerve --dump   # JSON dump of all sessions
nerve statusline [-- <cmd>]   # Claude statusLine hook (see above)
```

## Supported terminals

- **Ghostty** — tab switching, screen capture
- **tmux** — pane switching, screen capture, `claude attach` in a split
- **Claude desktop app** — sessions are shown; Enter brings the app forward (macOS)

## Development

```bash
cargo run                                   # launch the TUI from source
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

[`docs/TESTING.md`](docs/TESTING.md) covers the Linux end-to-end suite
(Docker) and the manual checks against real Claude sessions, Ghostty and
the desktop app. [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) describes
how discovery and the UI fit together.
