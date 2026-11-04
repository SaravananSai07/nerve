# Cross-device sync

Watch and drive Claude Code sessions running on your other devices: a
laptop, a desktop, a small always-on server. From any device you can:

- see every device's sessions and when each device last synced;
- type an instruction into a session running elsewhere;
- answer its permission prompts and `AskUserQuestion` questions;
- read its diff, transcript tail and subagent work;
- move a session to another device and carry on there;
- start a session on another device, in a folder there, with a prompt;
- send files and git refs into a named place on another device, or pull
  them from it;
- run an allowlisted command on another device and get its exit code;
- keep folders the same on the devices you choose, and undo a sync.

Transport is [Nostr](https://github.com/nostr-protocol/nips): every
device dials *out* to one or more relays over WebSocket, so it works
behind NAT with **or without** Tailscale. Everything is NIP-44
encrypted to the receiving device; relays only see ciphertext, kinds,
timestamps and public keys.

The research behind this design is in `research/sync/`:
- `REPORT.md`: findings, measurements and the comparison of 48 alternatives;
- `DECISIONS.md`: why each choice was made;
- `experiments/`: raw results.

## Connect devices

Every nerve build includes sync; nothing talks to a relay until a device
is connected. In the TUI, open Settings and press `c`; from a shell:

```bash
nerve connect                 # show a code (the first time, set this device up too)
nerve connect nerve1…         # on the new device: join with it
```

The first run on a device creates its key, installs the nerve mod into
Claude Code (2.1.287 or later) and installs the background agent
(launchd on macOS, a systemd user unit on Linux, a detached process plus
a Termux:Boot script on Termux). `nerve setup` is the same command.

1. **The code.** The device showing it makes a one-time secret, valid for
   10 minutes, and puts its key, relays, the secret and the names of the
   devices on offer into the code (and a terminal QR). Closing the TUI
   dialog withdraws it.
2. **The join.** The new device trusts that key, adopts its relays and
   sends the secret with the devices it wants (`--with`; all offered by
   default). The secret is used up on arrival.
3. **The yes.** The join waits for a person, as an approval (see
   *Approvals*). Both screens show three words made from both keys and
   the secret; allow it only if they match. Any of your devices can
   answer: `y` in the TUI, or `nerve allow <id>`.
4. **The group.** On a yes the inviter trusts the new device and
   introduces it to each device it joins; they trust it on the inviter's
   word. The new device learns their keys from the inviter.

Who a new device joins is the inviter's choice: `--with all` (every
device, and devices you add later: the default), `--with now` (the
devices there now; devices added later don't meet it unless you link
them), or `--with studio,pi`. Later:

```bash
nerve devices                 # who is connected, online, and on which nerve
nerve link pi all             # pi meets every device, now and later
nerve link pi studio          # or some
nerve forget pi               # every device stops trusting pi
```

Flags: `--name N`, `--relay URL` (repeatable), `--with …`,
`--no-service`, `--no-claude`, `--no-wait` (show the code and return;
allow the join with `nerve allow`). Check a device with
`nerve sync doctor`. The pieces underneath are still there:
`nerve sync init`, `pair`, `relay add|rm` and `install-hooks`.

### Upgrades

Install a new nerve the usual way (`install.sh`, `nerve update` or
`cargo install`). Each replaces the binary with a rename, never by
writing into it: macOS kills a binary rewritten in place when it next
starts. The background agent checks its binary every 10 seconds. Once
it changes, and no transfer is mid-way, the agent checks that the new
binary runs (`nerve --version`) and restarts as it. At start it also
rewrites the nerve mod's files if they differ from its own copy, and
sessions load the new mod when they next start.

### The nerve mod

`nerve connect` registers `<config>/sync/mod` as a local plugin
marketplace and installs `nerve@nerve-local`. New Claude sessions load
it; restart older ones to pick it up. It connects to the agent's Unix
socket (`<config>/sync/agent.sock`, mode 0600) and:

- submits prompts sent from other devices with `$.prompt.submit`
  (queued while a turn runs; works for `claude --bg` sessions);
- offers permission prompts and `AskUserQuestion` questions to other
  devices while Claude's own dialog stays on screen. The first answer
  wins, and an answer at the keyboard withdraws the request;
- reports each turn's duration and token use;
- shows a line under the prompt only when something needs attention:
  `⇄ nerve · 1 waiting on another device`, or `⇄ nerve · quiet: server 5m`
  when a peer hasn't reported for 2 minutes. Otherwise the line is hidden.

Sessions without the mod (older Claude, `--bare`, started before
setup) fall back to tmux/Ghostty paste and the settings hook, as before.
A session with neither can be watched but not typed into; its panel
says so.

### Relays

| setup | how | measured RTT | notes |
|---|---|---|---|
| Public relays (default) | what `nerve connect` configures | **0.5–0.9 s** | No VPN at all. Relays see metadata. |
| + private relay | `nerve sync serve-relay` on an always-on device, then `nerve sync relay add ws://<relay-host>:7777` on each device | **~16 ms** | Bulk data (hand-offs, diffs) goes only here when connected. In memory. |

Public relays that worked for fresh keys in testing:
- relay.primal.net
- relay.snort.social
- relay.nostr.net (lossy)

nos.lol and nostr.mom acknowledged events but never delivered them.

## Run

`nerve connect` installs the agent as a service, so there is nothing to
start by hand. The top line names this device and how many others are
online.

One agent works for a device: whichever holds `agent.lock` (an `flock`,
so it frees itself when its holder exits or crashes). A TUI that finds
the lock held attaches to that agent as a viewer; one that takes it runs
the agent itself. `nerve sync agent` (the service) waits for the lock,
so a TUI's agent keeps the device and the service takes over once that
TUI quits. CLI commands work either way: they hand their requests to
whichever agent runs.

An agent that cannot reach its relays retries, waiting 2 s, then twice
as long each time, up to 2 minutes, and re-reads `fleet.toml` before
each try. Meanwhile a TUI shows this device's own notifications itself.

Don't copy one device key to two devices: two agents holding the same
key both act on every command.

## Use it

### What leaves this device

Every session is visible on every connected device unless you keep it
private:

- `p` on a session asks whether to keep that session or its whole
  folder on this device. From the CLI, `nerve sync private [<dir>]` /
  `nerve sync public [<dir>]` (default: the current folder); a bare
  `nerve sync private` lists what is kept here.
- The lists live in fleet.toml as `private = ["/abs/dir", "~/code/x"]`
  (subfolders are included) and `private_sessions = ["<session id>"]`.
- A private session is not published, so peers don't see its name,
  folder, transcript tail or cost. Peers can't type into it, stop it or
  fetch from it, and its permission prompts and questions are answered
  only at its own keyboard. On its own device it carries ⊡.
- Settings lists everything kept private; `u` there makes a folder or
  session visible again.
- Files, refs, runs, started sessions and hand-offs move only when you start them.
  Privacy doesn't change that. Synced folders move on their own once
  agreed (see *Approvals*); keeping a synced folder private stops
  syncing it.

### In the TUI

Sessions from every connected device share one list, with the device
named on each row. The same keys work on all of them: `1`-`9` answers a
question or permission prompt, `i` sends a prompt, `x` stops the
session, `Enter` opens its transcript and `n` picks who hears
when its turn finishes. `H` hands a session on this device to another
one, and `s` starts a new session on any device: the device, a folder
there and the prompt. Approvals wait in Needs you too: `y` allows, `n` refuses, `Enter`
reviews.

The **Sync** page (`Tab`) lists the folders kept the same across
devices: who shares each one, when it syncs, the rule for folders asked
for next, and how it stands (in sync, newer files on another device,
waiting for an offline one or for a yes, or a progress bar while files
move). Below is **What went where**: the last files that moved, from
which device to which. `a` syncs a folder that a session here works in
(the folder, the devices, when, and the rule), `s` syncs the chosen
folder now, `o` changes its rule, `v` lists the earlier versions a sync
replaced and puts one back, and `x` stops syncing it on this device.
Sessions in a synced folder carry ⇄.

A device that hasn't reported for 3 minutes shows its sessions as
offline (◌). Settings lists each device, its OS and when it was last
seen, and marks one on an older nerve that syncs new folders without
asking. There `c` connects a device, `l` connects the one under the
cursor to more devices and `f` forgets it. Relay and hand-off status
are under **Connection**.

### Notifications

The agent holding `agent.lock` (the service, or a TUI with no service
beside it) sends this device's notifications for sessions on every device:
questions, permission prompts and failures by default, and finished
turns on the devices ticked under Finished turns, unless `n` chose
otherwise for that session. The agent skips a finished turn for the
session a focused TUI on its device has open (`Request::Viewing`; a
viewing TUI passes it through the spool). Which devices hear about what
is set in Settings and travels with each device's state, so every
device applies the same choices; the newest change wins. Clicking a
notification on macOS runs `nerve --land <device>/<session>`, which
opens that session in the running TUI.

The same features are available from the CLI. The everyday ones are
top-level (`nerve help` lists them: `connect`, `devices`, `link`,
`forget`, `requests`, `allow`, `refuse`, `sync <dir>`, `unsync`,
`folders`, `versions`, `restore`); the rest are under `nerve sync`:

```bash
nerve sync doctor                               # is this device set up and talking?
nerve sync private ~/code/secret               # keep sessions there on this device
nerve sync status                               # last synced, sessions, pending prompts
nerve sync send server 4c14e2fb "run the tests again"
nerve sync answer server 15a12dee3382bc05 allow     # or deny, or an option label
nerve sync fetch server 4c14e2fb diff
nerve sync handoff cc6fb097 server --to proj --then "carry on with the next step"
nerve sync start server '~/code/proj' "fix the failing test" --wait
nerve sync put server proj build --delete --wait
nerve sync get server proj build/reports --wait
nerve sync refs server proj feature:refs/remotes/laptop/feature --wait
nerve sync refs server proj --pull main:refs/remotes/server/main
nerve sync run server --place proj --timeout 1800 --wait -- make test
nerve sync away on
```

## How each feature works

### Progress and "last synced"

- Each agent scans local sessions (the same discovery the TUI uses) every
  3 s. It republishes its **device state** when anything changes, and at
  least once a minute.
- The state includes:
  - every session that isn't private: state, waiting reason, cwd, branch, cost, subagent count and an 8-line transcript tail;
  - pending prompts;
  - runs;
  - capabilities;
  - away flag.
- It is an *addressable* Nostr event (kind 30078, one per recipient). The
  relay keeps only the newest copy, so a laptop that has been shut keeps
  showing what it was doing when it went away, with the time.
- "Last synced" is the time the peer generated that state.
- `nerve sync sync` republishes immediately and asks peers to do the same.

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
Questions therefore go remote only in **away mode** (`nerve sync away on`), or with `remote_questions = "always"`. Otherwise the person at
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

### Starting a session

`s` on the Sessions page, or `nerve sync start`, starts a new session
on any connected device:

```bash
nerve sync start server '~/code/proj' "fix the failing test"   # session id, then tmux session
nerve sync start server proj "run the tests, sum up" --wait     # then its reply, once the turn ends
```

- **What runs.** The device runs `claude --session-id <id> -- <prompt>`
  in a detached tmux session `nerve-<id>` in the folder, the way a
  hand-off resumes one, with the same environment (`CLAUDE_CONFIG_DIR`,
  `PATH`, the sync settings). It is an ordinary interactive session: it
  shows on every device, takes prompts and approvals, and
  `tmux attach -t nerve-<id>` on that device opens it.
- **No shell.** tmux runs claude directly, the prompt one argument after
  `--`, so quotes, `;`, `$(…)` and newlines reach Claude as typed. A
  prompt is at most 8 KiB.
- **The id.** The sender picks the session id (a UUID), so the first
  line `start` prints is the session id and the second the tmux
  session, as soon as the device says it started.
- **Where.** `<dir>` on that device: an absolute path, `~` or `~/…` (its
  home; a path under your own home is sent as `~/…`), or a place name. It
  must be an existing folder inside its home, a place or
  `handoff_root`, and neither private nor one nerve never syncs (`~/.ssh`
  and the rest, Claude's and nerve's config). Anything else is refused,
  naming what to add to `[places]`.
- **Who may.** Any connected device, as for a hand-off. Like an
  instruction, a request older than 5 minutes is refused, not started.
  With `auto_trust_handoffs` (the default) Claude's folder-trust prompt
  is accepted, as for a hand-off. Claude asks for permissions as it does
  at the keyboard.
- **`--wait`** follows the session in the state its device publishes, as
  the TUI does, until its first turn ends (idle after working, or idle
  with a reply after the prompt). Then it prints Claude's last message,
  fetched in full. A permission prompt or question is noted on stderr
  once; answer it from any device and the wait goes on. `--timeout S`
  gives up after S seconds (default 3600). A failure exits 1 with the
  reason on stderr.
- **Older devices.** A device on an older nerve drops the request, so
  `start` reports no answer. A device on this one refuses a request it
  doesn't know, such as a headless job from before `start` replaced
  them, with an ack naming its nerve version.

### Places

A place is a name for a directory, defined on each device in its own
`fleet.toml`:

```toml
[places]                        # on the laptop
proj = "/Users/me/code/proj"
```

```toml
[places]                        # on the server
proj = "/srv/proj"
```

Commands name the place. Each side resolves its own path, so a sender
never needs to know remote paths. A place missing on either side is
refused, with the line to add.

### Files: `put` and `get`

```bash
nerve sync put server proj build scripts/check.sh --wait
nerve sync get server proj build/reports --delete --wait
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
server = "user@server.example"   # this device reaches the peer "server" at this ssh host

[xfer]
bulk_over = 67108864    # bytes
```

- **One route is enough.** rsync runs on whichever side has one. A
  device that can be reached but cannot reach back still sends: the
  peer with the route pulls from the path the sender gives. With no route on either side the
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
nerve sync refs server proj feature:refs/remotes/laptop/feature --wait
nerve sync refs server proj --pull main:refs/remotes/server/main --wait
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
nerve sync run server --place proj --timeout 1800 --wait -- make test
```

`run` starts one command on a peer. The command runs only if the peer's
`fleet.toml` allows it:

```toml
[run]
allow = [
  { place = "proj", argv = ["make", "*"] },
  { cwd = "/srv/api", argv = ["make", "test"] },
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
  most once a second. `nerve sync status` lists runs, with the exit code once a run ends.
- **`--wait`** prints the output as it arrives, on stdout, and exits with
  the remote exit code. When the command never ran (refused, unknown
  device, no answer), it exits 125. Without `--wait`, it returns once the
  peer accepts or refuses.

### Synced folders

```bash
nerve sync ~/code/site --with studio,server [--when turn|watch|manual] [--others ask|inside|any]
nerve folders                       # each folder and how it stands
nerve sync site                     # send this device's changes, ask for theirs
nerve unsync site                   # stop syncing it here; the others carry on
```

A synced folder stays the same on every device that shares it. The
running agent does the work, so these commands need the service or an
open TUI.

- **Joining.** Starting a folder names it after its directory, saves it
  as a place, scans it, and tells each chosen device how many files it
  holds. A device lands it at its own place of that name if it has one,
  else where `path_map` maps the sender's path, else at the same path
  under its home directory, else under `handoff_root`. It says what to
  add to `[places]` when it has nowhere to put it. Files already there
  stay: the first sync sends only what differs. Nothing is sent to a
  device before it has said yes.
- **Who says yes.** See *Approvals*: a folder started in the TUI is a
  person's yes; one started from a shell waits on each device that no
  folder rule covers.
- **Rules.** Each folder carries what its devices may sync next without
  asking: `ask` (the default: nothing), `inside` (folders inside its
  parent folder) or `any` (any folder that isn't private). A new folder
  from a device is covered when a folder this device already shares with
  it has a rule that covers where the new one lands. The rule chosen
  when a folder is started or allowed reaches every device sharing it;
  `o` on the Sync page changes it everywhere.
- **Never.** Whatever the rule, a device refuses its private folders,
  `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.docker`,
  `~/.config/gcloud`, `~/.netrc`, Claude's config roots and nerve's own
  config, and any folder that contains one of them (such as home).
- **When.** `turn` (the default) syncs when a session working in the
  folder finishes a turn. `watch` syncs 2 seconds after files stop
  changing. `manual` syncs only on `s` or `nerve sync <folder>`. Every
  device sends its changes on its own trigger. A device that comes back
  online catches up on what it missed.
- **What is in it.** In a git repository: tracked files and untracked
  files that aren't ignored, so `node_modules/` and build output stay
  behind. Elsewhere: every file except `.git/`. Symlinks are skipped.
- **How it moves.** Each device keeps an index of its copy (size, mtime
  and SHA-256 per file) and, per device it shares with, which version of
  each file that device holds. What a device lacks travels as a `put`
  into the folder's place: files under `[xfer] bulk_over` in batches over
  the relay, each bigger file alone over rsync (which needs an `[ssh]`
  route on one side, as for `put`).
- **Conflicts.** When both devices changed a file since they last agreed
  on it, the version from the device whose key sorts first keeps the
  name, and the other is saved beside it as
  `<file>.conflict-<device>`. Both devices make the same choice, so both
  end up with both versions, and nothing is lost.
- **Deletions.** A file deleted on one device is deleted on the others
  only if they still have the version that device knew. A file edited
  meanwhile stays.
- **Earlier versions.** Before a sync replaces or deletes a file, the
  receiving device keeps it in `<sync>/versions/`: its bytes cut into
  content-defined chunks named by SHA-256 (versions of a file share most
  chunks), and a plain JSON line with the folder, path, size, time and
  why. Files at or over `bulk_over` aren't kept. After
  `keep_versions_hours` (48) they are pruned with the chunks only they
  used. `nerve versions [<folder>]` lists them, `nerve restore <id>` (or
  `v` on the Sync page) puts one back, keeping what it replaces, and the
  restored file syncs to the others like any change.
- **Failures.** A transfer that fails is retried after a minute, then
  after twice as long each time, up to 15 minutes. The Sync page and
  `nerve folders` show the reason.

### Approvals

Two things wait for a person's yes on the device they would change: a
device joining with a code this device showed, and a folder another
device shares that no rule here covers. Each is kept in
`<sync>/approvals.json` and published in this device's state, so it
shows under Needs you on every device, notifies like a permission
prompt, and any connected device may answer it (`Body::Decide` to the
device holding it).

```bash
nerve requests                      # every approval, on any device
nerve allow <id> [--at DIR] [--others ask|inside|any]
nerve refuse <id>
```

An id may be shortened to any unique prefix. `--at` lands a folder
somewhere else (`~` is that device's home). A join that isn't answered
before its code would have expired is dropped. A device on an older
nerve doesn't hold anything: it joins every folder shared with it, so
Settings and `nerve devices` mark it until it is updated.

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
| `invites/` | Open one-time codes, with who they let a device join (0700). |
| `activity.log` | The agent's activity, which CLI commands follow for acks. |
| `spool/` | CLI requests for the running agent. |
| `agent.lock` | Held (`flock`) by the one agent working for this device; names its pid. |
| `agent.alive` | The running agent's heartbeat: its pid, touched every few seconds while it is on the relays. |
| `agent.log` | The background agent's output, when launchd or a detached start runs it (systemd logs to the journal). |
| `seen`, `since` | Replay protection and catch-up position. |
| `away` | Present while away mode is on. |
| `local.json` | This device's own state, as the agent last built it; a TUI beside the service reads it. |
| `folders/` | Per synced folder: its index, what each device holds, and the recent transfers. |
| `approvals.json` | Joins and folders waiting for a person's yes. |
| `versions/` | Files syncs replaced or deleted (0700): `chunks/` by SHA-256 and `log.jsonl`. |

Notification choices live one level up, in `<nerve config>/notify.toml`.

`fleet.toml` options:

```toml
device = "laptop"
relays = ["ws://192.0.2.10:7777", "wss://relay.primal.net"]
caps = ["ios-sim"]                 # extra, on top of detected ones (`nerve sync status` shows them)
handoff_root = "/Users/me/nerve-handoff"   # optional; unset, unplaced hand-offs are refused
remote_questions = "away"          # away | always | never
remote_permissions = true
auto_trust_handoffs = true         # accept Claude's folder-trust prompt for handed-off and started sessions
hook_timeout_secs = 600
private = ["~/code/secret"]        # folders whose sessions stay on this device
private_sessions = []              # single sessions kept here (`p` in the TUI)

keep_versions_hours = 48           # files syncs replaced stay this long
closed = ["pi"]                    # peers devices added later don't meet

[peers]
server = "npub1..."

[path_map]                         # source prefix = local prefix
"/Users/me/code" = "/home/user/code"

[places]                           # name = absolute path on this device
proj = "/Users/me/code/proj"

[run]                              # commands peers may run here
allow = [{ place = "proj", argv = ["make", "*"] }]

[ssh]                              # peer = ssh host; big files go over rsync on it
server = "user@server.example"

[xfer]
bulk_over = 67108864               # bytes from which files skip the relay

[[folders]]                        # kept the same on several devices
name = "site"                      # its path is the place of the same name
with = ["studio"]                  # the other devices that keep a copy
when = "turn"                      # turn | watch | manual
others = "ask"                     # ask | inside | any: folders asked for next
```

### The agent's PATH

launchd and systemd user units start with a bare PATH. Claude Code
installs into `~/.local/bin`, which is not on it. `nerve connect` writes
your PATH into the unit, adding `~/.local/bin`, `~/.cargo/bin`,
`/opt/homebrew/bin` and `/usr/local/bin` where they exist. Hand-offs,
started sessions and runs also search those directories when PATH
misses. If `claude`
still isn't found, the error lists every directory searched. Re-run
`nerve connect` to refresh a unit written by an older version.

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
- **Starting sessions.** A connected device can start Claude in a
  folder here: the same trust as a hand-off, or as typing into a
  session. Only inside the home folder, a place or `handoff_root`, never
  in a private folder or one nerve never syncs, and Claude asks for
  permissions as it would at the keyboard.
- **Places.** `put`, `get` and `refs` write only inside a place the
  receiver defined. Writes never follow a symlink out of it. `refs` sets
  only the destination refs named, never one that is checked out.
- **Privacy.** A private session is never published or reachable from a
  peer. Requests for one are refused by the receiving agent and the hook.
- **Synced folders.** A connected device can ask to share a folder with
  this one, but the folder waits for a person's yes unless a folder this
  device already shares with it has a rule that covers it. It lands
  under this device's home directory (or a place, `path_map` or
  `handoff_root`), never in a private folder or one nerve never syncs,
  and takes files only from the devices sharing it. Rules contain
  scripts, agents and mistakes; they are no wall against a device that
  is already compromised, which can drive your sessions anyway: forget
  it from any other device.
- **Files that run.** A synced folder can carry what executes when you
  work there: `.claude/settings.json` hooks, scripts, `.envrc`. Syncing
  a folder trusts every device sharing it with what runs in it.
- **Scope of trust.** A connected device can type into, stop and approve
  tool calls for every session that isn't private. That is remote code
  execution by design, so connect only devices you control. Treat
  `device.key` like an SSH private key.
- **What relays see.** Public relays can see which public keys talk, and
  when, and the size of each message. They cannot see content. Use a
  private relay on a private network if that metadata matters.
- **Codes.** A code is single-use and lasts 10 minutes, and on its own
  connects nothing: the join waits for a person's yes, by name, with
  three words both screens show. A stranger's `PairRequest` is read only
  while a code is open, and only that message type. Someone who gets you
  to enter *their* code joins you to *their* devices; the words can't
  catch that, since they control both screens. Enter only codes from
  your own screens.
- **Introductions.** Each device trusts a new one on the word of a
  device it already trusts (the inviter, or whoever runs `nerve link`),
  and drops one that any of them forgot. A compromised device could do
  both, which is no more than the session control it already has.
- **The mod** runs inside Claude and can approve tool calls; it talks
  only to the 0600 socket of the same user's agent.
- **Not implemented yet:**
  - NIP-59 gift wrap to hide the sender key (works, measured in E2, not wired in);
  - NIP-42 relay auth;
  - key rotation and revocation, beyond editing `[peers]`.
