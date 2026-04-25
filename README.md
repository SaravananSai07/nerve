# nerve

TUI dashboard for monitoring and switching between Claude Code sessions across terminal tabs.

## What it does

- Discovers all active Claude Code sessions on your machine via `~/.claude/sessions/`
- Shows state (processing, tool running, waiting, idle, error), git branch, CPU, token usage, cost, and activity sparkline
- Jump to any session's terminal tab with one keypress (Ghostty and tmux)
- Preview session logs or capture terminal screen content
- Kill sessions with confirmation
- Desktop notifications when sessions need input or hit errors
- Filters out daemon-spawned background processes — only shows real interactive sessions
- Deduplicates by PID and TTY to prevent ghost cards

## Keybindings

| Key | Action |
|-----|--------|
| `j/k` `Up/Down` | Navigate rows |
| `h/l` `Left/Right` | Navigate columns |
| `Enter` / `g` | Go to session tab |
| `p` | Preview session log |
| `P` | Preview with terminal capture (Ghostty) |
| `x` | Kill session |
| `s` | Cycle sort: stable / state / name / age |
| `t` | Cycle theme |
| `n` | Rename session |
| `m` | Toggle notification mute |
| `1-9` | Jump to session |
| `?` | Help |
| `q` | Quit |

## Notifications

Nerve sends desktop notifications when a session transitions to **Waiting for input** or **Error**. On macOS, if [`terminal-notifier`](https://github.com/julienXX/terminal-notifier) is installed, clicking the notification activates your terminal app directly.

```toml
# ~/.config/nerve/config.toml
[notifications]
on_waiting = true   # notify when session needs input (default: true)
on_error = true     # notify on errors (default: true)
sound = true        # play sound with notification (default: true)
```

Mute at runtime with `m`. State is persisted across restarts.

## Themes

nightfox, tokyonight, catppuccin, gruvbox, dracula, rosepine

Cycle with `t` or set in config.

## Config

```toml
# ~/.config/nerve/config.toml

[general]
refresh_interval_ms = 1000

[appearance]
theme = "nightfox"

[notifications]
on_waiting = true
on_error = true
sound = true

# Override session display names by CWD
[session_names]
"/Users/you/projects/my-app" = "my-app"
```

## Install

```bash
# One-liner (installs from crates.io)
curl -fsSL https://raw.githubusercontent.com/SaravananSai07/nerve/master/install.sh | bash

# From a cloned repo (builds from source)
./install.sh

# Or manually via cargo
cargo install nerve-tui
```

The install script places the binary at `~/.cargo/bin/nerve` and, on macOS, offers to install optional extras like `terminal-notifier`. First-time install takes a few minutes (Rust toolchain if missing, plus crate compilation).

## CLI

```
nerve          # launch TUI
nerve --list   # print sessions to stdout
nerve --dump   # JSON dump of all sessions
```

## Supported terminals

- **Ghostty** — tab switching, screen capture
- **tmux** — pane switching
