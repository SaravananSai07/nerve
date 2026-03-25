# nerve

TUI dashboard for monitoring and switching between Claude Code sessions running in Ghostty terminal tabs/splits.

## What it does

- Discovers all active Claude Code sessions on your machine
- Shows their state (processing, waiting, idle, running tools), git branch, CPU, and activity sparkline
- Lets you jump directly to any session's terminal tab and split pane with one keypress

## How tab switching works

Nerve uses Ghostty's native AppleScript API to enumerate every terminal across all windows, tabs, and splits — each with its working directory and UUID. When you press Enter on a session, nerve matches the session's CWD against terminal working directories to find the right one, then calls `focus terminal id` to switch to that exact split. No calibration, no probing, no configuration.

For sessions that share a CWD (e.g. multiple splits in `/home/user/project`), nerve disambiguates by matching the session name against the terminal's title bar text.

tmux is also supported — it matches by pane CWD.

## Keybindings

| Key | Action |
|-----|--------|
| `j/k` `↑/↓` | Navigate rows |
| `h/l` `←/→` | Navigate columns |
| `Enter` / `g` | Go to session's tab |
| `n` | Rename session |
| `s` | Cycle sort mode |
| `t` | Cycle theme |
| `1-9` | Jump to session |
| `?` | Help |
| `q` | Quit |

## Themes

nightfox, tokyonight, catppuccin, gruvbox, dracula, rosepine

Cycle with `t` or set in config:

```toml
# ~/.config/nerve/config.toml
[appearance]
theme = "nightfox"
```

## Config

```toml
# ~/.config/nerve/config.toml

[general]
refresh_interval_ms = 1000

[appearance]
theme = "nightfox"

# Override session display names by CWD
[session_names]
"/Users/you/projects/my-app" = "my-app"
"/Users/you/work/api" = "api"
```

## Build

```
cargo build --release
```

## CLI

```
nerve          # launch TUI
nerve --list   # print sessions to stdout
```
