# Decision log: nerve cross-device sync

Each entry gives the decision, the alternatives considered, and the evidence. "E*n*" refers to `experiments/RESULTS.md` and the experiment table in `REPORT.md`.

---

**D1. Build on Claude Code's own control surfaces, not on keystroke scraping alone.**

- *Decision:* Answer permissions with a `PermissionRequest` hook. Answer questions with a `PreToolUse` hook on `AskUserQuestion` that returns `updatedInput.answers`. Use tmux/Ghostty paste only to type new instructions.
- *Alternatives:*
  - tmux send-keys for everything. This is fragile: you have to scrape the screen to find the dialog.
  - Channels (`--dangerously-load-development-channels`). This needs a flag plus an interactive confirmation every launch, and it is a research preview.
  - The cross-session socket. It is undocumented, and Claude labels its messages as "not from the user".
  - Remote Control. There is no public API, it is Anthropic-only, and it needs a claude.ai login.
- *Evidence:* E6a and E6b both work on 2.1.288. The exact output shapes are recorded and unit-tested (`mailbox::tests`).

**D2. Questions go remote only in "away mode"; permissions go remote always.**

- *Why:* E6b showed that the question dialog is **not shown** while a `PreToolUse` hook is waiting. If questions always went remote, the person at the keyboard would never see them. E6a showed that the permission dialog **is shown** while `PermissionRequest` waits, so a local and a remote answer simply race, and that is safe.
- *Setting:* `remote_questions = away | always | never`. Default: `away`.

**D3. Detect a local answer from the transcript, not from the hook's stdin.**

- *Why:* A blocked hook is never told that the user answered at the keyboard (E6c), and the hook input carries no `tool_use_id`. So the hook matches the newest `tool_use` with the same name and input, then watches for its `tool_result` (E6d: the transcript grows about 10 KB the moment a local answer lands). Once it sees that, it withdraws its request, so other devices don't keep showing a question that is already resolved.

**D4. Transport: Nostr via nostr-sdk 0.45, inside nerve, behind a cargo feature.**

- *Alternatives:* 48 options surveyed (`reports/02-alternatives-survey.md`), including iroh, NATS, MQTT, Zenoh, Matrix, libp2p, Syncthing, Tailscale-plus-HTTP, and CRDT stacks.
- *Why Nostr:*
  - **No inbound connectivity.** Both ends dial out over WSS, so it works without Tailscale (E14).
  - **Store-and-forward for free.** A job sent to an offline Pi ran about 1 s after the Pi reconnected (E15).
  - **The relay data model matches the problem.** Addressable events give "last known state" and NIP-40 gives TTLs.
  - **Self-hostable in about 20 lines.** `nostr-sdk` with `local-relay` gave 16 ms RTT over Tailscale (E3).
  - **Fits nerve's MSRV.** nostr-sdk's MSRV is **1.85, the same as nerve's**.
  - **Phones can join later.** Existing Nostr apps and the `nostr-tools` JS library give a path to a phone client.
- *Why not iroh as the main transport:* iroh measured best (8.5 ms direct, 15 MiB/s, and it found the LAN path on its own, E4). But it needs MSRV 1.91, has no store-and-forward, and its relay-only path ran at 1.1 MiB/s. It is a good future bulk lane (see D10).
- *Why not NATS or MQTT:* they need a hub reachable from every device, so without Tailscale the Pi must be exposed. They also have no end-to-end encryption and add a Go service to run.
- *Why a cargo feature:* the default build stays dependency-light, and users who don't sync pay nothing.

**D5. One envelope format; three event classes.**

- Every payload is a JSON `Envelope { v, id, from, sent_at, body }`, NIP-44 v2 encrypted to exactly one device.
- State is kind 30078 (addressable, `d = nerve/1/state/<recipient>`). Everything else is kind 4590 (stored), with a NIP-40 expiry of 24 h.
- *Why not ephemeral kinds (2xxxx):* they give no catch-up for a sleeping device, and E2 showed public relays shadow-dropping them.
- *Why not NIP-59 gift wrap by default:* it works (E2: same latency on primal), but it triples the event size and gains little when the only peers are your own devices. Kept as an option.
- *Why not NIP-90 DVM kinds for jobs:* NIP-90 is now marked "unrecommended" upstream. We borrow only its request → feedback → result shape.

**D6. End-to-end acks; a relay's `OK` is not delivery.**

- *Evidence:* E2. nos.lol and nostr.mom returned `OK true` for every event from a new key, delivered nothing, and stored nothing.
- *Decision:*
  - Every command gets an `Ack` envelope back from the device.
  - The CLI prints `✓` or `✗` only from that ack.
  - Docs list relays that were verified to deliver.

**D7. Chunking at 32 KiB of plaintext; bulk only via private relays.**

- *Evidence:* E1. The smallest accepted size was 60 KB (strfry's 64 KiB default), nostr-sdk's LocalRelay default is 64 KiB, and offchain.pub allowed 8 KB for unknown keys.
- *Evidence:* E12. A 730 KiB handoff took 20.7 s over a public relay (paced to avoid bans; damus banned a test key within minutes, E2) and **0.6 s** via the private relay.
- *Decision:* `send_bulk` targets only connected `ws://` relays when any exist, falling back to all relays.

**D8. Handoff = git bundle + worktree patch + untracked tar + transcript, installed under the target's slug, then `--resume --fork-session`.**

- *Evidence:*
  - E7 (scripts) and E12 (nerve): the session recalled context given on the other machine and edited the carried-over uncommitted file.
  - The cwd slug is the **only** path that must match. Absolute source paths inside the transcript did not confuse later turns, so path rewriting was not needed.
- *Why `--fork-session`:* the two devices never append to one transcript.
- *Why the resumed session runs in detached tmux:* it then shows up in nerve, can receive typed instructions, and outlives the agent.
- *Safety:* refuse to unpack over a dirty checkout.

**D9. Replay protection on two layers.**

- *Evidence:* during E14 the seen-set was wiped, and a fresh agent replayed stored commands, including typing an old instruction.
- *Decision:*
  - The persisted seen-set covers every message type.
  - A hard 5-minute age limit applies to `Send`, because typing is not idempotent.
  - Answers are naturally idempotent: an unknown or answered request returns "already resolved".

**D10. Deferred: iroh bulk lane, phone/web client, NIP-59 by default, NIP-42 relay auth.**

- iroh becomes attractive once nerve's MSRV reaches 1.91. It could also run as a separate `nerve-bulk` sidecar today.
- A web or PWA client can reuse `protocol.rs` one-to-one: it is plain JSON in NIP-44.
- Remote Control and Nostr are complementary. Use Remote Control for zero-setup phone chat. Use nerve for fleet view, handoff and jobs, none of which Remote Control offers.

**D11. One-shot CLI commands never act on peer commands.**

- *Why:* `nerve sync send` starts a short-lived agent. If that agent consumed a `Send` addressed to this device, it would exit mid-task and the long-lived agent would never see it.
- *Decision:*
  - Short-lived agents only read.
  - When a long-lived agent is running (pid heartbeat), the CLI queues requests through `spool/` instead of starting a second agent.

**D12. The Devices view keys its selection by row identity, not by index.**

- *Evidence:* during TUI testing, a peer republished its state with sessions in a different order, and `m` sent an instruction to the *wrong session*.
- *Decision:*
  - Selection is keyed by row identity.
  - Agents publish sessions in a stable order (by cwd, then id).
  - A regression test covers this (`selection_follows_the_row_not_the_index`).

**D13. Keep MSRV 1.85 without pinning by hand.**

- The new transitive deps (icu 2.3 via `url`) want rustc 1.88. The lockfile was resolved with `CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback`.
- Zero pre-existing dependency versions changed.
- Verified: `cargo build --locked --all-targets` passes on rust:1.85 both with and without `--features sync`.

**D14. Environment facts, recorded rather than worked around in code.**

- **macOS Local Network Privacy.** It blocked our ad-hoc-signed binaries (and Go's `nak`) from dialling the Pi's **LAN** IP (`EHOSTUNREACH`), while Apple-signed curl and nc worked. Tailscale addresses were unaffected. Docs recommend tailnet or public relays from macOS.
- **Setting `CLAUDE_CONFIG_DIR=~/.claude` explicitly makes Claude rerun onboarding**, because it then looks for `~/.claude/.claude.json`. So the env var is set only for relocated roots.
- **The tmux server, not the caller, runs `new-session`'s command.** PATH and config therefore travel with `-e`.

---

## Round 2 (2026-10-12): mods, incremental sync, one-command setup

**D15. Prefer a Claude Code mod over tmux paste and settings hooks, and keep both as fallbacks.**

- *Decision:* `nerve setup` installs a mod (`mod/nerve`, embedded in the binary) on Claude Code ≥ 2.1.287. Each session's mod connects to the device's agent over a Unix socket and:
  - submits prompts with `$.prompt.submit`;
  - answers permissions from `classic.PermissionRequest`;
  - answers questions by racing Claude's own `AskUserQuestion` dialog;
  - reports `turn.start` and `turn.complete`.
- *Why:*
  - **Prompts.** A typed paste lands in whatever the TUI is showing. If a dialog is open, it can answer the dialog. `prompt.submit` is queued behind a running turn instead (E18b), and it reaches `claude --bg` sessions, which have no terminal at all.
  - **Questions.** The settings `PreToolUse` hook hides the question dialog while it waits (D2), so questions needed away mode. The mod leaves the dialog up and races it (E20), so away mode is no longer needed for sessions with the mod.
  - **Signals.** The mod knows when a turn starts and ends, and what it cost, rather than inferring that from the transcript.
- *Fallbacks:* sessions without the mod (older Claude, a session started before setup, or `--bare`) still get tmux/Ghostty paste and settings hooks, unchanged. The Devices view marks each session `mod`, `tty` or `view`.
- *Costs:*
  - Mods are not sandboxed. The nerve mod can approve tool calls, and that is the point of it. It talks only to a 0600 socket owned by the same user.
  - Mods load only in trusted folders, and the first load of a `--plugin-dir` mod asks for approval. An installed plugin (`claude plugin install`) loaded without a prompt in a trusted folder (probed).
  - Claude Code 2.1.287 or newer is required.

**D16. Mod hooks wait inside `$.http.fetch`, never `$.clock.sleep`.**

- *Evidence:* E21 run 1. A hook has 10 s of its own time, a sleep loop spends it, and after 10 s the hook was abandoned with the request still listed. An in-flight `$` call pauses the clock (`HookBudget` docs; E19 now holds 20 s).
- *Decision:* the agent offers `/v1/wait`, a 2 s long-poll per request.

**D17. Detect a local answer by watching the tool call settle, not by `next.signal`.**

- *Evidence:* the probe logged `next.signal.aborted == false` for 14 s after the user pressed "Yes".
- *Decision:*
  - The mod wraps every `tool.call`. When a call with the same tool and input settles after the request was raised, the request is withdrawn.
  - The agent drops any request whose hook stops polling for 6 s.

**D18. Hand-off is incremental: negotiate git, content-address everything else.**

- *Problem raised:* sending the whole repo history and the whole transcript on every hand-off.
- *Decision:*
  - **Git.** The target reports its checkout's tips, and the source bundles `--all --not <tips>`. The repo is identified by its root commit, so an unrelated checkout at the same path is never treated as "has these commits".
  - **Files.** The patch, untracked files, transcript and sidecars are cut with FastCDC-style content-defined chunking (gear hash, 2/8/32 KiB). Each chunk is stored by SHA-256 in `<sync>/chunks`. Per-peer `known/` lists record what each peer has acknowledged.
  - **Index.** The list of files and chunk ids travels as a chunked file too, so a big repo's index can't overflow a message.
- *Why content-defined rather than fixed-size or rsync-style:*
  - Transcripts are append-only JSONL, but an edit can land anywhere in a patch or untracked file.
  - With CDC both cost only the chunks around the change, and nothing needs a round trip to compute signatures first.
- *Result:* second hand-off 16–19 KiB against 694 KiB (E22). Chunks are verified against their id on arrival, and a pruned or missing chunk is re-requested.
- *Not done:* compressing chunks. Transcripts compress about 5×, but NIP-44 output is base64 anyway, and the bulk lane is the private relay. Worth adding if public-relay hand-offs of big repos matter.

**D19. One command per device: `nerve setup [code]`.**

- *Decision:*
  - **Without a code:** create an identity on the two verified public relays, install the mod (or hooks on older Claude), install the agent as a service, and print a one-time invite code plus a QR.
  - **With a code:** do the same, then pair.
- *Services:*
  - launchd LaunchAgent on macOS.
  - systemd user unit on Linux, with `loginctl enable-linger` so a headless Pi keeps it after logout.
  - On Termux or containers: a detached process, plus a `~/.termux/boot` script and `termux-wake-lock`.
- *Pairing:*
  - The invite is base64url JSON: name, pubkey, a 128-bit one-time secret and the relays.
  - The joiner trusts the inviter and sends `PairRequest{secret}`.
  - The inviter's agent accepts that one message type from unknown keys, and only while an invite is open: it subscribes to strangers only then.
  - The secret is consumed (deleted before checking, so two racing joiners can't both win). The inviter pairs back and acks.
- *Security:*
  - The code is a credential for an hour. Anyone with it can pair a device that can then type into your sessions. It is printed only on the inviting device.
  - A wrong or used code gets a refusal ack, which reveals nothing the code didn't already contain.

**D20. The Devices view is a two-pane screen that collapses for phones.**

- *Layout:*
  - **Wide** (≥ 90 columns): a device list on the left (freshness dot, waiting count, sessions, hand-off progress bars) and the selected device on the right.
  - **Narrow:** the list becomes a tab strip, so the same keys work in a phone's SSH app.
- *Content:*
  - "Needs you" (permissions, questions with numbered options) always comes first; sessions are ordered waiting → working → idle.
  - The selected session shows its path, last turn (time, tokens) and transcript tail.
  - The footer shows the last event, so acks are visible without a log pane.
- `Tab`/`←→` switch device. The title shows "N needs you".

**D21. CLI commands follow the running agent's activity log.**

- *Evidence:* E23 run 1. With a service-managed agent running, every `nerve sync send/answer` printed only "queued". The ack happened, but the CLI couldn't see it.
- *Decision:* the long-lived agent appends its activity to `<sync>/activity.log`, capped at 1 MiB. A CLI command that hands work to it follows that file until the ack, as the one-shot path always did.

**D22. Test scripts must background nerve itself, not a shell function.**

- *Evidence:* `alpha sync agent &` backgrounds a subshell, so `kill $!` left the agent running. Ten orphaned agents from earlier demo runs, some two days old, were found and stopped.
- *Decision:* `demo.sh` and the experiment scripts start agents through a `bg` helper that runs the binary directly. Every script now ends with "no leftover agents".
