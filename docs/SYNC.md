# Cross-device sync (`--features sync`)

Watch and drive Claude Code sessions running on your other devices: a
laptop, a Raspberry Pi, a server. From any device you can:

- see every device's sessions and when each device last synced;
- type an instruction into a session running elsewhere;
- answer its permission prompts and `AskUserQuestion` questions;
- read its diff, transcript tail and subagent work;
- move a session to another device and carry on there;
- send a job to whichever device has what the job needs.

Transport is [Nostr](https://github.com/nostr-protocol/nips): every
device dials *out* to one or more relays over WebSocket, so it works
behind NAT with **or without** Tailscale. Everything is NIP-44
encrypted to the receiving device; relays only see ciphertext, kinds,
timestamps and public keys.

The research behind this design is in `research/sync/`:
- `REPORT.md`: findings, measurements and the comparison of 48 alternatives;
- `DECISIONS.md`: why each choice was made;
- `experiments/`: raw results.

## Set up

One command per device. On the first device:

```bash
./install.sh --sync            # from the repo: builds with sync, then runs `nerve setup`
nerve setup                    # if nerve is already built with --features sync
```

It creates the device's key, installs the nerve mod into Claude Code
(2.1.287 or later), installs the background agent (launchd on macOS,
a systemd user unit on Linux, a detached process plus Termux:Boot
script on Termux), and prints a one-time invite code and QR.

On every other device:

```bash
./install.sh nerve1…           # or: nerve setup nerve1…
```

That joins: both devices trust each other, and the joiner adopts the
inviter's relays. A code works once, for an hour. Make another with
`nerve sync invite`. Check a device with `nerve sync doctor`.

Flags: `--name N`, `--relay URL` (repeatable), `--no-service`,
`--no-claude`, `--no-wait`. Re-running `nerve setup` is safe.

The pieces underneath are still available: `nerve sync init`, `pair`,
`relay add|rm` and `install-hooks`.

### The nerve mod

`nerve setup` registers `<config>/sync/mod` as a local plugin
marketplace and installs `nerve@nerve-local`. New Claude sessions load
it; restart older ones to pick it up. It connects to the agent's Unix
socket (`<config>/sync/agent.sock`, mode 0600) and:

- submits prompts sent from other devices with `$.prompt.submit`
  (queued while a turn runs; works for `claude --bg` sessions);
- offers permission prompts and `AskUserQuestion` questions to other
  devices while Claude's own dialog stays on screen. The first answer
  wins, and an answer at the keyboard withdraws the request;
- reports each turn's duration and token use;
- shows `⇄ nerve · pi 4s · 1 waiting on another device` under the prompt.

Sessions without the mod (older Claude, `--bare`, started before
setup) fall back to tmux/Ghostty paste and the settings hook, as before.
The Devices view tags each session `mod`, `tty` or `view`.

### Relays

| setup | how | measured RTT | notes |
|---|---|---|---|
| Public relays (default) | what `nerve setup` configures | **0.5–0.9 s** | No VPN at all. Relays see metadata. |
| + private relay | `nerve sync serve-relay` on the Pi, then `nerve sync relay add ws://<pi-tailscale-ip>:7777` on each device | **~16 ms** | Bulk data (hand-offs, diffs) goes only here when connected. In memory. |

Public relays that worked for fresh keys in testing:
- relay.primal.net
- relay.snort.social
- relay.nostr.net (lossy)

nos.lol and nostr.mom acknowledged events but never delivered them.

## Run

`nerve setup` installs the agent as a service, so there is nothing to
start by hand. The TUI attaches to that agent as a viewer; on a device
without the service, `nerve` starts an agent itself. The status bar
shows `⇄ pi 4s · 1 needs you`.

Run only **one agent per device key**. Two agents holding the same key
both act on every command.

## Use it

In the TUI, press `D` for Devices. At 90 columns or more, the device
list is on the left and the selected device on the right; on narrower
terminals (a phone over SSH) the list becomes a tab strip:

```
╭ Devices  1 needs you ─────────────────────────────────────────────────────────╮
│ ● mac  published 2s ago  · 1 session(s) via mod                     relays ●● │
│                              │ ● pi  synced 3s ago  linux aarch64             │
│▌● pi                         │   can run: linux aarch64 tmux git cargo        │
│    synced 3s ago · 1 waiting │                                                │
│    2 sessions, 1 active      │ NEEDS YOU                                      │
│                              │ ▶ demo wants to run Write  12s                 │
│ ● pixel                      │       /home/pi/demo/tui.txt                    │
│    synced 1m ago             │                                                │
│    0 sessions                │ SESSIONS  2                                    │
│                              │   ● demo        waiting: permission  ⎇ main  mod│
│ → pi 4153487f                │   ● notes       idle                       tty │
│ ██████████████░░░░░░░ 61/101 │                                                │
│ 13:52:00Z pi ✓ …: answered 46bf0e3df7a5a6b7                                   │
│ a allow  x deny  Tab device  r sync  Esc close                                │
╰───────────────────────────────────────────────────────────────────────────────╯
```

Keys in the Devices view:

| key | on | does |
|---|---|---|
| `Tab` / `←` `→` | | next / previous device |
| `j/k` | | move |
| `a` (or Enter) / `x` | permission | allow / deny |
| `1`-`9` / `x` | question | choose an option / decline |
| `m` (or Enter) | session | send a message to it |
| `d` / `l` / `g` | session | fetch its diff / transcript / subagent work |
| `r` | | sync now: republish here and ask every peer to |
| `A` | | toggle away mode on *this* device (only matters for sessions without the mod) |

On the main grid, `H` hands the selected session to another device.

The same features are available from the CLI:

```bash
nerve sync doctor                               # is this device set up and talking?
nerve sync invite                               # a one-time code for another device
nerve sync status                               # last synced, sessions, pending prompts
nerve sync send pi 4c14e2fb "run the tests again"
nerve sync answer pi 15a12dee3382bc05 allow     # or deny, or an option label
nerve sync fetch pi 4c14e2fb diff
nerve sync handoff cc6fb097 pi --then "carry on with the next step"
nerve sync job --needs macos --verify 'xcodebuild test ...' "ios tests" -- "fix the failing UI test"
nerve sync away on
```

## How each feature works

### Progress and "last synced"

- Each agent scans local sessions (the same discovery the TUI uses) every
  3 s. It republishes its **device state** when anything changes, and at
  least once a minute.
- The state includes:
  - sessions: state, waiting reason, cwd, branch, cost, subagent count and an 8-line transcript tail;
  - pending prompts;
  - jobs;
  - capabilities;
  - away flag.
- It is an *addressable* Nostr event (kind 30078, one per recipient). The
  relay keeps only the newest copy, so a laptop that has been shut keeps
  showing what it was doing when it went away, with the time.
- "Last synced" is the time the peer generated that state.
- `r` / `nerve sync sync` republishes immediately and asks peers to do the same.

### Sending an instruction

- The receiving agent finds the session's tmux pane (by tty) or Ghostty
  tab (through AppleScript `input text`).
- It checks that the PID is still a live `claude` process, then pastes the
  text as a bracketed paste and presses Enter.
- Control characters are stripped, so a peer can't send Escape or Ctrl-keys.
- Instructions older than 5 minutes are refused instead of typed. Typing
  is not idempotent, so a replay must never land.
- Background (`claude --bg`) and desktop-app sessions have no pane, so
  they are view-only.

### Answering prompts and questions

With the mod (see above), both permissions and questions go remote while
the dialog stays on screen. Without it, the settings hook works as
follows.

`nerve sync hook` is a Claude `PermissionRequest` hook and a `PreToolUse`
hook on `AskUserQuestion`.

**Permission prompts.** The dialog stays on screen while the hook waits,
so a local keypress and a remote answer race. Whichever arrives first
wins. The hook notices a local answer in the transcript and withdraws its
request.

**Questions.** The question dialog is *hidden* until the hook returns.
Questions therefore go remote only in **away mode** (`A`, or `nerve sync
away on`), or with `remote_questions = "always"`. Otherwise the person at
the keyboard couldn't answer.

**Timeouts and fallback.** The hook gives up after `hook_timeout_secs`
(default 600) and Claude asks locally as usual. With no agent running,
the hook returns at once.

### Handoff

`H` or `nerve sync handoff` starts a short exchange:

1. **Offer.** The source names the session and the repo, by its root commit.
2. **Have.** The target answers with the commit tips its copy of that repo
   already has (none for a new checkout).
3. **Pack.** The source bundles only the commits beyond those tips, then
   chunks the working-tree patch, untracked files, the transcript and its
   subagent sidecar. Chunking is content-defined (2/8/32 KiB, SHA-256 ids).
4. **Push.** It sends the index, then only chunks the target isn't known to
   hold. The target asks for anything missing.
5. **Unpack.** The target restores the repo at the `path_map`-mapped path or
   under `handoff_root`, refusing to overwrite uncommitted work. It installs
   the transcript under the slug Claude derives from the *new* path, and runs
   `claude --resume <id> --fork-session` in a detached tmux session
   `nerve-<id>`, typing `--then <text>` if given.
6. **Remember.** Both sides record which chunks the other now holds, so the
   next hand-off sends only what changed. A 700 KiB session one turn and one
   commit later moved 16–19 KiB (E22).

Chunks live in `<config>/sync/chunks/`, pruned oldest-first above 512 MiB.

### Jobs

`nerve sync job` routes to the least-busy device that advertises every
`--needs` capability, or to `--on <device>`.

- **Detected capabilities:** OS, architecture, tmux, docker, git, node,
  cargo, plus `xcode` and `ios-sim` when Xcode is installed.
- **Extra capabilities:** add them under `caps` in `fleet.toml`.
- **Running:** the device runs `claude -p` headless, with
  `--permission-prompts none` (nothing hangs waiting for a human), and
  streams progress.
- **Verifying:** it then runs `--verify`. Exit 0 marks the job done and
  verified.
- **Offline devices:** jobs sent to a device that is asleep wait on the
  relay for up to a day. They start when it reconnects.
- **Re-running:** the job's `session_id` is reported, so it can be
  resumed or handed off.

## Files

All under `<nerve config>/sync/`:

| file | what |
|---|---|
| `device.key` | This device's secret key (0600). It identifies the device: keep it private. |
| `fleet.toml` | Name, relays, peers, extra capabilities, path map, policies. |
| `claude-settings.json` | Hooks for Claude (`install-hooks`). |
| `mailbox/` | Prompts waiting for a remote answer (settings-hook path). |
| `agent.sock` | The mod's connection to the agent (0600). |
| `mod/` | The nerve mod, as installed into Claude. |
| `chunks/`, `known/` | Content-addressed hand-off chunks; which chunks each peer holds. |
| `invites/` | Open one-time invite secrets (0700). |
| `activity.log` | The agent's activity, which CLI commands follow for acks. |
| `spool/` | CLI requests for the running agent. |
| `seen`, `since` | Replay protection and catch-up position. |
| `away` | Present while away mode is on. |

`fleet.toml` options:

```toml
device = "mac"
relays = ["ws://100.73.26.46:7777", "wss://relay.primal.net"]
caps = ["ios-sim"]                 # extra, on top of detected ones
handoff_root = "/Users/me/nerve-handoff"
remote_questions = "away"          # away | always | never
remote_permissions = true
auto_trust_handoffs = true         # accept Claude's folder-trust prompt for handed-off sessions
hook_timeout_secs = 600

[peers]
pi = "npub1..."

[path_map]                         # source prefix = local prefix
"/Users/me/SS" = "/home/pi/SS"
```

## Security model

- **Peers.** Only keys listed in `[peers]` are obeyed. Anything else is
  dropped before decryption is acted on.
- **Encryption.** Every payload is NIP-44 v2 encrypted to one device.
- **Integrity.** Events are signature-checked.
- **Replay.** Envelopes are deduplicated by id across restarts, and
  instructions expire after 5 minutes.
- **Scope of trust.** A paired device can type into your sessions and
  approve tool calls. That is remote code execution by design, so pair
  only devices you control. Treat `device.key` like an SSH private key.
- **What relays see.** Public relays can see which public keys talk, and
  when, and the size of each message. They cannot see content. Use a
  private relay on the tailnet if that metadata matters.
- **Invites.** A code is a one-hour, single-use credential. A stranger's
  `PairRequest` is read only while an invite is open, and only that
  message type.
- **The mod** runs inside Claude and can approve tool calls; it talks
  only to the 0600 socket of the same user's agent.
- **Not implemented yet:**
  - NIP-59 gift wrap to hide the sender key (works, measured in E2, not wired in);
  - NIP-42 relay auth;
  - key rotation and revocation, beyond editing `[peers]`.
