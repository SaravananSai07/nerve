# Changelog

## Unreleased — TUI / UX changes

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

## Unreleased — wire-format and API changes

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

## Unreleased — comprehensive audit remediation

A 76-issue audit (security review + architecture review + low-level
systems review) drove this work. The full text of the audit is in
the project's review thread; this entry summarises what changed
behaviourally and why.

### Incident fix: orphan + 100% CPU after terminal close

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

### Security hardening

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

### Architecture

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

### Performance

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

### Operational hygiene

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

### Deferred (with explicit rationale)

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

### Test coverage

33 unit tests at the start of the audit → 89 at the end, plus
one end-to-end integration test (`lockfile_e2e`) that spawns the
real binary and asserts a second instance exits cleanly with the
expected stderr.
