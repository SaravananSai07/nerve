# Deployment

Installing nerve, running its sync agent as a service, connecting devices,
and keeping them working. For how sync is built, see
[ARCHITECTURE.md](ARCHITECTURE.md#sync). For keys and what each feature
does, see the README.

## Install

```bash
# One-liner (installs from crates.io)
curl -fsSL https://raw.githubusercontent.com/SaravananSai07/nerve/master/install.sh | bash

./install.sh                       # from a cloned repo: builds from source
cargo install nerve-tui --locked   # manually; --locked keeps tested dependency versions
```

All three install the command `nerve` at `~/.cargo/bin/nerve` (the crate is
`nerve-tui` because `nerve` was taken). Cross-device sync is part of every
build; nothing talks to a relay until a device is connected.

`install.sh` takes no arguments. It installs Rust through rustup if cargo
is missing (it asks first), asks whether to enable update checks, and on
macOS offers `terminal-notifier` for click-to-activate notifications. On
Termux it installs `rust`, `git`, `tmux` and `openssh` from `pkg`. The first
install takes a few minutes. The MSRV is Rust 1.85.

Requirements: Claude Code 2.1.287 or later for the nerve mod (without it,
sessions fall back to tmux/Ghostty paste and the settings hook), `tmux` on
any device that should accept started sessions or hand-offs, and `rsync`
plus ssh if you want files of 64 MiB and up to move.

## Connect devices

On the first device:

```bash
nerve connect                 # first run sets this device up, then shows a code
```

On each new device, install nerve, then:

```bash
nerve connect nerve1…         # join with the code
```

In the TUI, Settings then `c` does the same. `nerve setup` is an alias.

The first run on a device creates its key, installs the nerve mod into
Claude Code, and installs the background agent as a service (see below).

1. The code is a one-time secret that lasts 10 minutes. It carries the
   showing device's key, relays and the devices on offer, as text and a
   terminal QR.
2. The new device sends the secret with the devices it wants.
3. The join waits for a person. Both screens show three words; allow it
   only if they match. Any connected device can answer: `y` in the TUI,
   or `nerve allow <id>` (`nerve requests` lists them).
4. The inviter trusts the new device and introduces it to the devices it
   joins.

Choose who a new device meets with `--with all` (default, includes devices
added later), `--with now`, or `--with studio,pi`. Afterwards:

```bash
nerve devices                 # who is connected, online, and which nerve version
nerve link pi studio          # let pi meet studio (or `all`)
nerve forget pi               # every device stops trusting pi
nerve sync doctor             # is this device set up and talking?
```

Flags: `--name N`, `--relay URL` (repeatable), `--with …`, `--no-service`,
`--no-claude`, `--no-wait` (show the code and return; allow with
`nerve allow`). The pieces underneath remain: `nerve sync init`, `pair`,
`relay add|rm`, `install-hooks`.

Never copy `device.key` between devices. Two agents with one key both act
on every command. Treat the key like an SSH private key.

## The sync agent

`nerve connect` installs the agent, so there is nothing to start by hand.

| platform | how it runs | log |
|---|---|---|
| macOS | launchd agent `dev.nerve.sync`, plist in `~/Library/LaunchAgents/` | `<config>/sync/agent.log` |
| Linux | systemd user unit `nerve-sync.service` in `~/.config/systemd/user/` | `journalctl --user -u nerve-sync` |
| Termux, containers | detached `nerve sync agent`; a Termux:Boot script `~/.termux/boot/nerve-sync` starts it at boot | `<config>/sync/agent.log` |

On a headless Linux server, run `loginctl enable-linger $USER` so the user
unit survives logout.

The unit gets your `PATH` plus `~/.local/bin`, `~/.cargo/bin`,
`/opt/homebrew/bin` and `/usr/local/bin` where they exist, because service
managers start with a bare one and Claude installs into `~/.local/bin`. If
`claude` is not found, the error lists every directory searched. Re-run
`nerve connect` to refresh a unit written by an older version.

One agent works per device: whichever holds `agent.lock`. A TUI that finds
the lock held attaches as a viewer; if it takes the lock it runs the agent
itself, and the service takes over when that TUI quits. CLI commands work
either way. An agent that cannot reach its relays retries after 2 s,
doubling up to 2 minutes, and re-reads `fleet.toml` before each try.

`nerve connect --no-service` skips the service; run `nerve sync agent`
yourself.

### The nerve mod

`nerve connect` registers `<config>/sync/mod` as a local plugin
marketplace and installs `nerve@nerve-local` into Claude Code. New Claude
sessions load it; restart older ones. It talks to the agent over
`<config>/sync/agent.sock` (mode 0600) and submits remote prompts, offers
permission prompts and questions to other devices, and reports turn cost.
Use `--no-claude` to skip it.

## Relays

| setup | how | round trip |
|---|---|---|
| Public relays (default) | what `nerve connect` configures; no VPN needed | 0.5-0.9 s |
| Plus a private relay | on an always-on device run `nerve sync serve-relay`; on each device `nerve sync relay add ws://<host>:7777` | about 16 ms |

With a private relay connected, bulk data (hand-offs, diffs) goes only
there. It keeps events in memory and sweeps expired ones every ten
minutes. Public relays see which keys talk, when, and message sizes, never
content. Relays that worked for fresh keys: relay.primal.net,
relay.snort.social, relay.nostr.net (lossy). nos.lol and nostr.mom
acknowledged events but never delivered them.

Run `serve-relay` as its own service on the always-on device if you want it
to survive reboots; nerve does not install that unit.

## Configuration

Everything is under `<nerve config>/sync/` (`~/.config/nerve/sync/` by
default; `NERVE_CONFIG_DIR` relocates it). `fleet.toml` holds the device's
settings:

```toml
device = "laptop"
relays = ["ws://192.0.2.10:7777", "wss://relay.primal.net"]
caps = ["ios-sim"]                 # extra capabilities
handoff_root = "/Users/me/nerve-handoff"   # where unplaced hand-offs land; unset refuses them
remote_questions = "away"          # away | always | never
remote_permissions = true
auto_trust_handoffs = true         # accept Claude's folder-trust prompt for handed-off and started sessions
hook_timeout_secs = 600
private = ["~/code/secret"]        # folders whose sessions stay on this device
private_sessions = []
keep_versions_hours = 48
closed = ["pi"]                    # peers devices added later don't meet

[peers]
server = "npub1..."

[path_map]                         # sender prefix = local prefix
"/Users/me/code" = "/home/user/code"

[places]                           # name = absolute path on this device
proj = "/Users/me/code/proj"

[run]                              # commands peers may run here; empty runs nothing
allow = [{ place = "proj", argv = ["make", "*"] }]

[ssh]                              # peer = ssh host; big files go over rsync on it
server = "user@server.example"

[xfer]
bulk_over = 67108864               # bytes from which files skip the relay

[[folders]]                        # kept the same on several devices
name = "site"
with = ["studio"]
when = "turn"                      # turn | watch | manual
others = "ask"                     # ask | inside | any
```

Places: a name for a directory, defined per device, so commands never need
remote paths. A hand-off lands at `--to <place>`, else the `[path_map]`
mapping of the source path, else `<handoff_root>/<repo>`; with none of
these the target refuses and names the line to add. A `[run]` rule's
`argv` matches literally, a final `"*"` matches any further arguments, and
`shell = true` rules run through `sh -c`, so keep them literal.

Other files in `<config>/sync/`:

| file | what |
|---|---|
| `device.key` | This device's secret key (0600) |
| `agent.lock`, `agent.alive` | Which agent works for the device, and its heartbeat |
| `agent.sock` | The mod's connection to the agent (0600) |
| `agent.log` | Agent output under launchd or a detached start |
| `mod/` | The nerve mod as installed into Claude |
| `chunks/`, `known/` | Content-addressed chunks, and which each peer holds (pruned above 512 MiB) |
| `versions/` | Files syncs replaced or deleted, for undo |
| `folders/`, `approvals.json` | Synced-folder indexes; joins and folders awaiting a yes |
| `spool/`, `activity.log`, `local.json`, `seen`, `since`, `away`, `invites/`, `mailbox/` | CLI requests, agent activity, own state, replay protection, away flag, open codes, hook answers |

Notification choices are in `<nerve config>/notify.toml`.

## Upgrade

Use any install method: `install.sh`, `nerve update`, or
`cargo install nerve-tui --locked --force`. Each replaces the binary by
rename, never in place, because macOS kills a binary rewritten while it
runs. The agent checks its binary every 10 seconds; once it changes and no
transfer is mid-way, it checks `nerve --version` on the new binary and
restarts as it. On start it rewrites the mod's files if they differ, and
sessions load the new mod when they next start. A device on an older nerve
joins every folder shared with it without asking; `nerve devices` marks
it until it is updated.

Nerve checks crates.io once a day and mentions a newer version on the
bottom line. Turn that off with `check_on_launch = false` under `[updates]`
in `~/.config/nerve/config.toml`.

## Uninstall

There is no uninstall command. To remove sync and nerve from a device:

1. Tell your other devices: `nerve forget <this device>` from any of them.
2. Stop the service.
   - macOS: `launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/dev.nerve.sync.plist`, then delete the plist.
   - Linux: `systemctl --user disable --now nerve-sync.service`, then delete `~/.config/systemd/user/nerve-sync.service`.
   - Termux: stop `nerve sync agent` and delete `~/.termux/boot/nerve-sync`.
3. Remove the `nerve@nerve-local` plugin from Claude Code.
4. `cargo uninstall nerve-tui`.
5. Delete `~/.config/nerve/` (this removes the device key and kept versions).

## Troubleshooting

Start with `nerve sync doctor`, then `nerve sync status` (last synced,
sessions, pending prompts) and `nerve devices`.

- **A device shows offline (◌).** It has not reported for 3 minutes. Check
  its agent is running (service status, `agent.log`) and that its relays
  are reachable; a private relay must be reachable from both devices.
- **No relay delivers.** Add a second public relay or run a private one.
  The agent keeps retrying with backoff.
- **`claude` not found when starting or handing off.** The service's
  `PATH` is bare. Re-run `nerve connect`; make sure `claude` is in
  `~/.local/bin` or another searched directory.
- **A session cannot be typed into.** Without the mod it needs a tmux pane
  or Ghostty tab. Background (`claude --bg`) and desktop-app sessions are
  view-only without the mod. Restart sessions started before setup.
- **A hand-off is refused.** No place, `path_map` entry or `handoff_root`
  covers the target; the checkout has uncommitted changes, a different
  branch, or commits the hand-off lacks (`--force` overrides the last two).
- **A started session or folder is refused.** The folder must exist inside
  the home folder, a place or `handoff_root`, and not be private or one
  nerve never syncs (`~/.ssh`, `~/.gnupg`, `~/.aws`, Claude's and nerve's
  config). The refusal names the `[places]` line to add.
- **Large files do not move.** Files of `bulk_over` and up need an `[ssh]`
  route on one side and rsync installed.
- **Instructions are dropped as stale.** Requests older than 5 minutes are
  refused; check the clocks on both devices.
- **Questions never reach other devices.** Claude hides its question dialog
  until the settings hook returns, so without the mod they go remote only in
  away mode (`nerve sync away on`) or with `remote_questions = "always"`.
- **Two devices act on every command.** They share a `device.key`. Give one
  a new key.
