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
- the clipboard fallback and `claude attach` in a tmux split;
- SIGTERM reaching only the targeted session;
- nerve exiting after its tty dies (the watchdog).

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
printf '[notifications]\non_waiting = true\non_complete = true\nsound = false\n' > $T/cfg/config.toml
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
| Prompt: *Use the AskUserQuestion tool to ask me whether I prefer tea or coffee.* | Card: `Waiting (input needed)`; `$T/notifications.log` gets `…: input needed` |
| Answer it, then: *Run this bash command: curl -sI https://example.com* | Card: `Waiting (permission prompt)`; notification says so |
| Approve it | Card goes busy, then `Idle`; a `… is done` notification |
| In nerve: `/` to search, then `x` and `y` | Status shows `sending SIGTERM…`, then `sent SIGTERM (pid N)`; the process exits |
| `claude --resume <id>` in the same folder, then `x` and `y` again | Kill works on the resumed session too |

### Background job

```bash
cd $T/bg && claude --bg --model haiku --permission-mode bypassPermissions \
  "Use the Bash tool to run exactly: for i in \$(seq 1 60); do echo tick \$i; sleep 3; done"
```

| Step | Expect |
|---|---|
| Find its card | Kind `background`, state `Processing` / `Tool: Bash` (cross-check `claude agents --json`) |
| A job that asks a question | `Waiting (<its question>)`, matching `claude agents` state `blocked` |
| `Enter` in tmux | A split opens running `claude attach <job>`, attached to the job |
| `Enter` outside tmux | Status: ``copied `claude attach <job>` — paste it in any terminal``; the command is on the clipboard |
| `p` | The transcript, or a `TIMELINE` of state changes if there is none |
| `x` and `y` | `stopping…`, then `stopped background job <job>`; `claude agents --json --all` shows `stopped` |

Clean up with `claude stop <id>` and `claude rm <id>` for each job.

### Statusline hook

```bash
SET='{"statusLine":{"type":"command","command":"NERVE_CONFIG_DIR='$T'/cfg '$NERVE' statusline"}}'
cd $T/wait && claude --model haiku --settings "$SET"
```

Send one prompt. Claude's status bar shows `Haiku 4.5 · ctx N% · $X`, and
the nerve card and `NERVE_CONFIG_DIR=$T/cfg $NERVE --list` show the same
`ctx N% | $X`. `--settings` applies only to that session; your
settings.json is untouched.

### Ghostty (macOS)

Run nerve and a Claude session side by side in a new Ghostty window
(split right), with nerve started as in the setup above. Make the Claude
session ask a question (tea or coffee, as above), then focus nerve.

| Step | Expect |
|---|---|
| `Enter` on the Claude card | Focus moves to the Claude split |
| The waiting notification | Its `-execute` is `nerve --focus 'ghostty:<id>'`; running that command focuses the split |
| `P`, accept the prompt | A `LIVE` preview of the Claude screen (the question is visible); focus returns to nerve |
| `p` | A `LOG` preview showing your prompt |
| `x` and `y` | SIGTERM; the Claude process exits |

You can check focus without looking:
`osascript -e 'tell application "Ghostty" to get id of focused terminal of selected tab of front window'`.
`P` depends on Ghostty's accessibility layout, which changed in 1.3, so
recheck it after Ghostty upgrades. It also needs Accessibility permission
for the terminal running nerve.

### Desktop app (macOS)

With a session open in the Claude desktop app, its card shows `desktop`.
`Enter` brings the app forward; `x` is refused with *close it in the
Claude app*.
