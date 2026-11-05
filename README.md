# nerve

A TUI for watching, answering and steering Claude Code sessions across
terminal tabs, tmux panes and your other devices. It works with Claude
Code sessions only, for now.

## What it does

- Discovers every Claude Code session on your machine: terminal CLI sessions, Claude desktop-app sessions, and `claude --bg` background jobs (including parked ones waiting on you)
- Reads Claude's own `~/.claude` and `$CLAUDE_CONFIG_DIR` (both, if they differ)
- Puts what needs you first: one list grouped into **Needs you** (questions, permission prompts, failures, longest-waiting first), **Working** and **Idle**, under a one-line summary such as "Two sessions need you."
- Opens any session in a side panel: what it is asking (answer with `1`–`9`), its recent transcript, its folder, branch and cost, and a line to type a prompt into
- Jump to a session with one keypress: its terminal tab (Ghostty, tmux), the Claude app, or `claude attach` for a background job
- Stop terminal sessions and background jobs, after asking
- Desktop notifications when a session asks, wants permission or fails, and when one finishes a turn. Clicking one opens nerve on that session
- Filters out daemon-spawned helper processes and deduplicates by PID and TTY to prevent ghost sessions

Status is a shape, so it reads without colour: ● asking, ◆ wants
permission, ▲ failed, a spinner while working, ○ idle, ◌ offline or
gone. Amber is kept for what needs you and blue for the cursor. ⊡ marks
a session kept private, ⚑ one that notifies when it finishes, and ⇄
one whose folder is synced with other devices. Requests from your other
devices (a new device, a new folder) wait under Needs you too. The time
on the right is how long a working session's turn has run, and for the
rest, how long since its last activity.

## Keybindings

Press `?` inside nerve for the in-app list. The bottom line always shows
the keys that work right now.

| Key | Action |
|-----|--------|
| `j/k` `↑/↓` | Move between sessions (the cursor stays on its session when the list reorders) |
| `Enter` `l` `→` | Open the session's panel: beside the list at 110 columns or wider, full screen below that |
| `h` `←` `Esc` | Close the panel |
| `PgUp` `PgDn` `Home` `End` | Scroll the transcript. The mouse wheel scrolls it under the pointer and moves through the list beside it (in tmux, with `set -g mouse on`). Hold Shift to select text |
| `1`–`9` | Answer the question or permission prompt under the cursor |
| `y` `n` | Allow or refuse the request under the cursor (`Enter` reviews it first) |
| `i` | Write a prompt to the session (`Enter` sends, or queues it while a turn runs; `⌥Enter` starts a new line) |
| `n` | Who hears when this session finishes: tick other devices, or none to switch it off. Ticking the same devices as Settings follows Settings again |
| `p` | Keep the session, or its whole folder, private to this device |
| `t` | Jump to its terminal tab, the Claude app, or `claude attach` for a background job (a tmux split, else copied to the clipboard) |
| `x` | Stop it: SIGTERM for a terminal session, `claude stop` for a background job (`y` confirms, any other key cancels) |
| `r` | Rename the session |
| `P` | Capture its terminal screen into the panel (Ghostty, tmux) |
| `H` | Hand it to another device |
| `s` | Start a session on any device: pick the device, a folder there and the prompt |
| `/` | Find: letters in order, case-insensitive, across title, project, folder and device |
| `Tab` | Sessions, Sync, Settings |
| `?` | All keys |
| `q` | Quit (`Ctrl+C` quits from anywhere) |

On the Sync page, `a` syncs a folder, `s` syncs the chosen one now, `o`
sets what may sync next without asking, `v` puts back a file a sync
replaced, and `x` stops syncing it on this device.

On the Settings page, `c` connects a device, `l` connects the one under
the cursor to more of your devices, `f` forgets it, `← →` switch column
(or theme), `Space` toggles and `u` makes a private folder or session
visible again.

## Notifications

A session that asks a question, wants permission or fails notifies every
device unless you untick it under **Needs you** in Settings. Every
session's finished turn notifies the devices ticked under **Finished
turns**, however short the turn. `n` on a session overrides that for the
session alone: switch it off, or pick other devices. The session you
have open in nerve, while its terminal has focus, doesn't notify that
device when it finishes: you are watching it.

The title says what and where (`Question on laptop`, `Finished on
server`) and the body names the project. On macOS, with
[`terminal-notifier`](https://github.com/julienXX/terminal-notifier)
installed, clicking it raises nerve and opens that session, lit up for
a moment. Until you connect another device, only this device's sessions
notify; after that, the background agent notifies for every device.

```toml
# ~/.config/nerve/config.toml
[notifications]
sound = true        # play a sound with each notification (default: true)
```

Who hears about what is kept in `~/.config/nerve/notify.toml`.

## Themes

The default, `terminal`, uses your terminal's own colours, so it follows
a light or dark terminal theme. Six more built-ins: nightfox, tokyonight,
catppuccin, gruvbox, dracula, rosepine. Pick one on the Settings page
(`← →` on Theme) or set it in config.

### Custom themes

Drop a TOML file at `~/.config/nerve/themes/<name>.toml` and it
appears after the built-ins:

```toml
# ~/.config/nerve/themes/inkpot.toml
name = "inkpot"         # optional, defaults to the file name
text = "#cdcecf"        # titles, transcript
dim = "#9098a4"         # everything secondary: times, folders, hints
attention = "#dbc074"   # what needs you
accent = "#719cd6"      # where the cursor is
selection = "#2a313a"   # optional: behind the cursor's row
flash = "#3a3424"       # optional: behind a row that just arrived
```

Themes written for earlier versions still load: `muted`, `waiting`,
`processing` and `selected_bg` stand in for `dim`, `attention`, `accent`
and `selection`, and the other old keys are ignored.

## Config

```toml
# ~/.config/nerve/config.toml

[general]
refresh_interval_ms = 1000        # UI refresh tick
process_scan_interval_ms = 5000   # how often the `ps` snapshot refreshes

[appearance]
theme = "terminal"                # any built-in or user theme name

[notifications]
sound = true

[updates]
check_on_launch = true            # quiet daily check vs crates.io

# Override session display names by CWD
[session_names]
"/Users/you/projects/my-app" = "my-app"
```

A parse error in this file shows on the bottom line at start, and
defaults fill in for any missing field. `prefs.toml` keeps the theme
picked in Settings and "don't ask again" answers; it is schema-versioned
and migrated automatically across upgrades.

## Exact cost and context %

Claude Code hands its own figures (cost, context-window fill, model) to
its statusline command. Point the statusline at nerve and the session
panel shows Claude's cost instead of an estimate (`nerve --list` adds the
context-window fill):

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
| `~/.config/nerve/prefs.toml` | Persistent UI prefs (theme, "don't ask again") |
| `~/.config/nerve/notify.toml` | Who gets which notifications |
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
`nerve-tui` because `nerve` was taken on crates.io), with cross-device
sync included. The install script places the binary at `~/.cargo/bin/nerve`
and, on macOS, offers to install optional extras like `terminal-notifier`.
First-time install takes a few minutes (Rust toolchain if missing, plus
crate compilation).

Running the sync agent as a service, upgrades, uninstalling and
troubleshooting are in [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

### Updating

```bash
nerve update                             # in-app: re-runs cargo install nerve-tui --locked --force
cargo install nerve-tui --locked --force # equivalent, manual form
```

By default, nerve checks crates.io once a day and mentions a newer version on the bottom line when it starts. The installer prompts to opt out; or set `check_on_launch = false` under `[updates]` in `~/.config/nerve/config.toml`.

## CLI

```
nerve          # launch TUI
nerve update   # upgrade to the latest crates.io release
nerve --list   # print sessions to stdout
nerve --dump   # JSON dump of all sessions
nerve --land <device>/<session>   # open the running TUI on a session (what a notification click runs)
nerve statusline [-- <cmd>]   # Claude statusLine hook (see above)
nerve help     # every command, the device and folder ones included
```

## Your devices

Connect your other devices (a laptop, a desktop, a home server) and
their sessions join the one list, named by device, with the same keys:
answer, prompt, stop, open. A session you keep private (`p`) never
leaves its device. Everything travels encrypted to one device at a time,
through Nostr relays, with or without Tailscale.

Everything below works from the TUI and from a shell, so a person or an
agent can do all of it.

### Connect a device

In nerve, open Settings and press `c`. One device shows a code, the
other enters it, and you allow the join on the first, after checking
that both show the same three words.

```bash
nerve connect                 # show a code for the new device
nerve connect nerve1…         # on the new device: join with it
nerve requests                # on any of your devices: the join waits here
nerve allow <id>              # or: nerve refuse <id>
```

A code works once, for 10 minutes, and connects nothing by itself.
`nerve connect` also sets the device up the first time: its key, the
Claude Code mod and the background agent.

Who the new device joins is yours to choose: `--with all` (the default:
every device, and devices you add later), `--with now` (the devices there
now), or `--with studio,pi`. On the new device, `--with` picks among the
devices the code offers.

```bash
nerve devices                 # who is connected, and online
nerve link pi all             # connect pi to every device, and later ones
nerve link pi studio,mini     # or to some
nerve forget pi               # drop it from all your devices
```

### Start a session on a device

`s` starts Claude on any of your devices: pick the device, a folder
there (`~` is its home, or name a place) and the prompt. It runs in a
tmux session there and joins the list like any other, so you can answer
it, prompt it and attach to it. Until the device reports it, it shows at
once under the cursor as Starting, with its prompt and how far it got. If
the device refuses it or never answers, it moves to Needs you with the
reason: `r` retries, `e` edits the prompt first, `x` dismisses it.

The prompt box wraps long lines and grows to a dozen lines before it
scrolls; `⌥↵`, `⇧↵` or `Ctrl+J` starts a new line and a paste keeps its
line breaks. The panel's prompt box (`i`) works the same way.

```bash
nerve sync start studio '~/code/app' "fix the failing test"
nerve sync start studio '~/code/app' "run the tests, sum up" --wait [--timeout 600]
```

It prints the session id, then the tmux session (`tmux attach -t
nerve-…` on that device). `--wait` waits for its first turn to end and
prints Claude's reply. The folder must exist on that device, inside its
home or a place, and not be private.

### Sync folders

Off until you choose a folder. On the Sync page press `a`: pick the
folder, the devices, when it syncs, and what those devices may sync next
without asking.

```bash
nerve sync ~/code/site --with studio [--when turn|watch|manual] [--others ask|inside|any]
nerve sync site               # sync it now
nerve folders                 # how each stands
nerve unsync site             # stop syncing it here; the others keep their copies
```

**When** is `turn` (a session there finishes a turn, the default),
`watch` (files change) or `manual`. **What went where** lists every file
that moved, from which device to which, and its progress while it moves.

### Requests

A device can't make another one sync a folder on its own. A new folder
that no rule covers waits for a person's yes, under Needs you on all
your devices, and any of them can answer: `y` allows it, `n` refuses it,
`Enter` chooses where it lands and what may follow.

Each synced folder carries a rule for the folders its devices ask for
next. Change it with `o` on the Sync page.

| Rule | Other folders |
|---|---|
| `ask` (default) | wait for a yes |
| `inside` | inside the same parent folder sync without asking |
| `any` | sync without asking, unless private |

A sync you start in the TUI is your yes. One started from a shell (by
you, a script or an agent) waits on the other devices:

```bash
nerve requests
nerve allow <id> [--at ~/elsewhere] [--others inside]
nerve refuse <id>
```

Private folders, keys and credentials (`~/.ssh`, `~/.gnupg`, `~/.aws`
and the like) and nerve's and Claude's own settings never sync, whatever
the rule.

### Undo a sync

Files a sync replaces or deletes are kept for 48 hours
(`keep_versions_hours` in fleet.toml), stored by content hash so
versions share what they have in common. `v` on the Sync page, or:

```bash
nerve versions [site]
nerve restore <id>            # it then syncs like any change
```

Setting up relays and services, and every `fleet.toml` option:
[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md). The protocol and consent model:
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#sync).

## Supported terminals

- **Ghostty** — tab switching, screen capture
- **tmux** — pane switching, screen capture, `claude attach` in a split
- **Claude desktop app** — sessions are shown; `t` brings the app forward (macOS)

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
