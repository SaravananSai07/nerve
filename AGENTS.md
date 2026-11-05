# Nerve

Rust TUI that monitors running Claude Code sessions across Ghostty tabs and
tmux panes, and syncs them across devices. MSRV is 1.85.

## Commands

```bash
cargo run                                   # TUI
cargo run -- --dump | --list                # one-shot discovery (JSON / text)
cargo clippy --all-targets -- -D warnings   # CI gate
cargo test --all-targets                    # CI gate
docker build -t nerve-linux-e2e tests/linux && \
  docker run --rm -v "$PWD":/src:ro nerve-linux-e2e bash /e2e.sh   # CI gate
```

## Rules

- Wrap every string from outside (session files, transcripts, ps, git,
  peers) in `Sanitised` before storing or rendering it.
- Visibility is `pub(crate)` at most; `#![deny(unreachable_pub)]` enforces it.
- Gate macOS-only code (Ghostty, AppleScript, notifications) with
  `#[cfg(target_os = "macos")]`.
- Compare process names through `process::comm_name`. `ps` reads `ucomm`
  (a bare name); the `rsplit('/')` inside keeps a full path safe.
- The discovery worker does all I/O. The UI thread never touches the
  filesystem or spawns processes on a tick.
- Find Claude's data only through `detect::claude_roots()`. Never hardcode
  `~/.claude`; users relocate it with `$CLAUDE_CONFIG_DIR`.
- Before sending a signal, re-validate that the PID still belongs to a
  `claude` process (PIDs get recycled).

## Code structure

- Prefer few dense modules that each own one feature end to end (its state,
  keys, actions and view) over many shallow modules split by layer.
- Merge a thin file into its only user. Split a file only along a real seam.
- Keep a helper next to its only caller; share it only when a second caller
  appears.
- `src/tui/` is pure rendering: no I/O, no state. One file per page.

## Repo hygiene

- No screenshots, demos, experiments or research in commits. Keep them in
  the git-ignored `ignored/` directory.
- Docs are only `README.md`, `CHANGELOG.md`, `AGENTS.md` and the three below.
  Do not add others.

## Docs

- `docs/ARCHITECTURE.md`: module map, data flow, state model, CPU budget, sync design
- `docs/DEPLOYMENT.md`: install, run the sync agent as a service, connect devices, upgrade, troubleshoot
- `docs/TESTING.md`: the Linux Docker suite, and manual checks with real sessions
