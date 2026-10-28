# nerve cross-device sync: research report

Prepared 2026-10-10 on branch `sync-research`.

**Question.** Can nerve keep Claude Code agents in sync across the user's own devices? In scope: progress, instructions, pending questions, diffs and subagents, handoff, per-device sync status, and capability-based job distribution. The setups to cover are with and without Tailscale, preferably over Nostr.

**Short answer: yes.** It is implemented and measured on a real Mac and a real Raspberry Pi 5.

- **Over public Nostr relays, with no VPN at all:** every feature works end to end. The cost is 0.5–1 s of latency per hop, and choosing relays matters.
- **With a private relay on the Pi over Tailscale:** the same protocol runs at ~16 ms.
- **The hard part was not the transport.** It was getting *into* a running interactive Claude Code session. That is solved with documented hooks, and its edge cases were verified empirically.

## Round 2 (2026-10-12)

Feedback on round 1 asked for four things: a simpler setup, smaller repeat syncs, a look at Claude Code mods, and a less prototype-looking UI. Delivered:

- **`nerve setup [code]`.** One command does everything on a device: identity, the mod, a service and pairing. It prints a one-time code (and QR) for the next device. Joining took 4–7 s (E17, E23).
- **The nerve mod.** It runs inside Claude Code ≥ 2.1.287:
  - prompts go in through `$.prompt.submit` (queued while Claude is busy; works for `--bg` sessions);
  - permissions and questions are answerable remotely while Claude's own dialog stays up, so away mode is no longer needed;
  - every turn is reported.

  Two API traps were found and handled: the 10 s hook budget, and `next.signal` not firing on a local answer (D15–D17, E18–E21).
- **Incremental hand-off.** Git sends only `--not <target tips>`; everything else goes as content-defined chunks the target doesn't already hold. A repeat hand-off of a ~700 KiB session moved 16–19 KiB (D18, E22). Pi → Mac now tested (E26).
- **The Devices view, rebuilt.** Two panes on laptops, a tab strip under 90 columns (phones over SSH), "needs you" first (D20).
- **Phones.** Termux can run nerve as a full device; a static aarch64 binary was built. iPhone stays SSH-only, because iOS can't run native Linux binaries.

The guided overview is `GUIDE.html`; measurements are under E17–E26 in `experiments/RESULTS.md`; decisions are D15–D22.

## 1. What was built (round 1)

- **`nerve --features sync`.** About 4,000 lines in `src/sync/`, `src/tui/devices.rs` and `src/app/sync_ui.rs`.
  - 39 new unit tests, including an end-to-end handoff test.
  - Clippy clean.
  - Builds on the MSRV, 1.85.
  - The default build is unchanged.
- **`research/sync/lab`.** A standalone measurement harness, `nsync-lab`. It covers Nostr ping/pong (plain and gift-wrapped), relay size probes, an embedded relay, and iroh direct versus relay paths with bulk transfer.
- **`research/sync/experiments/`.** Shell prototypes that came first, then raw outputs and `RESULTS.md`.
  - The shell prototypes are the mailbox hook, handoff pack and unpack, and relay survey scripts.
  - The raw outputs cover 15 experiments.
- **`reports/`.** Three web-research surveys with sources:
  - Nostr NIPs, crates, relays and prior art;
  - 48 alternative protocols and frameworks;
  - Claude Code's control surfaces and 20+ remote-control projects.

### Prototype versions, in order

| version | what it proved |
|---|---|
| v0: shell + `nak` + `jq` | Relay limits; hook mailbox; handoff by tarball (E1, E5–E7) |
| v1: `nsync-lab` (Rust, standalone) | Latency and loss across public relays, private relay, gift wrap, iroh direct and relay (E2–E4) |
| v2: `nerve sync` CLI + headless agent | Commands, answers, fetch, handoff, jobs between two real devices (E10–E13) |
| v3: TUI Devices view (`D`, `H`) | The same features driven interactively; fixed a real selection bug (D12) |
| v3 on public relays only | No Tailscale; offline store-and-forward (E14, E15) |

## 2. Requirements → status

| # | requirement | status | how |
|---|---|---|---|
| 1 | View progress on any device | ✅ | Per-device state event: sessions, state, waiting reason, branch, cost, subagent count, transcript tail. Republished on change and every 60 s |
| 2 | Send instructions to a running agent elsewhere | ✅ tmux, ✅ Ghostty (macOS), ✗ background/desktop sessions (no pane) | Bracketed paste plus Enter, after re-validating the PID; control characters stripped; 5-minute replay limit |
| 3 | View and answer pending questions and decisions | ✅ permissions always; ✅ AskUserQuestion in away mode | `nerve sync hook` (PermissionRequest + PreToolUse) ⇄ mailbox ⇄ agent ⇄ Nostr |
| 4 | Read changes, progress, subagent work | ✅ | `fetch diff\|log\|subagents`, chunked; the TUI shows the tail inline and the diff in a pager |
| 5 | Continue the work on another device | ✅ | `H` / `nerve sync handoff`: git bundle + patch + untracked + transcript → `claude --resume --fork-session` in tmux on the target; `--then` adds a follow-up instruction |
| 6 | Last synced per device; manual sync | ✅ | "last synced 4s ago (19:36:46Z)" per peer; ⇄ in the status bar; `r` / `nerve sync sync` |
| 7 | Nostr without WireGuard; compare with Tailscale | ✅ both measured | Table in §3 |
| 8 | TUI integration; web UI later | ✅ TUI; web is planned (§6) | Protocol is plain JSON in NIP-44, so a nostr-tools PWA can reuse it as-is |
| 9 | Distribute and verify work by device capability | ✅ | `nerve sync job --needs … --verify …`; auto-detected caps (OS, arch, tools, xcode/ios-sim); least-busy routing; offline queueing |

## 3. Key measurements

From `experiments/RESULTS.md`. All RTTs are single-clock round trips Mac→Pi→Mac.

| transport | p50 RTT | bulk throughput | needs |
|---|---|---|---|
| Private Nostr relay (embedded in nerve) over Tailscale | **15.8 ms** | 730 KiB handoff in 0.6 s | Tailscale (or any reachable host) |
| iroh, direct LAN path, auto-discovered | 8.5 ms | 14.7 MiB/s | nothing (n0 relays for discovery); MSRV 1.91 |
| scp over Tailscale (baseline) | — | 13.8 MiB/s | Tailscale + ssh |
| Public Nostr, relay.snort.social | 538 ms | — | nothing |
| Public Nostr, relay.primal.net | 640 ms (gift-wrapped: 704 ms) | 32 KB events in 1.7 s; 730 KiB in 20.7 s (paced) | nothing |
| Public Nostr, three relays, first copy wins | 721 ms | — | nothing |
| iroh relay-only path (n0 relay) | 197 ms | 1.1 MiB/s | nothing |

### Findings that shaped the design

1. **Relays lie about delivery.** nos.lol and nostr.mom said OK to every event from a new key, then neither delivered nor stored any of them. Damus and nostr.mom rate-limited or banned test keys within minutes. That led to end-to-end acks, several relays per device, and a list of verified relays (D6).
2. **Size limits are lower than advertised.** The smallest accepted size was 60 KB, and some relays accepted only 8 KB from unknown keys. Hence 32 KiB chunks, and a bulk lane that uses only private relays (D7).
3. **Claude's two prompt kinds behave differently under hooks.**
   - A `PermissionRequest` hook waits while the dialog is shown, so local and remote answers race safely.
   - A `PreToolUse` hook on `AskUserQuestion` hides the dialog until it returns. That is why "away mode" exists (D2).
   - Neither hook is told about a local answer, so the transcript is used to detect one (D3).
4. **Handoff needs only the cwd slug to match.** Absolute paths inside the transcript don't matter. `--fork-session` keeps the two devices' histories apart (D8).
5. **macOS Local Network Privacy blocks ad-hoc-signed binaries from reaching LAN IPs.** Tailscale addresses are unaffected (D14).

## 4. Architecture as built

```
 Claude session ──PermissionRequest/PreToolUse hook──► nerve sync hook ─┐
 (tmux pane / Ghostty tab)                                               │ mailbox/
       ▲ bracketed paste                                                 ▼
       └──────────────── inject ◄──── sync agent (thread, tokio) ──── handoff / jobs
                                       │  discovery (same code as the TUI)
                                       │  NIP-44 per-peer envelopes
                     ┌─────────────────┴───────────────────┐
              ws://pi:7777 (private,             wss://primal, wss://snort
              Tailscale, bulk lane)              (public, no VPN)
                     └─────────────────┬───────────────────┘
                                       ▼
                         other devices' agents / TUI `D` view
```

Event classes:
- **kind 30078:** addressable per-recipient state.
- **kind 4590:** stored commands, answers, blobs and jobs, with a 24 h NIP-40 expiry.

Envelope bodies are `State`, `Send`, `Answer`, `SyncNow`, `Fetch`, `Blob`, `HandoffOffer`, `JobSubmit`, `JobUpdate` and `Ack`. Details are in `docs/SYNC.md` and `src/sync/protocol.rs`.

The design follows nerve's existing rules:
- All I/O stays in the agent thread; the UI reads a snapshot.
- Every network string is `Sanitised` at render time.
- Claude data is located through `claude_roots()`.
- The PID is re-validated as `claude` before typing.
- New items are `pub(crate)` only.
- macOS-only code (Ghostty) is behind `cfg`.

## 5. Landscape and why not X

The full comparison of 48 options is in `reports/02-alternatives-survey.md`.

- **Claude Code Remote Control** is first-party and excellent for phone chat and approvals. It has no handoff, no fleet view, no job routing and no public API, and it requires an Anthropic login. It complements nerve and doesn't replace it.
- **Happy, Paseo and YepAnywhere** are mature remote UIs. They *own* the Claude process through the Agent SDK. nerve monitors sessions it didn't start, which is why the hook and paste approach was chosen.
- **CodeDeck+** (Nostr + NIP-44, Android) is the closest prior art, and its kind split matches ours. **ContextVM** offers MCP over Nostr, and could later expose nerve as an MCP tool that one agent uses to drive another device.
- **iroh** was the best raw performer, but it needs MSRV 1.91 and has no offline delivery. It is the planned bulk lane.
- **NATS and MQTT** need a reachable hub and have no end-to-end encryption.
- **Matrix** gives a free phone UI, but needs MSRV 1.96 and a heavy SDK.
- **Tailscale + HTTP** is simplest, but fails the "no WireGuard" goal.

## 6. Limits and next steps

**Not done, with reasons:**
- Phone and web client: §8 of the brief marks it as later.
- An iroh bulk lane: blocked by MSRV.
- NIP-59 by default and NIP-42 relay auth: they work, but add size and setup cost for a single-owner fleet.
- Key rotation and revocation, beyond editing `[peers]`.
- Injecting into `claude --bg` sessions: there is no pane, and no CLI to send input.

**Untested here:**
- Two different networks with the Pi behind a different NAT. Both devices sat on the same home LAN. The "no Tailscale" runs went through public relays on the internet, which is the same path a remote laptop would take. iroh's relay-only mode was forced to simulate a NAT barrier.
- Xcode or the iOS simulator: this Mac has Command Line Tools only. Routing was shown with the `macos`/`linux` capabilities instead; adding `ios-sim` is detection only.

**Suggested next steps:**
1. A systemd unit and launchd plist for `nerve sync agent`.
2. A PWA that uses nostr-tools plus `protocol.rs`'s JSON. Push via ntfy or UnifiedPush from the Pi.
3. Let agents drive other devices: expose `nerve sync job/send/status` as an MCP server, either over stdio or over ContextVM. Then a Claude on the Mac can say "verify this on the Pi" through nerve, which covers requirement 9's "tell the agents to use the other device" option.
4. Show the Remote Control `bridgeSessionId` in details, and offer "open in Claude app".

## 7. How to reproduce

**Start here:** `research/sync/GUIDE.html` covers setup, how each feature works and what it sends, the mod, incremental hand-off, what was tested and what wasn't, and step-by-step testing on Mac, Pi and phones.

**Browse everything offline:** open `research/sync/site/index.html`. It holds this report, the decision log, results, surveys, raw outputs and prototype source in one file, and is rebuilt with `research/sync/site/build.sh`.

**See it run on one Mac:** `research/sync/demo/demo.sh` starts a private relay and a second "device", then opens the TUI (press `D`). See `demo/README.md`. `experiments/e17_mod_e2e.sh` runs the full mod flow with a real Claude session.

```bash
# measurements
cd research/sync/lab && cargo build --release        # Pi build: see RESULTS.md (Docker linux/arm64)
nsync-lab keys mac; nsync-lab ping --key mac --peer <hex> --relays wss://relay.primal.net
# integrated prototype
cargo build --release --features sync
nerve sync init --name mac --relay wss://relay.primal.net --relay wss://relay.snort.social
nerve sync pair pi npub1...   # and the reverse on the Pi
nerve sync install-hooks      # then start Claude with --settings <printed path>
nerve                          # press D
```
