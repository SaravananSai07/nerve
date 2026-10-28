# Cross-device sync (`--features sync`)

Watch and drive Claude Code sessions running on your other devices: a
laptop, a Raspberry Pi, a server. From any device you can:

- see every device's sessions and when each device last synced;
- type an instruction into a session running elsewhere;
- answer its permission prompts and `AskUserQuestion` questions;
- read its diff, transcript tail and subagent work;
- move a session to another device and carry on there;
- send a job to whichever device has what the job needs;
- send files and git refs into a named place on another device, or pull
  them from it;
- run an allowlisted command on another device and get its exit code.

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
nerve sync handoff cc6fb097 pi --to v2-harness --then "carry on with the next step"
nerve sync job --needs macos --verify 'xcodebuild test ...' "ios tests" -- "fix the failing UI test"
nerve sync put pi v2-harness ignored/v2-build --delete --wait
nerve sync get pi v2-harness ignored/v2-build/units/S5 --wait
nerve sync refs pi v2-harness v2-build:refs/remotes/mac/v2-build --wait
nerve sync refs pi v2-harness --pull main:refs/remotes/pi/main
nerve sync run pi --place v2-harness --timeout 1800 --wait -- node bin/v2.mjs verify S5
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
5. **Unpack.** The target restores the repo where it was told (see
   *Where a hand-off lands*), refusing to overwrite uncommitted work. It installs
   the transcript under the slug Claude derives from the *new* path, and runs
   `claude --resume <id> --fork-session` in a detached tmux session
   `nerve-<id>`, typing `--then <text>` if given.
6. **Remember.** Both sides record which chunks the other now holds, so the
   next hand-off sends only what changed. A 700 KiB session one turn and one
   commit later moved 16–19 KiB (E22).

Chunks live in `<config>/sync/chunks/`, pruned oldest-first above 512 MiB.

#### Where a hand-off lands

The target picks the first of these that applies:

1. the place named with `--to <place>`: its `[places]` entry on the target;
2. the source path mapped through the target's `[path_map]`;
3. `<handoff_root>/<repo name>`, but only when `handoff_root` is set.

With none of these, the target refuses and names the config to add. It
never picks a directory on its own.

An existing checkout keeps its branch. The hand-off is refused, unless
you pass `--force`, when:

- the checkout is on a different branch from the hand-off's, or
- its branch has commits the hand-off doesn't have.

Uncommitted changes always refuse it.

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
- **Where:** `--cwd` on the executing device. Without it, the job runs in
  `job-<id>` under `handoff_root` when that is set, else in
  `<config>/sync/jobs/`.

### Places

A place is a name for a directory, defined on each device in its own
`fleet.toml`:

```toml
[places]                        # on the Mac
v2-harness = "/Users/me/SS/theerv-app"
```

```toml
[places]                        # on the Pi
v2-harness = "/home/pi/SS/theerv-app"
```

Commands name the place. Each side resolves its own path, so a sender
never needs to know remote paths. A place missing on either side is
refused, with the line to add.

### Files: `put` and `get`

```bash
nerve sync put pi v2-harness ignored/v2-build bin/v2.mjs --wait
nerve sync get pi v2-harness ignored/v2-build/units --delete --wait
```

`put` sends files and directories, given relative to the place (`.` is
the whole place). `get` asks the peer to send them back the same way, so
either device can start a transfer.

- **Incremental.** Files go through the hand-off's content-defined chunks
  and per-peer ledger, so only chunks the peer isn't known to hold travel.
  A repeat put of a 61-file, 1 MiB folder with one changed file moved
  8.9 KiB (2 of 237 chunks).
- **Atomic.** Each file is written to a temp file next to it and renamed
  into place. Its permission bits are kept. A file whose content and mode
  already match is left alone.
- **Contained.** Every path is checked before anything is written. A path
  that leaves the place (`..` or absolute), or that goes through a
  symlink, refuses the whole transfer. The sender skips symlinks.
- **`--delete`** removes files under the given *directories* that the
  sender no longer has, then any directories that leaves empty. It never
  deletes outside them, and doesn't follow symlinked directories.
- **`--wait`** blocks until the receiver acknowledges, then prints what
  moved. Without it, a running agent takes the transfer over and the
  command returns at once. With no agent running, the CLI stays until the
  transfer ends anyway. A failure exits 1.

```
beta ✓ b15eac3f…: beta place harness: 1 file(s) written, 60 unchanged, 0 deleted;
  received 8.9 KiB in 2 of 237 chunks (235 reused) in 0.5s
```

#### Big files: rsync

The relay holds every chunk in memory until it is deleted or expires,
so files at or over `[xfer] bulk_over` (default 64 MiB) skip it: the
relay carries the offer, the other side's path and the outcome, and
the files go over rsync on ssh.

```toml
[ssh]
pi = "theerv-pi"        # this device reaches the peer "pi" at this ssh host

[xfer]
bulk_over = 67108864    # bytes
```

- **One route is enough.** rsync runs on whichever side has one. A Pi
  the Mac can reach but not the other way round still sends: the Mac
  pulls from the path the Pi gives. With no route on either side the
  transfer is refused, naming the setting to add.
- **Same rules.** Roots stay inside the place, symlinks are skipped
  (`-rtp`, no `-l`), nothing is written through one on the receiver,
  and `--delete` reaches only into the named directories. Each root is
  its own rsync; `--partial` lets each of up to three attempts resume.
- **Fallback.** A relay transfer the relay fails to carry (down,
  rate-limit patience spent) goes over rsync when there is a route. The
  path then comes from the far side's own nerve over ssh
  (`nerve sync place <name> --src|--dst <path>...`), never a guess. A
  `--wait` that hears nothing about its transfer for five minutes, or
  runs with no agent, does the same from the CLI.
- **Outcome.** The same `✓ <id>:` line and exit code as the relay path,
  ending `via rsync, N file(s), X MiB in Ys`. A running rsync logs a
  line a minute.

#### Relay memory

A sender deletes (NIP-09) the events of a transfer once the receiver
confirms it, and `serve-relay` sweeps expired events every ten minutes
and keeps at most 4000 (oldest dropped first), so the in-memory store
stays well under the service's memory cap.

### Git refs: `refs`

```bash
nerve sync refs pi v2-harness v2-build:refs/remotes/mac/v2-build --wait
nerve sync refs pi v2-harness --pull main:refs/remotes/pi/main --wait
```

Each pair is `<src-ref>:<dst-ref>`. The source is a branch, tag or `HEAD`
in the sender's place repo. The destination is a full ref in the
receiver's. `--pull` reverses the direction: the source is on the peer
and the destination is here.

- The receiver reports its branch and remote-tracking tips first, as for
  a hand-off. The git bundle carries only the commits beyond them, and
  none at all when it already has them.
- The receiver adds the bundle's objects (`git bundle unbundle`), then
  points **exactly the named destination refs** at their commits with
  `git update-ref`.
- Nothing is checked out. HEAD, the index, the working tree and every
  other ref stay as they were.
- These are refused:
  - a destination that isn't a full `refs/…` name;
  - the destination the receiver has checked out;
  - a place that holds a different repository (a different root commit).

### Runs

```bash
nerve sync run pi --place v2-harness --timeout 1800 --wait -- node bin/v2.mjs verify S5
```

`run` starts one command on a peer. The command runs only if the peer's
`fleet.toml` allows it:

```toml
[run]
allow = [
  { place = "v2-harness", argv = ["node", "bin/v2.mjs", "*"] },
  { cwd = "/home/pi/SS/theerv-server", argv = ["make", "test"] },
]
```

- **Matching.** `argv` matches literally. A final `"*"` matches any
  further arguments, including none; a `*` anywhere else makes the rule
  match nothing. A rule with `place` or `cwd` applies only there; a rule
  with neither applies anywhere.
- **Refusals** name the rule to add and the file to add it to.
- **No shell.** The argv is passed as given. `./bin/x` resolves inside the
  run's directory; a bare name is looked up on PATH, then in
  `~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin` and
  `/usr/local/bin`. A rule with `shell = true` instead joins the argv with
  spaces and runs `sh -c`. A trailing `*` in such a rule admits arbitrary
  shell, so keep shell rules literal.
- **Directory.** `--place P` runs in the place's directory on the peer,
  `--cwd DIR` in an absolute directory there. With neither, it runs in the
  home directory.
- **Timing.** The 5-minute expiry applies to *starting*: a stored request
  older than that is refused. A started command runs as long as it needs,
  or until `--timeout` kills its process group (exit 124).
- **Replay.** Like every envelope, a run request is deduplicated by id, so
  it starts at most once.
- **Output.** Stdout and stderr are merged into a 20-line tail, sent at
  most once a second. `nerve sync status` and the Devices view list runs
  with jobs, showing the exit code once the run ends.
- **`--wait`** prints the output as it arrives, on stdout, and exits with
  the remote exit code. When the command never ran (refused, unknown
  device, no answer), it exits 125. Without `--wait`, it returns once the
  peer accepts or refuses.

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
| `chunks/`, `known/` | Content-addressed chunks for hand-offs and `put`/`get`/`refs`; which chunks each peer holds. |
| `jobs/` | Working directories of jobs given no `--cwd` (when `handoff_root` is unset). |
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
handoff_root = "/Users/me/nerve-handoff"   # optional; unset, unplaced hand-offs are refused
remote_questions = "away"          # away | always | never
remote_permissions = true
auto_trust_handoffs = true         # accept Claude's folder-trust prompt for handed-off sessions
hook_timeout_secs = 600

[peers]
pi = "npub1..."

[path_map]                         # source prefix = local prefix
"/Users/me/SS" = "/home/pi/SS"

[places]                           # name = absolute path on this device
v2-harness = "/Users/me/SS/theerv-app"

[run]                              # commands peers may run here
allow = [{ place = "v2-harness", argv = ["node", "bin/v2.mjs", "*"] }]
```

### The agent's PATH

launchd and systemd user units start with a bare PATH. Claude Code
installs into `~/.local/bin`, which is not on it. `nerve setup` writes
your PATH into the unit, adding `~/.local/bin`, `~/.cargo/bin`,
`/opt/homebrew/bin` and `/usr/local/bin` where they exist. Hand-offs,
jobs and runs also search those directories when PATH misses. If `claude`
still isn't found, the error lists every directory searched. Re-run
`nerve setup` to refresh a unit written by an older version.

## Security model

- **Peers.** Only keys listed in `[peers]` are obeyed. Anything else is
  dropped before decryption is acted on.
- **Encryption.** Every payload is NIP-44 v2 encrypted to one device.
- **Integrity.** Events are signature-checked.
- **Replay.** Envelopes are deduplicated by id across restarts, and
  instructions expire after 5 minutes.
- **Commands.** `nerve sync run` executes only what a `[run] allow` rule
  in the *receiving* device's fleet.toml names. A sender cannot widen the
  rule, and an empty allowlist (the default) runs nothing.
- **Places.** `put`, `get` and `refs` write only inside a place the
  receiver defined. Writes never follow a symlink out of it. `refs` sets
  only the destination refs named, never one that is checked out.
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
