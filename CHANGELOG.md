# Changelog

## Unreleased

### Added

- **A new interface.** The card grid is replaced by one list of
  sessions grouped as Needs you, Working and Idle, with status as a
  shape (● asking, ◆ permission, ▲ failed, a spinner, ○ idle, ◌ offline)
  so it reads without colour. The cursor stays on its session when the
  list reorders. `Enter` opens a panel with the question, the
  transcript and a prompt box, beside the list at 110 columns or wider
  and full screen below. A Settings page (`Tab`) holds devices, private
  folders and sessions, who gets notified and the theme. The bottom line
  shows the keys that work right now; `?` lists them all.
- **Answer and prompt from nerve.** `1`–`9` answers the question or
  permission prompt under the cursor, and `i` sends a prompt (queued
  while a turn runs).
- **A readable transcript.** The panel renders Claude's markdown
  (headings, lists, tables, code) and folds long runs of tool calls
  into one line. What Claude Code adds to a turn (reminders, command
  output, compaction summaries) stays out. The wheel, `PgUp`/`PgDn` and
  `Home`/`End` scroll it. A session opens at once on the transcript
  last read, and a spinner shows while the first read runs. Working
  sessions spin about ten times a second.
- **Notify when it finishes.** Every session's finished turn notifies
  the devices ticked under Finished turns in Settings, except on the
  device where nerve has that session open. `n` overrides it for one
  session (⚑ marks those that notify): switch it off or pick other
  devices. Sessions switched on with `n` before keep notifying.
  Questions, permission prompts and failures notify every device ticked
  under Needs you in Settings. Titles say what and where
  (`Question on laptop`). On macOS a
  click runs the new `nerve --land <device>/<session>`, which opens that
  session in the running nerve, lit up for a moment. Choices are kept in
  `notify.toml`.
- **Cross-device sync (experimental).** Devices pair
  by Nostr key and exchange NIP-44-encrypted state and commands through
  public relays or a private one (`nerve sync serve-relay`). Sessions on
  every paired device share the one list, named by device, and the same
  keys answer, prompt, stop (`x`) and open them; `H` hands a session to
  another device. See docs/DEPLOYMENT.md and docs/ARCHITECTURE.md.
- **Start a session on a device.** `s` (or `nerve sync start <device>
  <dir> <prompt> [--wait] [--timeout S]`) runs `claude <prompt>` in a
  detached tmux session in a folder on any connected device, the way a
  hand-off resumes one, so it is an ordinary session: it shows in the
  list, takes prompts and approvals, and can be attached to. The prompt
  is one argument, never through a shell. The folder must exist inside
  the device's home, a place or `handoff_root`, and not be private. The
  CLI prints the session id, then the tmux session; `--wait` also waits
  for the first turn to end and prints the reply. It replaces the
  headless `nerve sync job`, whose peers could pick a `verify` shell
  command and `bypassPermissions`; a device still sending a job gets a
  refusal.
- **Private sessions.** Every session is visible on every paired
  device unless you keep it private: `p` keeps the session, or its whole
  folder, on this device (marked ⊡), as does `nerve sync private
  [<dir>]`. Peers can't see, type into, stop or answer a private
  session. fleet.toml keeps the lists as `private` and
  `private_sessions`.
- **`nerve setup [code]`** sets a device up in one step. It creates the
  identity, installs the nerve mod into Claude Code, installs the agent as
  a launchd, systemd or Termux service, and pairs by a one-time invite
  code. `nerve sync doctor` checks a device; `nerve sync invite` makes a
  new code.
- **Claude Code mod** (`mod/nerve`, Claude Code ≥ 2.1.287). It submits
  remote prompts inside Claude (queued while busy, works for `--bg`
  sessions), and answers permissions and questions from other devices
  while the local dialog stays up.
- **Incremental hand-off.** Only the commits the target lacks and
  content-defined chunks it doesn't hold are sent. A repeat hand-off of a
  700 KiB session moved 16–19 KiB.
- **Places, runs and refs.** Each device names
  directories under `[places]` in its fleet.toml, so peers address a
  place by name and never need its remote path.
  - `nerve sync put|get <device> <place> <paths…> [--delete] [--wait]`
    moves files through the hand-off's chunk store. Either side can
    start it. A repeat put of a 1 MiB folder with one changed file moved
    8.9 KiB (2 of 237 chunks). Files are written atomically, with their
    modes, only inside the place and never through a symlink.
  - `nerve sync refs <device> <place> [--pull] <src>:<dst>…` sends only
    the commits the target lacks and sets exactly the named refs. It
    never checks out and never touches HEAD, the index or the working
    tree.
  - `nerve sync run <device> [--place P | --cwd DIR] [--timeout S]
    [--wait] -- <argv…>` runs a command that a `[run] allow` rule on the
    target names (a trailing `"*"` admits further arguments). The argv is
    passed as given, with no shell. Runs stream an output tail, show in
    `nerve sync status`, and with `--wait` the CLI
    exits with the remote exit code. The 5-minute expiry bounds only
    *starting* a run.
- **Synced folders.** A folder can stay the same on the devices
  you choose: `a` on the new Sync page, or `nerve sync <dir> --with
  <devices> [--when turn|watch|manual]`. It syncs when a session
  there finishes a turn, when files change, or only when asked (`s`,
  `nerve sync <folder>`). Small changes travel through the relays in
  batches and files over `[xfer] bulk_over` go over rsync. A file both
  devices changed keeps the version from one device, chosen alike on
  every device, and saves the other as `<file>.conflict-<device>`. The
  Sync page shows how each folder stands and what went where; sessions
  in a synced folder carry ⇄. Keeping a synced folder private stops
  syncing it.
- **One build, one install.** Sync is always built in: the `sync` cargo
  feature and `install.sh --sync` are gone, and nothing talks to a relay
  until a device is connected. `nerve --version` prints the version.
- **Connect devices from nerve.** `c` in Settings (or `nerve connect`)
  shows a code; the other device enters it (or runs
  `nerve connect <code>`). The join then waits for a yes on any of your
  devices, with three words both screens show. A code works once, for 10
  minutes. The inviter chooses who the new device joins (every device and
  later ones, the devices there now, or some), and the new device picks
  among those. It is introduced to each of them. `l` and `nerve link`
  connect a device to more devices later; `f` and `nerve forget` drop it
  from all of them.
- **Requests.** A folder another device shares waits for a yes under
  Needs you, on every device, unless a rule covers it: each synced folder
  says what its devices may sync next (`ask`, `inside` its parent folder,
  or `any`), set when it starts and with `o` on the Sync page. A sync
  started in the TUI is your yes; one started from a shell asks.
  `nerve requests`, `nerve allow <id> [--at] [--others]` and
  `nerve refuse <id>` do the same from a shell. Keys, credentials and
  nerve's and Claude's own settings never sync.
- **Undo a sync.** Files a sync replaces or deletes are kept for 48 hours
  (`keep_versions_hours`), stored by content hash. `v` on the Sync page,
  `nerve versions` and `nerve restore <id>` put one back.
- **Everyday commands at the top level:** `nerve connect`, `devices`,
  `link`, `forget`, `requests`, `allow`, `refuse`, `sync <dir>`,
  `unsync`, `folders`, `versions`, `restore` (`nerve help` lists them).
  `nerve sync folder …` and `nerve sync invite` are gone; `nerve setup`
  stays as another name for `nerve connect`.

- **Starts show at once.** A session started on another device (`s`)
  joins the list straight away as Starting, with its prompt and how far
  it got, and becomes the real session once that device reports it. A
  device that refuses it or never answers moves it to Needs you with
  the reason, to retry (`r`), edit (`e`) or dismiss (`x`).
- **A multi-line prompt box.** Starting a session and `i` wrap long
  prompts instead of scrolling sideways, take new lines (`⌥↵`, `⇧↵`,
  `Ctrl+J`) and keep a paste's line breaks; past a few lines the box
  scrolls to follow the cursor.

### Changed

- **Keys.** `t` jumps to the terminal (was `Enter`), `Enter` opens the
  panel (was `i`), `r` renames (was `n`), `P` captures the screen into the panel,
  `/` finds across title, project, folder and device.
- **Themes have four roles:** `text`, `dim`, `attention` and `accent`,
  plus optional `selection` and `flash`. Older theme files still load
  (`muted`, `waiting`, `processing` and `selected_bg` map onto them).
  The default theme is now `terminal`, which uses your terminal's own
  colours.
- **Hand-offs land only where you said.** A hand-off goes to the place
  given with `--to <place>`, else the `path_map`-mapped path, else under
  `handoff_root` when that is set. With none of them it is refused, and
  the message names the config to add. It no longer falls back to
  `~/nerve-handoff`. An existing checkout on another branch, or one whose
  branch has commits the hand-off lacks, is refused unless you pass
  `--force`; it is no longer switched with `git checkout -B`.
- **Services find `claude`.** `nerve setup` writes a PATH into the
  launchd plist and systemd unit that includes `~/.local/bin`,
  `~/.cargo/bin`, `/opt/homebrew/bin` and `/usr/local/bin` where they
  exist. Hand-offs, started sessions and runs also look in those
  directories when PATH misses, and the error lists where they looked.

### Fixed

- **Whole transcripts from other devices.** A session elsewhere showed
  only its last eight lines, each cut at 240 characters, so replies
  broke off mid-sentence and your prompt dropped out once tools ran. Its
  device now sends the whole recent transcript while you have it open.
- **No doubled or invented prompts.** A prompt the transcript holds
  twice shows once, and what Claude Code adds as a hidden user turn (a
  skill's or command's expanded text, a subagent's report) no longer
  reads as something you typed.
- **Stopping or attaching to a background job failed when it lived under
  the other Claude config dir.** nerve watches both `~/.claude` and
  `$CLAUDE_CONFIG_DIR`, but ran `claude stop` / `claude attach` with
  whichever one it inherited, so the CLI couldn't see the job ("couldn't
  confirm … was stopped"). Every CLI call now targets the session's own
  config dir.
- **nerve still pinned a CPU core when its tmux server was killed.** The
  pty stays nerve's controlling tty in that case, so the 0.5.0 watchdog
  never fired. It now also checks the tty for a hang-up.
- **Background jobs blocked for months showed as "Waiting 3m".** With no
  transcript, nothing dated them; the job file now does, and after 48 h
  they show as Dormant like any stale prompt.
- Plain `cargo run` works again (it broke when the `nerve-tui` alias
  binary was added).
- **A fresh install crashed on launch on macOS.** `install.sh` copied the
  new binary over the old one in place, and macOS kills a binary
  rewritten that way. It now copies beside it and renames.
- **Upgrades left the background agent and the Claude mod on the old
  version.** The agent now restarts itself when its binary is replaced,
  and rewrites the mod's files when they differ from its own copy.
- **A pasted code typed in slowly**, one key per frame; keys that
  arrive together are now taken together.
- **`nerve sync …` without a terminal crashed** in a build without sync,
  falling through to the dashboard. Every build has sync now, and the
  dashboard refuses to start with no terminal instead of panicking.

### Removed

- Sorting (`s`), the activity sparkline, muting (`m`), the update banner
  and its dismissal (`u`; a new release is now mentioned once at
  launch), the Devices view (`D`) and the details view (`i`).
- The `on_waiting`, `on_error` and `on_complete` options under
  `[notifications]`: Settings and `n` decide who hears about what.
  `sound` stays.

### New

- **Rows name the repo** when Claude's session name doesn't. For Claude
  worktrees it's the repo, not the worktree folder.
- **Background jobs blocked for days can still be stopped** with `x`,
  though they show as Dormant.

## 0.5.2 — 2026-10-09

- Repackage of 0.5.1 without stray build artifacts (cargo-install
  bookkeeping and a compiled binary) that were accidentally committed and
  published with it. No code change; 0.5.1 is yanked.

## 0.5.1 — 2026-10-09

### Fixed

- **`cargo install nerve-tui` installed the command as `nerve-tui`**, so
  `nerve` (as the README and `nerve update` assume) was "command not
  found". Only `install.sh` renamed it. The binary is now `nerve` for every
  install method.
- `nerve-tui` remains as an alias that runs `nerve`. Without it, a
  `cargo install` upgrade would leave the old `nerve-tui` binary behind,
  since cargo doesn't remove binaries a package stops providing.

## 0.5.0 — 2026-10-09

Catches nerve up with Claude Code 2.1.x and fixes a CPU runaway.

### Fixed

- **Orphaned nerve pinned a CPU core forever.** When the terminal
  went away (tab closed, tmux killed, SSH dropped), crossterm's
  `try_read` spun on `read() == 0` and never returned, so the lost-tty
  and SIGHUP checks never ran. A watchdog thread now force-exits
  ~2.5 s after the tty is lost.
- **`$CLAUDE_CONFIG_DIR` was ignored.** nerve only read `~/.claude`,
  so users who relocate Claude's config saw no sessions at all. Both
  roots are now scanned (absolute paths only, matching Claude ≥ 2.1.284).
- **Claude desktop-app sessions were invisible.** Their binary path
  contains a space, which broke `ps -o comm` parsing; switched to
  `ucomm`.
- Background sessions with a live process showed as Idle forever: their
  pid file reports `status: "shell"` for the job's whole life. Their state
  now comes from the job's `state.json`, the same source `claude agents`
  uses.
- **Didn't build on the declared MSRV (1.85), and CI has been red since
  0.4.1.** `instability` 0.3.12 / `darling` 0.23 need rustc 1.88; pinned
  `instability` to 0.3.9 in Cargo.lock, and `nerve update` / `install.sh`
  now pass `--locked` so installs use it. Linux clippy also failed on
  macOS-only items that were never gated, and one test flaked on Linux
  inode reuse.
- A newly started session could take up to `process_scan_interval_ms`
  (5 s) to appear: its pid wasn't in the cached process table yet. The
  worker now re-scans processes when a session file names an unseen pid.
- Ghostty live preview (`P`) returned nothing on Ghostty 1.3: the screen
  capture walked a fixed accessibility path that's one level shallower
  than 1.3's layout. It now finds terminal text areas by class.
- The log preview showed "No log entries" for fresh sessions: typed
  prompts are stored as a plain string (not a content array), and large
  `attachment` rows could fill the 64 KB tail window. Both handled.
- Transcript lookup used `/`→`-` encoding; Claude encodes every
  non-alphanumeric character, so worktree sessions always fell back to
  the slow scan.

### Added

- State comes from Claude's own `status` (`busy` / `waiting` / `idle`)
  in the session file when present, committed without the 3-tick
  debounce. The transcript heuristics remain for older Claude versions.
- Waiting reason (`waitingFor`, or a background job's open questions)
  shown on the card, in `--list`, and in notifications.
- Session names from Claude (`/rename`, auto-titles) instead of the
  cwd's last component.
- Desktop and `claude --bg` sessions, including parked jobs that are
  blocked on you with no live process. Enter on a background job runs
  `claude attach` in a tmux split (clipboard elsewhere), `x` runs
  `claude stop`, and previews show the transcript or, for parked jobs
  with none, the job's state timeline.
- `nerve statusline [-- <cmd>]`: a Claude `statusLine` hook that
  records Claude's own cost, context-window % and model. Cards and
  totals prefer these over the transcript estimate.

## 0.4.1 — 2026-05-28

Post-`0.4.0` cleanup pass — no observable runtime change.

### Operational

- New `.github/workflows/ci.yml` — `cargo build`, `cargo clippy
  --all-targets -- -D warnings`, and `cargo test` on `ubuntu-latest`
  + `macos-latest`, plus a job pinned to the declared MSRV. The
  `#![deny(unreachable_pub)]` lint and the 130-test trunk now
  actually enforce on PRs.
- `rust-version` bumped from `1.75` to `1.85`. The code didn't
  need anything 1.85-specific, but pretending to support a
  2.5-year-old toolchain was misleading; the MSRV CI job keeps
  the declaration honest.

### Sanitised type-wall closure

- `impl Serialize for Sanitised` (delegates to the inner `&str`),
  so `DiscoverySnapshot` can carry `Sanitised` fields without
  changing the `--dump` JSON shape.
- `DiscoverySnapshot.{name, tty, branch, current_tool}` lifted
  from `String` / `Option<String>` to `Sanitised` /
  `Option<Sanitised>`. The discovery worker is the ingestion
  boundary; `Session::from_snapshot` and `merge_snapshot` no
  longer re-wrap.
- Outgoing `From<&Sanitised> for Cow<'_, str>` lets the preview
  overlay borrow text directly into `Span::styled` instead of
  cloning — five allocations per visible log line per frame
  removed.
- `PendingNotification.name` lifted to `Sanitised`.

### Decomposition

- `detect/claude.rs` (922 LOC) split: JSONL transcript parsing
  (`read_tail_state`, `parse_token_usage`, `read_tail_entries`,
  `extract_tool_result_snippet`, plus their helper structs and
  tests) moved into a new sibling `detect/jsonl.rs`. `claude.rs`
  now 425 LOC focused on session JSON + discovery orchestration.
- `state/session.rs` (659 LOC) split: `TokenUsage` and
  `ActivityHistory` moved to sibling files `state/token_usage.rs`
  and `state/activity.rs`. `session.rs` now 592 LOC.

### Polish

- `list_project_dirs` switched to the same `enumerate+break+warn`
  pattern as the sessions-dir enumeration (was
  `take(MAX+1)+truncate`). Both `read_dir` caps now read
  identically.
- `platform::ghostty::focus_terminal` and `platform::tmux::focus_pane`
  narrowed from `pub(crate)` to `pub(super)` (only consumer is
  `platform::mod::BridgeId::focus`).

## 0.4.0 — 2026-05-25

### Code-quality + type-safety pass

- `Sanitised` newtype in `util::sanitize` carries the
  `strip_ansi` + control-byte invariant from JSONL / ps / git
  ingestion through to TUI display. `Session.name`, `tty`,
  `branch`, `current_tool` and every text-bearing `LogEntry`
  variant use it; a code path that constructs one of these
  fields from a raw `String` no longer compiles.
- Filesystem reads now route through `util::fs::read_capped`
  with named byte budgets (theme TOML 64 KiB, prefs.toml
  64 KiB, config.toml 256 KiB, update\_cache.json 16 KiB, git
  HEAD 4 KiB, lockfile PID 64 B). A sibling process or
  dotfile-sync glitch can't OOM the cold-start path.
- `~/.claude/{sessions,projects}/` enumeration capped at
  10 000 entries; the cap fires a `log_warn!` so runaway state
  surfaces in `nerve.log` rather than silently degrading the
  dashboard.
- `crates.io` update check post-validates stdout size on top
  of `curl --max-filesize`.

### CLI contract hardening

- `--dump` now always emits valid JSON on stdout. On a serde
  error, the payload is `[]`, the error message goes to stderr,
  and the process exits with code 2 — so `nerve --dump | jq`
  pipelines fail through exit codes instead of producing
  unparseable stdout the way the prior `unwrap_or_else` did.
- `--list` sanitises `tty` values at ingestion alongside branch
  names (which were already filtered via `safe_refname`).

### Internal: `app.rs` decomposition

The main loop's god-object (1038 LOC) split into submodules
with sibling-private (`pub(super)`) methods:

- `app/input.rs` — keyboard handlers
- `app/overlays.rs` — overlay open/start/execute actions
- `app/discovery.rs` — worker-thread integration +
  `PendingNotification`
- `app.rs` — `App` struct, constructor, run loop, view ops

No behaviour change; cohesion + grep-ability only.

### Visibility

- Crate-wide `pub` items narrowed to `pub(crate)`;
  `#![deny(unreachable_pub)]` at the crate root prevents new
  drift.
- `process.rs` helpers (`build_child_map`, `find_process`,
  `has_child_named`, `resume_session_id`, `valid_session_id`)
  narrowed to private or `pub(super)` per their actual call
  scope.

### Operational

- `prefs.toml` parse failures surface to the startup status bar
  (matching `config.toml`) instead of vanishing into
  `nerve.log`.
- `SessionRegistry::mark_stale` returns `bool` — vanished
  sessions in the 60 s grace window no longer re-fire
  `re_disambiguate_names` and `FilteredView` invalidation on
  every tick.
- `Ctrl+C` matches both lowercase and uppercase `c` so Caps
  Lock / Shift don't break the "always quits" contract.
- Theme name fallback to `custom` (empty post-sanitise or
  > 64 B) now logs the reason.

### Testing

- New `tests/cli_smoke.rs` integration suite: 4 tests exercise
  `--dump` JSON shape and `--list` cold paths against an
  isolated `HOME` and config dir, guarding the wire-format
  contract scripts depend on.
- 125 unit tests (up from 89 pre-audit) + 5 integration tests.

### TUI / UX changes (pre-audit wave)

**Keybindings**

- `g` no longer aliases Enter for "switch to session's tab". Enter is
  now the only navigation key for that destructive cross-app action.
- `u` dismisses the update banner (per-version; a newer release
  surfaces it again).
- `q` inside any overlay closes the overlay rather than quitting the
  app. Ctrl+C still quits unconditionally.
- The confirm-kill modal binds Enter to cancel (safer default for a
  destructive prompt); only `y` / `Y` fires SIGTERM.
- Preview overlay grows PgUp/PgDn, g/G, and clamps the scroll cursor
  so holding `j` past the bottom doesn't trap the cursor.

**Help overlay**

- Documents every binding the code accepts (Shift+P live capture,
  Ctrl+C, scroll keys). Grouped into navigate / actions / view /
  preview / app / preferences sections.
- Caps height to the available frame so small terminals don't draw
  half-off-screen.

**Rendering**

- Sparkline switches from `▓░` to `█▁` for a flush baseline on
  every monospace font (Apple Terminal rendered the shaded blocks
  unevenly; an intermediate middle-dot interim sat off-baseline).
- Theme grows a `muted` colour for chrome text (status hints, log
  numbers, modal hints) distinct from `idle` (state colour). User
  themes without `muted` fall back to `idle` so existing TOML files
  still load.
- Status bar splits left (counts) and right (chrome hints +
  `[muted]`). The right half is right-aligned so the muted indicator
  survives a narrow terminal.
- Card grid grows a 3-column mode at width ≥ 160 (previously capped
  at 2).
- Overlay `centered()` clamps to the parent area.

**Inputs**

- Rename caps by char count, not byte length — CJK names work to
  the full 48-character limit. The rename overlay also sanitises
  pasted control bytes at the boundary.
- Search overlay shows `fuzzy · case-insensitive` plus the keybinds.
- A search query that matches zero sessions now shows a dedicated
  empty-state card (`No sessions match "/query"`) with the Esc-to-
  clear hint, rather than an empty grid.

**Safety**

- Confirm-kill modal binds Enter / Esc / n / q to cancel; `y` is the
  only key that fires SIGTERM. Long session names truncate by
  grapheme cluster (no orphan combining marks).
- `g` removed as a Enter alias for tab-switching to defuse vim
  muscle memory.
- `q` inside any overlay closes the overlay; Ctrl+C is the
  unconditional quit.

**Robustness**

- Tool-result snippet truncation in the log preview now operates on
  grapheme clusters via unicode-segmentation.
- Update banner version string is sanitised at the network boundary
  (crates.io response can't smuggle ANSI through the banner).
- Update banner suppression is semver-aware: a yanked release that
  reverts to an older version doesn't re-open the banner.
- Preview overlay scroll cursor resets to 0 on empty buffers so the
  initial usize::MAX seed doesn't survive into a later read.
- 3-col grid descent (`j` / `Down`) snaps to the last session on a
  partial trailing row instead of pinning the cursor.

**Theme schema**

- Themes grow a `muted` colour for chrome text. User themes without
  it fall back to `idle` and log a warning at load — see the
  README's *Color keys* table for the full schema.

**Status messages**

- Coloured by intent (success/info/error) — successful kills read
  green, errors red, navigational notes neutral. Previously every
  message rendered as `theme.error`.

### Wire-format and API changes

**`--dump` JSON schema** now emits the discovery worker's snapshot
directly rather than a synthesised `Session` whose registry-owned
fields were always zeros:

- `state` renamed to `detected_state` — the CLI was always emitting
  the detected (pre-confirmation) state anyway.
- `activity` removed — was always the empty sparkline buckets.
- `renamed` removed — was always `false` for a one-shot CLI invocation.

Scripts relying on `jq '.[].state'` or `.activity` need to update.

**`--list` columns** are now `name | state | tty | branch | tokens`.
The duration column (always `0s` because no `Instant` existed in the
CLI path) and the sparkline column (always empty) are removed.

### Comprehensive audit remediation

A 76-issue audit (security review + architecture review + low-level
systems review) drove this work. The full text of the audit is in
the project's review thread; this entry summarises what changed
behaviourally and why.

#### Incident fix: orphan + 100% CPU after terminal close

The original incident: closing a Ghostty tab without typing `q`
left `nerve-tui` running, reparented to `launchd`, spinning at
100% CPU. Sometimes two instances ended up coexisting. This was
traceable to two compounding bugs:

1. Ghostty's known SIGHUP-non-delivery (ghostty#4554) meant the
   default-disposition kill never fired.
2. With stdin in a hung state, `crossterm::event::poll` returned
   `POLLHUP` immediately on every call; combined with crossterm's
   buffered writes hiding EPIPE, the loop spun at MHz rates
   without ever erroring out.

Fixed by:

- Installing `SIGHUP`/`SIGTERM`/`SIGINT`/`SIGQUIT` handlers via
  `signal-hook` that flip a shared atomic. The main loop checks
  it every iteration.
- A `tcgetpgrp(stdin)` canary that catches the lost controlling
  tty regardless of whether SIGHUP was delivered.
- A 50 ms minimum loop interval that defends against any future
  poll-true busy-spin without introducing noticeable input lag.
- An exclusive `flock(2)` lockfile at `~/.config/nerve/nerve.lock`
  that refuses a second concurrent launch with a clear message
  naming the holder PID.

#### Security hardening

Every subprocess invocation audited and rewritten where it
interpolated user-controlled values into a shell or AppleScript:

- Notifications and the Ghostty bridge no longer interpolate
  session names, cwds, or terminal IDs into `osascript` script
  bodies. A new helper passes values through `on run argv`
  exclusively. `escape_applescript` deleted.
- `terminal-notifier`'s `-execute` payload is double shell-quoted
  AND pre-validated against an allow-list charset; bridge IDs
  containing shell metacharacters are refused outright.
- `--focus` argument validated against length (≤ 160 bytes) and
  a restricted charset before any parsing.
- A new `strip_ansi` filter sanitises C0/C1/CSI/OSC/DCS sequences
  out of JSONL log content before it reaches the TUI, preventing
  cursor jumps and OSC 52 clipboard writes from untrusted
  transcripts.
- Session JSON reads capped at 64 KiB; `curl` updater capped at
  `--max-filesize 65536`.
- All JSONL opens use `O_NOFOLLOW`.
- `kill_by_session_id` re-validates the pid's `comm` is `claude`
  inside the same function as the `kill(2)` call, closing the
  pid-reuse TOCTOU.
- `git rev-parse` fork removed entirely; replaced by a direct
  `.git/HEAD` parser that handles worktrees and detached HEAD.

#### Architecture

- **Discovery worker thread.** `ps -eo`, `git`, JSONL reads, and
  session-state inference all run on a named background thread
  (`nerve-discovery`). The UI thread does only `try_recv` on an
  `mpsc` channel and rendering. A slow disk no longer freezes
  the dashboard.
- **FS watcher.** `~/.claude/sessions/` is watched via
  FSEvents/inotify. The worker sleeps until an FS event fires
  or the fallback 1 Hz timer ticks.
- **Configurable `process_scan_interval_ms`.** The `ps` snapshot
  is cached for the configured TTL (default 5 s) — roughly 80 %
  fewer forks than the prior per-tick scan.
- **`SessionId` newtype.** Encapsulates the `--resume`-vs-
  `sessionId` precedence rule. The registry keys on it.
- **`FilteredView` with version cache.** The per-frame filter
  recomputation is replaced with a version-cached `Vec<SessionId>`.
  Steady-state idle ticks do zero filter passes.
- **`Bridge::NoOp` variant.** Replaces the `Option<Bridge>` shape
  that peppered the codebase with `if let Some(bridge)` checks.
- **`StateMachine<T>` type.** The hand-rolled 3-tick confirmation
  logic that used to live inside `Session::propose_state` is
  now a tested standalone type.
- **State split.** `SessionState::Stale` was two fused semantics;
  it's now `Vanished` (process gone — evicted after grace
  period) and `Dormant` (idle ≥ 48 h but alive — kept
  indefinitely).
- **Loadable themes.** Drop a TOML at
  `~/.config/nerve/themes/*.toml` and it appears in the `t`
  cycle.
- **Cold-boot hint.** When `~/.claude/` doesn't exist, the empty
  state shows install instructions instead of "no sessions
  detected".
- **Transactional refresh.** Notifications are batched and
  dispatched after the registry is fully mutated, so a slow
  `osascript` spawn no longer interleaves with in-progress
  registry writes.

#### Performance

The combined effect of mtime short-circuit on JSONL reads,
process-scan caching, worker-thread offload, FS-event-driven
wakes, and skipping render when unfocused: idle nerve with a
dozen large JSONLs sits at near-zero CPU. The previous tail-read
hot spot (per-tick 256 KiB read + serde_json parse per session)
is skipped entirely when files are unchanged.

Smaller wins:

- O(1) pid lookup via `ProcessTable.pid_index`.
- Byte-level `ps` line iteration (no full-stdout String).
- Pre-canonicalised terminal cwds (no per-lookup `realpath(3)`).
- Single `Instant::now()` per frame (consistent timestamps + cheap).
- Coalesced burst Resize events (no render-per-pixel on edge drag).
- AppleScript probe moved off the startup path to a background
  thread (`nerve-ghostty-probe`).
- Inode-change detection on JSONL token-usage parsing so a log
  rotation doesn't double-count tokens.

#### Operational hygiene

- Panic hook restores the terminal before printing the panic.
- RAII `TerminalGuard` covers every exit path including errors.
- `SIGCHLD` reaper thread keeps zombie counts at zero.
- `setsid` on every subprocess spawn so a SIGTERM to nerve
  doesn't propagate to notification subprocs mid-display.
- `RLIMIT_NOFILE` raised at startup to `min(hard, 4096)`.
- Discovery scans that overrun a 750 ms budget log a warning
  with elapsed time + session count.
- New ring-buffer + file log at `~/.config/nerve/nerve.log`
  (rolls at ~1 MiB).
- Schema versioning + automatic migration on `prefs.toml`.
- Loud parse-failure surfacing on `config.toml` (status-bar
  message + log entry).
- `NERVE_CONFIG_DIR` env override for hermetic tests.

#### Deferred (with explicit rationale)

Four items were evaluated and consciously not implemented:

- **A2** Overlay state ownership: the externalised Preview-overlay
  fields are local to `App`; encapsulating them in the variant
  adds borrow-checker friction without changing behaviour.
- **A5** Single discovery implementation: `claude::discover_sessions`
  remains for the CLI `--list`/`--dump` paths; unifying with
  the worker-driven `discover_sessions_with` would require the
  CLI to spawn the worker just to print a snapshot once.
- **A10** Moving `LogEntry` out of `detect::`: cosmetic layering;
  no behaviour change.
- **A17** `App::new` pure: it already is, modulo spawning the
  discovery worker, which is fundamental to operation.

A handful of micro-optimisations (L9 mmap, L12 ratatui buffer
reuse, L21/L22 streaming serde, L23 borrowed disambiguation refs,
L27 JSONL FD cache, L37 model rate cache, L38 static sparkline
table) were similarly deferred — the dominant per-tick costs
they would target are already eliminated by L4 (mtime
short-circuit), L7 (pid index), L8 (byte-line ps), and L26
(unfocused render skip).

#### Test coverage

33 unit tests at the start of the audit → 89 at the end, plus
one end-to-end integration test (`lockfile_e2e`) that spawns the
real binary and asserts a second instance exits cleanly with the
expected stderr.
