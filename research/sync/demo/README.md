# Run the prototype

Two ways to see `nerve sync` working: on one Mac in a minute, or across real devices.

## One machine, no setup

```bash
research/sync/demo/demo.sh           # relay + "beta" agent, then the TUI as "alpha"
research/sync/demo/demo.sh --check   # the same, non-interactive; prints status and exits
research/sync/demo/demo.sh --job     # also runs a headless Claude job on beta (costs a few cents)
research/sync/demo/places.sh         # put/get/refs/run between two agents, each side initiating; exits non-zero on failure
```

The script does the following:

1. Builds `nerve` with `--features sync` into `target/release`.
2. Starts the embedded private relay on `ws://127.0.0.1:7788`.
3. Creates two devices, **alpha** and **beta**, each with its own `NERVE_CONFIG_DIR` under `research/sync/demo/.state/`, and pairs them. Your real nerve config is not touched.
4. Runs beta's sync agent in the background, waits for its first state event, and prints `nerve sync status`.
5. Runs a manual sync, then opens the TUI as alpha.

In the TUI:

| key | what you see |
|---|---|
| `D` | Devices: beta's sessions, last-synced time, pending prompts, jobs |
| `r` | sync now |
| `m` | type an instruction into the selected session |
| `d` / `l` / `g` | fetch the session's diff, transcript log, or subagents |
| `a` / `x` / `1`–`9` | answer a pending permission or question |
| `H` | hand the selected session off to beta (it resumes in a detached tmux session, checkout under `.state/beta-work`) |

Both "devices" run on this Mac, so beta reports the same Claude sessions you have open. An instruction sent with `m` is typed into a real session.

Logs are in `.state/relay.log` and `.state/beta-agent.log`. Quitting the TUI stops the relay and beta.

## Across real devices (what the experiments used)

See `docs/SYNC.md` for the full guide. In short:

```bash
cargo build --release --features sync
nerve sync init --name mac --relay wss://relay.primal.net --relay wss://relay.snort.social
nerve sync pair pi npub1…            # and the reverse on the Pi
nerve sync agent                     # on the Pi
nerve                                # on the Mac: press D
```

To add a private relay over Tailscale (about 16 ms instead of 0.5–1 s), run `nerve sync serve-relay` on the Pi. Then run `nerve sync relay add ws://<pi-tailscale-ip>:7777` on both devices.

## What was verified, and where

| claim | evidence |
|---|---|
| Every requirement end to end, Mac ⇄ Pi | `experiments/RESULTS.md` E10–E16 |
| Public relays only, no Tailscale | E14 |
| Offline store-and-forward | E15 |
| A Mac agent delegating a verified job to the Pi | E16 |
| This one-machine demo | `demo.sh --check`, run 2026-10-10. It found and fixed a same-second state-publish collision; the regression test is `transport::tests::back_to_back_state_publishes_are_both_accepted` |
