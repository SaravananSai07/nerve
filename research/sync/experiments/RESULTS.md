# Experiment results (raw numbers live in the `*.out` files next to this)

Setup, 2026-10-09/10:
- **Mac:** M-series, macOS 27.0.1, Claude Code 2.1.295, LAN address 192.168.1.18, Tailscale address 100.100.98.51.
- **Pi:** Raspberry Pi 5 (8 GB), Debian 13, Claude Code 2.1.288 logged in with a claude.ai Max account, LAN address 192.168.1.17, Tailscale address 100.73.26.46. Same home LAN as the Mac.
- **Harness:** `research/sync/lab` (`nsync-lab`), built natively and for linux/arm64 in Docker.
- **How RTT is measured:** every RTT is a ping→pong→ping round trip measured on one clock. Clock skew between the two machines therefore can't distort the numbers.

## E1: public relay limits (`e1_*.out`)

NIP-11 advertised limits and the probed limits disagree. What relays actually accept, measured by publishing NIP-40-expiring events of increasing size:

| relay | largest accepted | note |
|---|---|---|
| relay.damus.io | 128 KB, then rate-limited | "you are noting too much" after ~10 large events |
| nos.lol, nostr.mom | 60 KB (rejects 64 KB) | strfry default `maxEventSize` 64 KiB |
| relay.primal.net | 256 KB (rejects 500 KB) | |
| relay.nostr.net | 200 KB | |
| relay.snort.social | 500 KB | memlay, idle timeout 10 s |
| offchain.pub | 8 KB from an unknown key | "pubkey is not in our web of trust" |

**Decision:** cap every event at 32 KiB of plaintext and chunk anything bigger. That fits under every relay we tested, including strfry and nostr-sdk's own embedded relay, whose default is 64 KiB.

## E2: Nostr Mac→Pi→Mac over public relays, NIP-44 encrypted (`e2_*.out`)

| path | payload | RTT p50 | RTT p95 | loss |
|---|---|---|---|---|
| relay.primal.net | 512 B | 640 ms | 813 ms | 0% |
| relay.nostr.net | 512 B | 909 ms | 1343 ms | 0% |
| primal + nos.lol + nostr.mom (first copy wins) | 512 B | 721 ms | 947 ms | 0% |
| primal, NIP-59 gift wrap (kind 1059) | 512 B | 704 ms | 1021 ms | 0% |
| primal | 8 KB | 1302 ms | 1374 ms | 0% |
| primal | 32 KB | 1732 ms | 5284 ms | 0% |
| primal | 60 KB | 2466 ms | 5372 ms | 0% |
| **nos.lol, nostr.mom** | 512 B | — | — | **100%** |

Publish acknowledgements (relay `OK`) arrive in 200–430 ms.

**Finding: silent drops.** nos.lol and nostr.mom answer `OK true` but never deliver or store events from a fresh, reputation-less pubkey. `nak req` afterwards finds nothing, for ephemeral 21059/25501 and for regular 4501 alike. These relays shadow-drop spam. **A relay's OK is not proof of delivery.** The protocol needs end-to-end acks, and a fleet must not depend on a single public relay.

**Finding: rate limits are real.** Within minutes of starting tests, damus banned the test key ("banned: rate-limit"), and nostr.mom rate-limited it ("slow down matey").

## E3: self-hosted relay on the Pi (nostr-sdk `LocalRelay`, embedded in our own binary)

| path | payload | RTT p50 | RTT p95 | loss |
|---|---|---|---|---|
| ws://100.73.26.46:7777, over Tailscale | 512 B | 15.8 ms | 27.5 ms | 0% |
| same, NIP-59 gift wrap | 512 B | 15.0 ms | 18.6 ms | 0% |
| same | 60 KB | rejected | | `max_event_size` 65536 (configurable) |

That is 40–45× faster than public relays, with no third party and no rate limits. The relay is about 30 lines of code on top of nostr-sdk's `local-relay` feature.

**Finding: macOS Local Network Privacy.** Our ad-hoc-signed Rust binary, and Go's `nak`, got `EHOSTUNREACH` ("no route to host") when dialling the Pi's **LAN** IP over TCP. `curl` and `nc`, which are Apple-signed, reached it fine. The Tailscale `utun` address was unaffected. This matches macOS's per-app Local Network permission. In practice, on macOS, dial peers by Tailscale address or by a public relay, or sign the binary and grant the permission.

## E4: iroh 1.3 (QUIC, dial-by-key, n0 public relays) (`e4_iroh.out`)

| path | RTT p50 | RTT p95 | bulk |
|---|---|---|---|
| direct path (iroh discovered and selected the LAN address 192.168.1.17 on its own) | 8.5 ms | 9.3 ms | 64 MiB at 14.7 MiB/s |
| relay-only (`clear_ip_transports`), via aps1-1.relay.n0.iroh.link | 197 ms | 231 ms | 16 MiB at 1.1 MiB/s |
| baseline: scp over Tailscale | — | — | 64 MiB at 13.8 MiB/s |

- Connection setup including key lookup took 744 ms. Becoming "online" (relay plus address discovery) took 3.3 s on the Pi.
- iroh needed no configuration, no ports and no Tailscale, and it upgraded to the LAN path automatically.
- The catch is that iroh 1.3 requires **MSRV 1.91**, but nerve pins 1.85. It also has no store-and-forward, so a sleeping laptop misses messages.
- So iroh fits a separate "bulk lane" binary or a future MSRV bump, not the control plane.

## E5–E6: controlling a live interactive Claude Code session on the Pi (tmux)

- **E5, inject an instruction.** `tmux send-keys -l "<text>"` + `Enter` into a fullscreen-TUI Claude session works. Multi-line text works with `tmux load-buffer` + `paste-buffer -p` (bracketed paste), followed by `Enter`. Claude replied `alpha-beta-gamma` to a 4-line paste.
- **E6a, answer a permission prompt remotely.** A `PermissionRequest` hook (our `mailbox-hook.sh`) parks the request in a mailbox and blocks. Writing `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}` into the response slot approved it. The transcript shows "Allowed by PermissionRequest hook".
  - **The local dialog is displayed while the hook waits.** Local and remote answers therefore *race*: whichever comes first wins.
- **E6b, answer AskUserQuestion remotely.** A `PreToolUse` hook matched on `AskUserQuestion` returns `permissionDecision: allow` + `updatedInput: {questions, answers: {"<question>": "Bash"}}`. Claude logged "User answered Claude's questions: → Bash" and wrote `greet.sh`.
  - **Unlike PermissionRequest, the question dialog is NOT shown until the hook returns.** Remote-answering questions must therefore be opt-in ("away mode"). Otherwise the person at the keyboard can't answer.
- **E6c/E6d, the local answer wins the race.** After a local "1" keypress the stale `PermissionRequest` hook keeps blocking until its own timeout. That is harmless: its late output is ignored. Its pending request must be withdrawn, though, or remote devices show a stale question.
  - The hook input carries `transcript_path`, `tool_name` and `tool_input`, but **no tool_use_id**. The transcript grows by about 10 KB the moment the local answer lands.
  - **Design:** the hook finds the matching `tool_use` in the transcript tail. Once a `tool_result` with that id appears, the request was answered locally, so the hook withdraws the request and exits.
- **Notification hook.** It fired with `notification_type: permission_prompt` about 6 s after the prompt appeared. That is enough for "needs you" alerts, but too slow and too lossy to drive answering.

## E7: hand a session over Mac → Pi (`claude/handoff-*.sh`)

The pack is a git bundle of all refs, a `git diff --binary HEAD` working-tree patch, a tar of untracked files, and the transcript plus its sidecar directory: 779 KB, mostly the transcript.

Steps that worked:
1. On the Mac, `claude -p` created a session that was told a codeword and made an *uncommitted* edit.
2. The pack was copied to the Pi and unpacked to a **different absolute path** (`/home/sspi/nerve-lab/ho-plain`). The transcript was installed under the slug for *that* path, without rewriting any paths inside it.
3. On the Pi, `claude --resume <id> --fork-session` in that directory recalled `PINEAPPLE-42` and correctly edited `calc.py` in the *new* path, which already carried the Mac's uncommitted change.

**Finding:** the cwd slug is the only path that has to match. Old absolute paths inside the transcript did not confuse the next turn. Path rewriting is offered as an option but is not needed.

## E8: headless worker shape (`claude -p --output-format stream-json --verbose --include-hook-events`)

- A 8-turn job (write a test, run it) produced 29 events: `system/init`, `assistant` with `tool_use` blocks, `user` with `tool_result`, `rate_limit_event`, and a final `result`.
- The `result` event carries `num_turns`, `duration_ms`, `total_cost_usd` and `session_id`.
- The job took 11.3 s and cost $0.005 with Haiku.

This is the job-worker contract: stream the progress, keep the `session_id` (so it can be resumed or handed off later), and report the result.

## E9: Ghostty injection on the Mac (AppleScript)

Ghostty 1.3.1's scripting dictionary has `input text <s> to terminal id <id>` ("as if it was pasted") and `send key "enter"`.

The test opened a throwaway window running `cat > file` and injected `line one\nline two; with $(dollar) and "quotes"`. Both lines arrived verbatim, with no shell expansion, and submitted correctly.

nerve already resolves the Ghostty terminal id for every terminal session (`platform::ghostty`). Remote instructions therefore reach Ghostty tabs as well as tmux panes.

## E10–E15: the integrated prototype, `nerve --features sync`, Mac ↔ Pi

These runs use the real nerve binary on both machines (the Pi build is cross-compiled for linux/arm64). Each device has its own config dir and Nostr identity. "Private relay" means `nerve sync serve-relay` on the Pi.

| # | what | path | result |
|---|---|---|---|
| E10a | `nerve sync send pi <session> "<text>"` | private relay over Tailscale, plus primal | ✓ typed into tmux pane %3; ack back on the Mac. ~3 s per CLI invocation, mostly the one-shot client connecting |
| E10b | pending `Write` permission visible on the Mac | same | ✓ `nerve sync status` lists `? <id> [permission] Write: …/hello.py` within one publish cycle (~4 s) |
| E10c | `nerve sync answer pi <id> allow` | same | ✓ ack "answered"; the file appeared on the Pi ~3 s later. The transcript says "Allowed by PermissionRequest hook" |
| E10d | `nerve sync fetch pi <session> diff` | same | ✓ git status and diff of the Pi checkout printed on the Mac |
| E11 | AskUserQuestion answered from the Mac (Pi in away mode) | same | ✓ question with options [Namaste, Hola] seen on the Mac; answered "Namaste"; the following Write permission was also answered remotely; `greet.txt` = Namaste |
| E12 | `nerve sync handoff <mac session> pi --then "<question>"` | same | ✓ 730 KiB pack in **0.6 s** over the private relay (20.7 s when sent through a public relay); resumed in tmux `nerve-cc6fb097` at `/home/sspi/nerve-handoff/demo2`; answered "The secret project codename is BLUE-HERON" (set on the Mac) and added `mul()` on top of the Mac's uncommitted `sub()` |
| E13 | three jobs: `--needs linux`, `--needs macos`, `--needs gpu,cuda` | same | ✓ linux job routed to the Pi, ran, verify passed (`fizzbuzz.py` line 15 == FizzBuzz), 3 turns, $0.003. ✓ macos job ran on the Mac, verified. ✓ gpu job refused: "no device has [gpu, cuda]" |
| TUI | `D` → select Pi session → `m` → type → Enter → request appears → `a` | same | ✓ `tui.txt` written on the Pi with the exact text; ack shown in the Devices log. Found and fixed bug D12: a reorder sent the instruction to the wrong session |
| E14 | full loop with **no Tailscale**: relays primal + snort only | public internet | ✓ send → prompt visible → answer → file written. Answer-to-file took 1.1 s |
| E15 | job sent while the Pi agent was **offline**; agent started 25 s later | public relays only | ✓ job delivered from relay storage 1 s after the agent reconnected, ran, verified |

Public relay delivery for a fresh key, measured with stored kind 4590 (`e14_public_relay_delivery.out`):

| relay | delivery | p50 RTT |
|---|---|---|
| relay.snort.social | 6/6 | 538 ms |
| relay.primal.net | 20/20 (E2) | 640 ms |
| nostr-pub.wellorder.net | 4/6 | 648 ms |
| relay.nostr.net | 2/6 | — |
| nostr.oxtr.dev | 6/6 | 10.5 s |
| relay.0xchat.com | not connectable | — |
| nostr.bitcoiner.social | rejected ("not in web of trust") | — |

## E16: an agent delegates verified work to another device through nerve

A headless Claude (Haiku) on the Mac was told only how to use `nerve sync status` and `nerve sync job`. It was asked for proof that python3 on an aarch64 Linux machine reports its platform, and was told not to run anything locally.

1. **First run.** The job ran on the Pi but failed: headless jobs run with `--permission-prompts none`, so `Bash(python3 …)` was denied. The agent reported this honestly and did not work around it.
2. **Fix.** Added `--allow <rule>` (→ `--allowedTools`) to jobs.
3. **Second run.** It failed with 0 turns. Cause: `--allowedTools` is variadic and swallowed the positional prompt. Fix: the prompt now goes in on stdin.
4. **Third run (plat-proof-2).** ✓ The agent chose `--needs linux,aarch64 --allow 'Bash(python3 *)' --verify 'grep -q aarch64 plat.txt'`. nerve routed the job to the Pi, which ran it in 2 turns. Verify passed, and `plat.txt` on the Pi = `aarch64`. The agent took 10 turns and $0.08, including polling.

This covers requirement 9's "tell the agents to use the other device": the sync and routing go through nerve, and the agent needs nothing more than the CLI.

## E17–E22: Claude Code mods, one-command setup, incremental hand-off (2026-10-12)

Script: `e17_mod_e2e.sh`, output `e17_mod_e2e.out`. Two devices (alpha, beta) on one Mac, each with its own `NERVE_CONFIG_DIR`, sharing a private relay on localhost. A real interactive Claude Code 2.1.295 session (Haiku) runs on "beta" and loads the nerve mod with `--plugin-dir`.

### What a mod can do, probed first

A mod is a plugin whose JavaScript runs **inside** the Claude process (Claude Code ≥ 2.1.287). The full API is in the type declarations Claude writes into a mod's `.claude-plugin/types/` the first time it loads it (16,000 lines). The calls that matter for sync:

| need | mod API | what the probe showed |
|---|---|---|
| send a prompt to a running session | `$.prompt.submit({text, asUser: true})` | Starts a turn when idle. While a turn runs it is **queued**, and runs in order. Also works in `claude --bg` sessions, which have no terminal. |
| talk to nerve's agent | `$.http.fetch(url, {socketPath})` | HTTP over a Unix socket works, so no TCP port is needed. |
| answer a permission prompt | `classic.PermissionRequest` returning `{decision: {behavior}}` | The dialog **stays on screen** while the hook waits. A remote answer returned from the hook closes it. |
| answer `AskUserQuestion` | `tool.call {tool:'AskUserQuestion'}` returning `{result: {questions, answers}}` | Racing `next(e)` (Claude's own dialog) against a remote answer works. The dialog **stays visible** the whole time, so away mode is no longer needed. |
| progress | `turn.start`, `turn.complete` (`durationMs`, `usage`) | Fires per turn, with token counts and the model name. |
| show something in Claude | `$.ui.status(text)` | One line under the prompt: `⚠ nerve: ⇄ nerve · alpha 2s · 1 waiting on another device`. |

**Dead ends found:**
- `$.ui.ask` can't be cancelled once shown.
- Returning early from `tool.call` while Claude's own permission dialog is open **leaves the dialog on screen** and wedges the turn.
- Both are why the mod answers through `classic.PermissionRequest` rather than replacing dialogs.

**Two traps that cost a re-run each:**

1. **A mod hook has 10 s of its own time** (`HookBudget.ms`). The budget pauses while a `$` call is in flight, except `$.clock.sleep`.
   - The first version waited for remote answers with a sleep loop. The hook was cut off after 10 s and the request vanished from other devices.
   - The fix: wait inside `$.http.fetch` (a 2 s long-poll on the agent, `/v1/wait`), which doesn't count.
   - E19 now waits 20 s before answering, to prove it.
2. **Claude does not abort `next.signal` when the user answers a permission dialog at the keyboard.**
   - Measured: `aborted=false` for 14 s after the local answer, until the hook ran out.
   - The mod therefore watches its own `tool.call` hook. When a call with the same tool and input settles, it was answered here, and the request is withdrawn (E21).
   - The agent also withdraws any request whose hook stops long-polling for 6 s.

### Results

| # | what | result |
|---|---|---|
| E17 | `nerve setup` on beta prints a one-time code; `nerve setup <code>` on alpha joins | ✓ both trust each other after **4.7 s**. The code is single-use; a re-run by an already-paired device just confirms |
| E18 | prompt from alpha to an idle session | ✓ `submitted to the session`, ack in **0.37 s** (one-shot CLI, including its connect) |
| E18b | two prompts sent while a 6 s tool call runs | ✓ both queued by Claude and run in order (`E18-QUEUED` answered last) |
| E19 | `Write` permission, answered from alpha **20 s** after it appeared | ✓ still pending after 20 s, then allowed, file written; dialog visible locally throughout |
| E20 | `AskUserQuestion`, answered from alpha, **no away mode** | ✓ question visible on both devices; alpha's "Blue" answered it |
| E21 | permission answered at the keyboard | ✓ withdrawn from alpha's view (pending 1 → none), file written |
| E22 | hand-off beta → alpha, then one more turn and one commit, then again | ✓ first: **694 KiB** (101/101 chunks), 0.5 s. Second: **16–19 KiB** (4–5 of 100 chunks), 0.5 s. Both resumed in tmux with the conversation |

### Hand-off size, in detail

The old pack was one tar: a `git bundle --all` (full history), the patch, untracked files and the transcript, always sent whole. The new one is an exchange:

1. **Offer.** It names the repo by its root commit.
2. **Have.** The target answers with the commit tips its checkout already has.
3. **Pack.** The source bundles only `--all --not <tips>`, so with nothing new there is no bundle at all. Every other file (patch, untracked files, transcript, subagent sidecars) is cut into content-defined chunks: gear hash, 2–32 KiB, 8 KiB average, SHA-256 ids.
4. **Push and fill.** The source pushes only chunks it doesn't know the target holds. The target asks for anything still missing (`ChunkWant`), then acks.
5. **Remember.** On the ack both sides record which chunks the other has.

| | first hand-off | second (1 turn + 1 commit later) |
|---|---|---|
| git | full bundle | only the new commit |
| transcript (~700 KiB, append-only) | all chunks | the re-cut tail: 1–2 chunks |
| sent (E22) | 694 KiB | **16–19 KiB** (~2.5%) |

Unit tests pin the properties:
- an append re-sends at most the old last chunk plus the new bytes;
- an edit in the middle stays local (≤ 3 chunks);
- a forged chunk is rejected by id;
- a second hand-off moves under a tenth of the first (`handoff::tests`).

## E23–E26, E25b: Mac ⇄ Pi, public relays only (2026-10-12)

Scripts: `e23_pi_e2e.sh` and `e25b_mac_session_pi_answers.sh`, outputs next to them.

- **Relays:** only relay.primal.net and relay.snort.social. No private relay, no Tailscale relay; SSH only drives the test.
- **Configs:** throwaway on both machines.
- **Pi:** Claude Code 2.1.296 with the nerve mod loaded via `--plugin-dir`.

| # | what | result |
|---|---|---|
| E23 | `nerve setup` on the Mac prints a code; `nerve setup <code>` on the Pi joins **and installs the systemd user service** | ✓ join + service in **4 s**. `systemctl --user` shows `nerve-sync.…service active running`; `nerve sync doctor` all ✓ except "mod not installed", which is expected with `--no-claude` |
| E24 | prompt from the Mac to the Pi's session, via the mod | ✓ `submitted to the session`, ack in **0.86 s** (4.8 s in the first run, which included the CLI's own relay connect) |
| E25 | permission on the Pi, answered from the Mac | ✗ **not run.** The Pi's Claude account hit its usage limit ("You've hit your session limit · resets 10:20pm") partway through, so Claude on the Pi never asked. Covered by E25b. |
| E25b | the same, reversed: Claude on the Mac with the mod, answered from the Pi | ✓ prompt visible on the Pi after **6 s**. Answered with `nerve sync answer mac … allow` from the Pi; ack ~5.8 s including SSH and a one-shot client; file written on the Mac |
| E26 | hand the Pi's session to the Mac (**Pi → Mac, never run in round 1**), then once more after a commit | ✓ first: 5 of 5 chunks, 3.1 s, resumed in tmux on the Mac. Second: **3 of 5 chunks**, 2.3 s. The Mac checkout's HEAD was the new commit ("more") |

In E26 the test repo is tiny (≈24 KiB packed), so the second transfer saves less than in E22, where the transcript dominates. The index file and the transcript tail are always re-sent; the unchanged files and old history are not.

**Finding: a service-managed agent hid acks from the CLI.**
- In E23's first run, every `nerve sync send` and `answer` printed only "queued", because the systemd agent did the work. The ack arrived, but the CLI couldn't see it.
- Fix (D21): the agent writes `activity.log`, and the CLI follows it until the ack.
- The timings above come from that.
