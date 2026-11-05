# Testing

Three layers, from cheapest to most realistic. CI runs the first two; the
third uses real Claude sessions and your desktop, so it's a manual
checklist for changes that touch discovery, actions or the bridges.

## 1. Unit and integration tests

```bash
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

These cover parsing, state mapping and the UI actions (`app::app_tests`
drives `App` through the same `apply_discovery` path production uses).
Discovery tests build a fake Claude root in a tempdir with a fixture
process table, so they never read your real `~/.claude`.

## 2. Linux end-to-end (Docker)

```bash
docker build -t nerve-linux-e2e tests/linux
docker run --rm -v "$PWD":/src:ro nerve-linux-e2e bash /e2e.sh
```

Runs in about a minute after the first build. It uses Rust 1.85 (the MSRV)
with real procps, inotify, tmux and an X11 clipboard (Xvfb + xclip), plus
stand-ins for `claude` and `notify-send`, so no Claude account is needed.
It checks:

- the build, clippy and tests on Linux at the MSRV, with `--locked`;
- `ps` parsing, both config roots (`~/.claude`, `$CLAUDE_CONFIG_DIR`), and
  dropping daemon-spawned `claude` processes;
- new sessions appearing within 2 s via inotify in both roots;
- notifications carrying the waiting reason;
- the clipboard fallback (with the job's `CLAUDE_CONFIG_DIR` pinned) and
  `claude attach` in a tmux split;
- the session panel naming a background job's folder and what it waits for;
- SIGTERM reaching only the targeted session;
- nerve exiting after its tty dies (the watchdog), including when the
  tmux server is killed outright.

To iterate faster, keep the build and crate caches between runs:

```bash
docker run --rm -v "$PWD":/src:ro \
  -v nerve-linux-target:/target -v nerve-linux-cargo:/usr/local/cargo/registry \
  nerve-linux-e2e bash /e2e.sh
```

CI runs it as the `linux-e2e` job.

## 3. Manual checks with real Claude sessions

Use these when changing state detection, actions, or the Ghostty / tmux
bridges. They cost a few cents of Haiku usage.

### Setup

Work in throwaway directories and a separate nerve config, so nothing
touches your real sessions or settings:

```bash
T=$(mktemp -d); mkdir -p $T/cfg $T/wait $T/bg $T/bin
cargo build --release; NERVE=$PWD/target/release/nerve
# Record notifications instead of showing them:
printf '#!/bin/sh\necho "$*" >> %s/notifications.log\n' $T > $T/bin/terminal-notifier
chmod +x $T/bin/terminal-notifier
printf '[notifications]\nsound = false\n' > $T/cfg/config.toml
```

Claude only runs in trusted folders: start `claude` once in each of
`$T/wait` and `$T/bg` and accept the prompt. Run nerve with
`PATH=$T/bin:$PATH NERVE_CONFIG_DIR=$T/cfg $NERVE`, keeping your usual
`CLAUDE_CONFIG_DIR` if you set one.

Only one nerve can run at a time (it holds a lock): quit your normal
instance first, and after `pkill -x nerve` wait for it to exit before
starting another.

### Terminal session (tmux)

```bash
tmux -L nt new-session -d -s main -c $T/wait "claude --model haiku"
tmux -L nt new-window -d -n nerve "PATH=$T/bin:\$PATH NERVE_CONFIG_DIR=$T/cfg $NERVE"
tmux -L nt attach
```

| Step | Expect |
|---|---|
| Prompt: *Use the AskUserQuestion tool to ask me whether I prefer tea or coffee.* | The row moves to **Needs you** with ● and its options under it; `$T/notifications.log` gets `Question on <host>` |
| `1` in nerve | The answer reaches Claude and the row goes back to **Working** |
| `Tab` to Settings, tick this device under **Finished turns**, `Tab` back | Every row carries ⚑ |
| *Run this bash command: curl -sI https://example.com* | ◆ in **Needs you**; a `Permission on <host>` notification |
| Approve it, with the panel closed (`Esc`) | The row goes to **Working**, then **Idle**; a `Finished on <host>` notification (none while the session is open in a focused nerve) |
| In nerve: `/` to find it, then `x` and `y` | The footer asks `Stop <name>?`, then says `Stopped <name>.`; the process exits |
| `claude --resume <id>` in the same folder, then `x` and `y` again | Stop works on the resumed session too |

### Background job

```bash
cd $T/bg && claude --bg --model haiku --permission-mode bypassPermissions \
  "Use the Bash tool to run exactly: for i in \$(seq 1 60); do echo tick \$i; sleep 3; done"
```

| Step | Expect |
|---|---|
| Find its row | Under **Working** (cross-check `claude agents --json`) |
| A job that asks a question | Under **Needs you** with its question, matching `claude agents` state `blocked` |
| `t` in tmux | A split opens running `claude attach <job>`, attached to the job |
| `t` outside tmux | The footer says `Copied CLAUDE_CONFIG_DIR=… claude attach <job>. Paste it in any terminal.`; the command is on the clipboard |
| `Enter` | The panel shows the job's folder, its question and the transcript |
| Start nerve with the *other* `CLAUDE_CONFIG_DIR`, then `x` and `y` | Still stops the job: CLI calls target the job's own config dir |
| `x` and `y` | `Stopped <name>.`; `claude agents --json --all` shows `stopped` |

Clean up with `claude stop <id>` and `claude rm <id>` for each job.

### Statusline hook

```bash
SET='{"statusLine":{"type":"command","command":"NERVE_CONFIG_DIR='$T'/cfg '$NERVE' statusline"}}'
cd $T/wait && claude --model haiku --settings "$SET"
```

Send one prompt. Claude's status bar shows `Haiku 4.5 · ctx N% · $X`,
`NERVE_CONFIG_DIR=$T/cfg $NERVE --list` shows the same `ctx N% | $X`, and
the session's panel shows the same cost. `--settings` applies only to that session; your
settings.json is untouched.

### Ghostty (macOS)

Run nerve and a Claude session side by side in a new Ghostty window
(split right), with nerve started as in the setup above. Make the Claude
session ask a question (tea or coffee, as above), then focus nerve.

| Step | Expect |
|---|---|
| `t` on the Claude row | Focus moves to the Claude split |
| The waiting notification | Its `-execute` is `nerve --land <host>/<id> --focus 'ghostty:<id>'`; running that command focuses the split, and nerve opens the session, lit up |
| `P`, then `y` | The panel shows the Claude screen (the question is visible); focus returns to nerve |
| `Esc` | Back to the transcript, showing your prompt |
| `x` and `y` | SIGTERM; the Claude process exits |

You can check focus without looking:
`osascript -e 'tell application "Ghostty" to get id of focused terminal of selected tab of front window'`.
`P` depends on Ghostty's accessibility layout, which changed in 1.3, so
recheck it after Ghostty upgrades. It also needs Accessibility permission
for the terminal running nerve.

### Desktop app (macOS)

With a session open in the Claude desktop app, `t` brings the app
forward; `x` is refused with *A desktop session: close it in the Claude
app.*

### Devices, requests and synced folders

Three devices on one machine, each with its own `HOME` and
`NERVE_CONFIG_DIR` and a private relay (`nerve sync serve-relay`). Pass
`--no-service --no-claude` to every scratch `nerve connect`, or it
installs a launchd job and the Claude mod for that scratch device. Run
beta's and gamma's agents headless (`nerve sync agent`) and open the
TUI as alpha.

| Step | Expect |
|---|---|
| Settings, `c`, Show a code | The code, `It works once, for 10 minutes` |
| `nerve connect <code> --no-service --no-claude` (as beta) | Beta prints three words; alpha's box turns into `beta wants to connect` with the same words |
| `y` | Beta prints `Connected to alpha`; Settings lists beta |
| `nerve connect --with now --no-wait …` (alpha), then on gamma's TUI: `c`, Enter a code, paste it | Gamma offers `[×] beta`; Enter shows the words; alpha's Needs you shows `gamma wants to connect` |
| `y` on that row | Gamma joins alpha and beta; their fleet.toml lists `closed = ["gamma"]` |
| `nerve sync <dir> --with alpha` (as beta) | Alpha's Needs you: `beta wants to sync <dir>`, with where it lands and its size; beta's Sync page: `Waiting for a yes for alpha` |
| `↵`, `↵`, `j`, `↵` | It lands, and both fleet.toml folders say `others = "inside"` |
| `nerve sync <sibling dir> --with alpha` (as beta) | Syncs at once, no request |
| `a` on the Sync page, through the four steps | The other device joins without asking |
| Edit a file on beta | It reaches alpha; `v` on alpha lists `Replaced by beta`; `↵` puts it back and beta gets it too |
| Change the same file on both within 2 seconds | Both keep both versions, one as `<file>.conflict-<device>` |
| `f` on gamma in Settings, then `y` | Alpha and beta both drop gamma |
| `x` on a folder, then `y` | Alpha stops syncing; beta drops the folder too (`nerve folders`) |
| `p` on a session in a synced folder, then the folder option | The box warns `Stops syncing <path>.`; keeping it private stops syncing |
