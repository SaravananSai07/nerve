# Nerve

Rust TUI that monitors running Claude Code sessions across Ghostty tabs and tmux panes.

## Commands

```bash
cargo run                                   # TUI
cargo run -- --dump | --list                # one-shot discovery (JSON / text)
cargo clippy --all-targets -- -D warnings   # CI gate
cargo test --all-targets                    # CI gate
docker build -t nerve-linux-e2e tests/linux && \
  docker run --rm -v "$PWD":/src:ro nerve-linux-e2e bash /e2e.sh   # CI gate
```

MSRV is 1.85.

## Rules

- Wrap every string that comes from outside (session files, transcripts, ps,
  git) in `Sanitised` before storing or rendering it.
- Visibility is `pub(crate)` at most; `#![deny(unreachable_pub)]` enforces it.
- Gate macOS-only code (Ghostty, AppleScript, notifications) with
  `#[cfg(target_os = "macos")]`.
- Compare process names through `process::comm_name`. `ps` reads `ucomm`
  (a bare name), and the `rsplit('/')` inside keeps it safe if a full path
  ever arrives.
- The discovery worker does all I/O. The UI thread must never touch the
  filesystem or spawn processes on a tick.
- Locate Claude's data only through `detect::claude_roots()`. Never hardcode
  `~/.claude`, because users relocate it with `$CLAUDE_CONFIG_DIR`.
- Before sending a signal, re-validate that the PID still belongs to a
  `claude` process, to avoid hitting a recycled PID.

## Reference

- `docs/ARCHITECTURE.md`: module map, data flow, state model, CPU budget
- `docs/TESTING.md`: the Linux Docker suite, and manual checks with real
  Claude sessions, Ghostty and the desktop app
- `README.md`: user-facing behaviour, keybindings, config, themes
- `CHANGELOG.md`: release history
