# Architecture

## Module map

```
src/
  main.rs             Entry point; argv[1] picks the command (--dump, --list,
                      --land/--focus, update, statusline, sync verbs)
  app.rs              App state (+ Ui), main loop, key routing, draw and footer,
                      tick/drain/rebuild, row order, landing on a session,
                      local notifications
  app/
    sessions.rs       Sessions page: keys, find, prompt box, actions, footer hints
    sync.rs           Sync page: keys, view, hints, folder actions
    settings.rs       Settings page: keys, view, hints, theme, device actions
    dialog.rs         Dialog, confirmation and paste input
    fleet.rs          Everything through the sync agent: rows and actions for
                      other devices, connecting, devices, privacy
  config.rs           config.toml (people edit it) and prefs.toml (the TUI keeps it)
  paths.rs            Config-dir files; `~` contraction and expansion
  lockfile.rs         Single-instance flock (nerve.lock; the sync agent's agent.lock)
  log.rs              File-backed debug log
  notify.rs           Desktop notifications (on macOS a click runs `nerve --land`);
                      NotifyBook, who gets which notifications (notify.toml)
  terminal.rs         TerminalGuard (restore on exit/panic), tty-loss watchdog,
                      shutdown signals
  updater.rs          Background latest-release check
  detect/
    mod.rs            claude_roots(): $CLAUDE_CONFIG_DIR + ~/.claude
    worker.rs         Background thread: FS-watch + 1 Hz heartbeat → sessions,
                      the open session's transcript, notification clicks
    claude.rs         Session + background-job discovery, PID/TTY dedup, stop/kill
    statusline.rs     `nerve statusline` hook + reader for Claude's own cost/context
    jsonl.rs          Transcript tail → SessionState; token usage; LogEntry
    jsonl_cache.rs    Incremental parse cache keyed by (mtime, len, inode)
    process.rs        `ps -eo` process table, cached
    git.rs            Branch lookup, mtime-cached
  state/
    session.rs        Session, SessionId, SessionState, DiscoverySnapshot,
                      TokenUsage, the N-tick state debounce
    registry.rs       Sessions by id; apply() folds a scan in and returns
                      the notifications it calls for
  platform/
    mod.rs            Bridge (+ own tab), BridgeId for --focus, desktop app,
                      clipboard, osascript runner (macOS)
    ghostty.rs        Tab focus, own-tab probe, screen capture via AppleScript (macOS)
    tmux.rs           Pane focus + capture
  tui/
    mod.rs            Page, footer and layout; draw()
    model.rs          Row: one session, local or remote, as the UI shows it; bands, find
    paint.rs          Line builder and the scroll window that keeps the cursor in view
    sessions.rs       The session list (full and compact)
    detail.rs         The panel: question, transcript or captured screen, composer
    editor.rs         The wrapping, multi-line prompt box (composer, starting a session)
    settings.rs       Settings: devices, private folders and sessions, notify, theme
    sync.rs           Sync: synced folders, their rules, what waits for a yes, what went where
    dialog.rs         Notify, keep private, rename, hand-off, start a session, sync a
                      folder, folder rules, earlier versions, answering a request,
                      connecting a device, keys
    markdown.rs       Claude's markdown as a terminal shows it (headings, lists, code, tables)
    theme.rs          Colour roles and built-in themes
  sync/               Cross-device sync over Nostr, see the Sync section below
    protocol.rs       Wire format (pure): envelopes, state, requests, offers
    fleet.rs          Device identity, peers, places, capabilities (fleet.toml)
    transport.rs      Nostr + NIP-44, the seen-set, the fleet's own relay
    agent.rs          The per-device worker: peers, requests, runs, starts, hand-offs
    agent/approvals.rs  Joins and folders waiting for a person's yes; the device group
    agent/folder_sync.rs  The agent's side of synced folders: indexes, pushes, landing changes
    agent/publish.rs  This device's state, local.json, notifications a new state calls for
    agent/transfer.rs Hand-offs and transfers into places, over the relay or rsync
    ipc.rs            CLI to agent: the spool, outcome lines, following a request
    cli.rs            `nerve sync ...`, and the everyday commands: devices, link,
                      forget, requests, allow, refuse, sync <dir>, unsync, folders,
                      versions, restore
    connect.rs        `nerve connect`, the join handshake, invite codes
    install.rs        The Claude mod or hooks, and the background service
    mailbox.rs        Claude hook and agent rendezvous for remote answers
    modhub.rs         The nerve mod's Unix-socket bridge to the agent
    inject.rs         Typing into a session's tmux pane or Ghostty tab
    chunks.rs         Content-defined chunking and the chunk store
    handoff.rs        Pack, unpack and resume a session on another device, or start a
                      new one there (folder checks, `claude` argv); both run in tmux
    runs.rs           Allowlisted plain-command runs
    places.rs         Files and git refs into named places; big files over rsync
    folders.rs        Folders kept the same on several devices
    versions.rs       What syncs replaced, by content hash, kept to undo
    bins.rs           Executable lookup that survives a service's bare PATH
  util/               hostname, safe-id check, fs helpers (capped reads, TOML,
                      FileKey, walks, scratch dirs), command runner (run, text), sanitising (Sanitised, clean), text truncation
  bin/nerve_tui_alias.rs  `nerve-tui`, the pre-0.5.1 name; execs `nerve`
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
5. `SessionRegistry::apply` folds them in. Authoritative states commit
   immediately; heuristic ones only stick after 3 consecutive matching
   ticks (`observe_state`).
6. A session that is missing from discovery goes to `Vanished` and is evicted
   after a grace period. A session waiting for 48 h or more goes to `Dormant`
   and is kept.

## State model

`Processing` → `ToolRunning(tool)` → `WaitingForInput` → `Idle` → `Error` →
`Dormant` → `Vanished`. The list groups sessions into three bands: Needs
you (waiting or failed), Working and Idle. Notifications fire on
committed transitions into `WaitingForInput` and `Error`, and into `Idle`
for sessions marked with `n`.

## CPU budget

With six sessions a healthy instance uses about 0.15 s of CPU per minute and
about 12 MB of RSS. The main loop is floored at 50 ms per iteration, the
process table is cached, and transcripts are re-parsed only when
`(mtime, len, inode)` changes. Only while a spinner shows (a session
working, or the panel waiting for its transcript) does the screen
redraw ten times a second; ratatui writes only the cells that changed.
Discovery's channel is drained every pass; the rest of the tick runs
at `refresh_interval_ms`.

Watch for crossterm's `try_read`: it spins on `read() == 0` once the tty
hangs up, which is why `spawn_tty_watchdog` exists. Don't remove it.

Reap every child where it is spawned (`.status()`, `.output()`, `wait`).
Don't add a SIGCHLD reaper: its `waitpid(-1)` steals children from
`std::process`, whose calls then fail with ECHILD.

Don't poll `claude agents --json` for background jobs: each call costs
about 90 ms of CPU. The job state files carry the same data.

## Sync

Operating it is in [DEPLOYMENT.md](DEPLOYMENT.md). This is the design.

### Transport

Every device dials out to one or more Nostr relays over WebSocket, so sync
works behind NAT with or without a VPN. Payloads are NIP-44 v2 encrypted to
one device and events are signature-checked; relays see ciphertext, kinds,
sizes, timestamps and public keys. The only relay nerve provides is
`nerve sync serve-relay`, an in-memory store used as a fast private path
(about 16 ms against 0.5-0.9 s on public relays). Bulk data goes only
there when one is connected.

Not implemented: NIP-59 gift wrap (hides the sender key), NIP-42 relay
auth, key rotation beyond editing `[peers]`.

### Process model

One agent works per device: whoever holds the `agent.lock` flock, which
frees itself on exit or crash. A TUI that finds it held is a viewer;
`nerve sync agent` (the service) waits for the lock. CLI commands drop
requests in `spool/` and follow `activity.log` for acks. Claude sessions
reach the agent through the nerve mod over a 0600 Unix socket
(`modhub.rs`); without the mod, the settings hook and `mailbox/` do the
same job for permission prompts and questions.

### Protocol

- **Device state.** Every 3 s the agent scans local sessions and
  republishes its state on change, and at least once a minute: non-private
  sessions (state, cwd, branch, cost, an 8-line transcript tail), pending
  prompts, runs, approvals, capabilities and the away flag. It is an
  addressable event (kind 30078, one per recipient), so a relay keeps the
  newest copy and a closed laptop still shows what it was doing. "Last
  synced" is the time the peer generated that state.
- **Envelopes.** Requests are deduplicated by id across restarts (`seen`).
  Anything typed or started expires after 5 minutes, because typing is not
  idempotent and a replay must never land.
- **Instructions.** The receiver finds the tmux pane (by tty) or Ghostty
  tab, re-validates that the PID is a live `claude`, strips control
  characters and pastes as a bracketed paste. With the mod it submits
  through `$.prompt.submit` instead.
- **Prompts and questions.** With the mod both go remote while Claude's
  dialog stays up; the first answer wins. With only the hook, a permission
  prompt races the local keypress, and a question (whose dialog is hidden
  while the hook waits) goes remote only in away mode.
- **Hand-off.** Offer (session, repo by root commit), have (target's commit
  tips), pack (bundle of missing commits plus chunked working-tree patch,
  untracked files, transcript, subagent sidecar), push (index, then chunks
  the target lacks), unpack (restore where told, never over uncommitted
  work, install the transcript under the slug of the new path, resume in a
  detached tmux session `nerve-<id>`), remember (per-peer ledger of held
  chunks). Chunks are content-defined (2/8/32 KiB, SHA-256) and live in
  `chunks/`. A start-session request reuses the tail of this path: the
  sender picks the session id, the target runs `claude --session-id` in tmux
  with the prompt as one argument, no shell.
- **Places, files, refs.** `put`/`get` move files through the same chunks
  and ledger, written atomically into a named place, with every path checked
  to stay inside it and no symlink followed. Files at or over `bulk_over`
  skip the relay and go over rsync/ssh, which is also the fallback when the
  relay fails. `refs` ships a git bundle and moves only the named
  destination refs with `update-ref`; nothing is checked out.
- **Runs.** A command runs only if the receiver's `[run] allow` names it;
  no shell unless a rule says so; a 20-line merged output tail streams back.
- **Synced folders.** Each device keeps an index (size, mtime, SHA-256) and
  per peer which version it holds; the difference travels as a `put`. Both
  sides resolve a conflict the same way: the device whose key sorts first
  keeps the name and the other copy is saved as `<file>.conflict-<device>`.
  A deletion applies only where the file is still the version the deleter
  knew. Before a sync overwrites or deletes anything, the old bytes go to
  `versions/` (chunked, pruned after `keep_versions_hours`).

### Consent model

Trust is a list of public keys in `[peers]`; anything else is dropped. A
connected device can type into, stop and approve tool calls for every
non-private session, which is remote code execution by design: connect only
devices you control.

- **Connecting.** A code is single-use, lasts 10 minutes and connects
  nothing alone. The join becomes an approval; both screens show three words
  derived from both keys and the secret. A stranger's pair request is read
  only while a code is open. The inviter introduces the new device, and
  introduced devices trust it on the inviter's word and drop it when any
  trusted device forgets it.
- **Approvals.** Two things wait for a person's yes on the device they would
  change: a join, and a folder that no rule covers. They live in
  `approvals.json`, are published in device state, show under Needs you on
  every device, and any connected device may answer them.
- **Folder rules.** `ask` (nothing), `inside` (folders under the parent
  folder) or `any` (anything not private) say what a shared folder lets its
  devices sync next without asking. A TUI-started sync is the person's yes.
  A rule is a convenience, not a wall against a compromised device.
- **Never synced or started in:** private folders, `~/.ssh`, `~/.gnupg`,
  `~/.aws`, `~/.kube`, `~/.docker`, `~/.config/gcloud`, `~/.netrc`, Claude's
  config roots, nerve's config, and any folder containing one of them.
  Started sessions and hand-offs land only inside home, a place or
  `handoff_root`.
- **Privacy.** A private session or folder is never published; the receiving
  agent and hook refuse any request for it. Privacy does not stop
  user-started transfers, but keeping a synced folder private stops its sync.
- **Executable content.** A synced folder can carry hooks, scripts or
  `.envrc`; syncing trusts every sharing device with what runs there.
