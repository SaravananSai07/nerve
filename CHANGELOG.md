# Changelog

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
