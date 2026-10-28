# Remote control, answering prompts and handoff for Claude Code sessions: research for nerve

## 0. What I checked locally (Claude Code 2.1.295)

- **Install.** `claude` points to `/opt/homebrew/Caskroom/claude-code@latest/2.1.295/claude`. It is a 240 MB native arm64 Mach-O built with Bun. There is no readable JS package, so I ran `strings` over the binary instead.
- **Hidden flags.** `--channels` and `--dangerously-load-development-channels` are in the binary but not in `claude --help`. So is `--sdk-url`, an internal flag I won't use. The binary also contains `notifications/claude/channel`, `notifications/claude/channel/permission` and `notifications/claude/channel/permission_request`.
- **Hook event names in the binary.** PreToolUse, PostToolUse, PostToolUseFailure, PostToolBatch, UserPromptSubmit, UserPromptExpansion, Notification, Stop, StopFailure, SubagentStart, SubagentStop, PreCompact, PostCompact, SessionStart, SessionEnd, Setup, PermissionRequest, PermissionDenied, Elicitation, ElicitationResult, TaskCreated, TaskCompleted, TeammateIdle, ConfigChange, CwdChanged, FileChanged, InstructionsLoaded, WorktreeCreate, WorktreeRemove.
  - The docs also list MessageDisplay, DirectoryAdded, PreModelSwitch and PostModelSwitch (33 events in total).
- **`claude agents --json`.** Prints active sessions, interactive and background, as JSON and does not need a TTY. `--all` also includes finished ones.
- **`claude attach <id|name>`.** Opens a background session in the current terminal.
- **`claude logs <id|name>`.** Prints a background session's recent terminal output.
- **Other subcommands.** `stop`, `respawn`, `rm` and `purge` exist.
- **`--bg --resume <id>`.** Continues that session in the background under the same ID. If the session is already running, it starts a copy instead.
- **`--permission-prompts host|none`.** In `-p` mode, decides whether the SDK host or `--permission-prompt-tool` answers prompts, or whether they are auto-denied.
- **`~/.claude/settings.json`.** Top-level keys: `enabledPlugins`, `extraKnownMarketplaces`, `hooks`, `model`, `permissions`, `skipDangerousModePermissionPrompt`, `theme`.
  - Hooks are already set for PostToolUse, PreCompact, PreToolUse, SessionEnd, SessionStart, Stop and UserPromptSubmit. I did not print any values.
- **This session's config dir is `~/.claude-ext`, not `~/.claude`.** Its transcripts are under `~/.claude-ext/projects/`, so `CLAUDE_CONFIG_DIR` is set. Both directories have a `sessions/` folder, and the session file I inspected lives under `~/.claude/sessions/`. nerve's `detect::claude_roots()` rule is the right one.
- **Fields in a live `sessions/<pid>.json`.** Secret-like keys were redacted. Useful fields for nerve:
  - `sessionId`, `cwd`, `kind` (`interactive`), `entrypoint` (`claude-desktop`)
  - `peerProtocol: 1`, `peerFeatures: ["notify_idle", …]`
  - `messagingSocketPath: "/tmp/cc-socks/<pid>.sock"`
  - `status` (`busy`) and `statusUpdatedAt`
  - `bridgeSessionId: "session_01…"` (looks like the Remote Control bridge ID **[UNCERTAIN]**)
  - `<pid>.<hash>.key` files sit next to the JSON. They are probably the messaging token, so nerve must never render them.
- **Session layout on disk.** `projects/<slug>/<session-id>/subagents/agent-<id>.jsonl` and `.meta.json`, plus `tool-results/`. `file-history/<session-id>/` is separate.

## 1. Ways to send input into a running interactive session

### 1.1 Remote Control (official)

**How it works**
- Start it with `claude --remote-control [name]`, `/remote-control` (`/rc`) inside a session, or `claude remote-control`. The last one is server mode: it can spawn sessions on demand, `--capacity` defaults to 32, and it has `--spawn` and `--[no-]create-session-in-dir`.
- `/config` can turn on auto-connect for every session.
- Each interactive process registers one remote session.

**Transport**
- The docs say the session "makes outbound HTTPS requests only" and "never opens inbound ports".
- Messages go through Anthropic's servers, which issue short-lived credentials. The machine needs `api.anthropic.com:443`.
- Cross-session messages to a session on another machine also travel "through Anthropic servers, arriving over that machine's Remote Control connection".

**Auth**
- Works on Pro, Max, Team and Enterprise. "API keys are not supported."
- `claude setup-token` and `CLAUDE_CODE_OAUTH_TOKEN` fail with "Remote Control requires full-scope login token". You need `claude auth login` with a claude.ai account.
- Team and Enterprise need an Owner to turn it on, and may require Trusted Devices.
- It is off when `ANTHROPIC_BASE_URL` points anywhere other than api.anthropic.com, and on Bedrock, Vertex or Foundry.

**Clients**
- claude.ai/code, the Claude iOS and Android apps, or any browser. `/mobile` shows a QR code to get the app.

**What a remote client can do**
- Send prompts and attach images or files. Files are downloaded locally and passed to Claude as `@` references.
- See subagent and workflow progress.
- Answer permission prompts and AskUserQuestion. The docs' limitations section talks about forwarded prompts, and the CHANGELOG mentions remote status for "a session waiting on a permission prompt". **[UNCERTAIN]** I did not find a spec of exactly which dialog types are forwarded.

**Limitations**
- The local process has to keep running; it reconnects after sleep or network loss.
- After roughly 30 minutes of failed heartbeats it disconnects and needs `/remote-control` again.
- Queued messages that arrive while the process is exiting are kept for the next resume (CHANGELOG).
- There is no public API or protocol. nerve cannot be a Remote Control client; only Anthropic's apps can. Its role would be to start sessions with RC enabled and show the `bridgeSessionId` or a link.
- A Raspberry Pi can only take part as a browser pointed at claude.ai/code.

### 1.2 Channels: MCP servers that push events into a session (research preview)

**What they are**
- A channel is an MCP server over stdio that declares `capabilities.experimental['claude/channel']`. It pushes events with `notifications/claude/channel`, and params are `{ content: string, meta?: Record<string,string> }`.
- Each `meta` key becomes an attribute on a `<channel source="<server>" …>` tag that Claude sees. Keys must be letters, digits or underscores; anything else is silently dropped.

**Requirements**
- Bun for the official plugins.
- Claude.ai or Console API-key auth. Not available on Bedrock, Vertex or Foundry.
- Team and Enterprise must set `channelsEnabled: true` and can restrict plugins with `allowedChannelPlugins`.
- Official channels are Telegram, Discord and iMessage, plus the `fakechat` demo.

**Enabling**
- `claude --channels plugin:telegram@claude-plugins-official` accepts only allowlisted plugins.
- A custom server needs `claude --dangerously-load-development-channels server:<name>` (or `plugin:x@market`). This shows a confirmation prompt, so it only works interactively. Under `-p` or the SDK the flag is ignored and the channel does not register.

**Delivery**
- There is no acknowledgement. `await mcp.notification()` resolves once the message is written. If the session isn't there to receive it, the event is silently dropped.

**Two-way**
- Expose a `reply` tool. Claude calls it to answer.

**Permission relay** (exact protocol from channels-reference)
- **Opt in:** declare `experimental: { 'claude/channel': {}, 'claude/channel/permission': {} }`.
- **Outbound from Claude Code:** `notifications/claude/channel/permission_request` with params `{ request_id, tool_name, description, input_preview }`.
  - `request_id` is five lowercase letters a–z, excluding `l`.
  - Since v2.1.211, `description` and `input_preview` are sanitised, capped at 3,500 code points, and credentials are replaced with `[REDACTED]`.
- **Inbound verdict:** `notifications/claude/channel/permission` with `{ request_id, behavior: 'allow' | 'deny' }`. It applies to that one call only.
- **Which answer wins:** the local dialog and the relay are both live, and "Claude Code applies whichever answer arrives first and closes the other."
- **What it covers:** tool-use approvals such as Bash, Write and Edit. Project-trust and MCP-consent dialogs are not relayed.
- **AskUserQuestion is not documented as relayable** over channels **[UNCERTAIN]**. The only documented channel path is the `yes/no <id>` tool approval.
- **In auto mode** the classifier decides and no relay happens. The doc example switches to manual mode.
- **Security:** the docs say "An ungated channel is a prompt injection vector". Gate on the sender's identity (`message.from.id`), not the chat or room.
- A recent CHANGELOG fix ignores a repeated reply ID so it can't approve a different prompt.

**For nerve:** this is the best documented way to push instructions into a live interactive session and approve tools remotely without Anthropic's relay. The catch is that the session must be launched with the dev-channel flag and a confirmation, unless nerve ships as an allowlisted plugin (unlikely).

### 1.3 Hooks

**Common input fields:** `session_id`, `prompt_id`, `transcript_path`, `cwd`, `scratchpad_dir` (v2.1.257+), `permission_mode`, `hook_event_name`. Tool events add `tool_name`, `tool_input` and `tool_use_id`.

**Universal output fields:** `continue` (false stops everything), `stopReason`, `systemMessage`, `suppressOutput`, `terminalSequence`.

| Event | Can it add input or keep Claude going? | Output |
|---|---|---|
| SessionStart | Adds context at start or resume. `initialUserMessage` works only with `-p`. | `hookSpecificOutput.additionalContext`, `sessionTitle`, `watchPaths`, `reloadSkills`. Input includes `source` (startup/resume…), `seconds_since_last_response`, `context_tokens`. |
| UserPromptSubmit | Can add context but **cannot replace the prompt**. | `additionalContext`; `decision:"block"` + `reason`. Plain stdout becomes context. |
| PostToolUse | Adds context after a tool runs. | `hookSpecificOutput.additionalContext`, `updatedToolOutput`; `decision:"block"`. |
| Stop / SubagentStop | **Keeps the agent working:** `{"decision":"block","reason":"…"}` makes Claude continue, with `reason` as the instruction. Check `stop_hook_active` to avoid loops. | Also accepts `hookSpecificOutput.additionalContext`. |
| Notification | Watch only. `notification_type` is one of `permission_prompt` (fires about 6 s after the prompt appears), `idle_prompt`, `elicitation_dialog`, `elicitation_url_dialog`, `elicitation_complete`, `elicitation_response`, `agent_needs_input`, `agent_completed`, `auth_success`, `quota_auto_resume_*`. | No decision control; only `terminalSequence` works. |
| PermissionRequest | **Answers a permission prompt.** Fires only when Claude Code is about to ask, or would auto-deny. | See schema below. Exit code 2 is ignored. |
| PreToolUse | Decides on any tool call and **can answer AskUserQuestion**. | `permissionDecision: allow/deny/ask/defer`, `permissionDecisionReason`, `updatedInput`, `additionalContext`. |
| Elicitation | Answers MCP elicitation forms without showing a dialog. | `hookSpecificOutput.action` accept/decline/cancel + `content`. |
| Async hooks | `"async": true` delivers output on the next turn; if the session is idle it waits. `"asyncRewake": true` exiting with code 2 **wakes an idle Claude immediately**, with stderr shown as a system reminder. | This is the only documented hook path that injects into an idle session. |

**PermissionRequest input** (from the docs):
```json
{"session_id":"…","transcript_path":"…","cwd":"…","permission_mode":"default","hook_event_name":"PermissionRequest",
 "tool_name":"Bash","tool_input":{"command":"rm -rf node_modules","description":"…"},
 "permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"rm -rf node_modules"}],"behavior":"allow","destination":"localSettings"}]}
```

**PermissionRequest output:**
```json
{"hookSpecificOutput":{"hookEventName":"PermissionRequest",
  "decision":{"behavior":"allow","updatedInput":{"command":"npm run lint"},"updatedPermissions":[…]}}}
```
- On deny: `{"behavior":"deny","message":"…","interrupt":true}`.
- A matching deny rule still wins over `allow`.

**Answering AskUserQuestion through PreToolUse.** Echo back the original `questions` and add an `answers` map. `"allow"` on its own is not enough.
```json
{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow",
 "updatedInput":{"questions":[{"question":"Which framework?","header":"Framework",
   "options":[{"label":"React","description":"…"},{"label":"Vue","description":"…"}],"multiSelect":false}],
 "answers":{"Which framework?":"React"}}}}
```
- For multi-select, pass an array or `", "`-joined labels. The SDK also accepts a free-text `response`.
- MCP tools marked `_meta["anthropic/requiresUserInteraction"]` cannot be auto-answered this way.

**Design note for remote answering**
- A blocking PermissionRequest or PreToolUse hook could wait on nerve's server for a phone answer. The default `command` hook timeout is 600 s.
- **[UNCERTAIN]** The docs don't say whether the local dialog is already visible while a PermissionRequest hook runs. Everything suggests the hook runs *before* the dialog, so a slow hook delays the local prompt. Channel relay, by contrast, races the local dialog explicitly.
- For AskUserQuestion, a PreToolUse hook that blocks until the phone answers is the only documented remote path besides Remote Control and the SDK.
- Hooks need to be in settings when the session starts. nerve can't add one to a session that is already running without a settings reload (`ConfigChange` exists, but I didn't verify hot reload).

### 1.4 Cross-session messaging socket (local)

- Each session exposes `messagingSocketPath`, for example `/tmp/cc-socks/<pid>.sock`, falling back to `/tmp/cc-socks-<uid>`.
- A connection may start with `{"type":"auth","token":"<CLAUDE_CODE_MESSAGING_TOKEN>"}`.
- Messages from peers are labelled as coming from another session, not from the user, and "can't answer a pending permission prompt". The receiver's `crossSessionInbound` setting is `accept`, `hold` or `refuse`.
- **[UNCERTAIN]** The wire format beyond the auth line isn't publicly specified. I couldn't pull it from the binary quickly, and it's an internal, versioned protocol (`peerProtocol: 1`).
- That makes it a tempting "send an instruction" path for nerve, but brittle, and semantically weaker than a real user message.

### 1.5 tmux send-keys and PTY

- Works on any interactive session running in tmux: `tmux send-keys -t pane "text" Enter`, or arrow keys and digits for dialogs.
- Claude Squad, ccremote, Claude-Code-Remote (tmux mode) and handmux all work this way.
- Pros: universal, and answers any dialog, including trust prompts.
- Cons: you scrape the screen for state, typing races with the TUI, and it can't reach Ghostty tabs that aren't in tmux. On macOS, Ghostty would need AppleScript keystroke injection.

### 1.6 Background sessions

- `claude --bg`, then `claude attach`, `claude logs` and `claude agents --json`. The supervisor (`~/.claude/daemon/`) keeps sessions alive.
- **Reading state:** the docs call `claude agents --json --all` "the supported way to read session state from outside". Fields are `state` (`working|blocked|done|failed|stopped`), `status` (`busy|waiting|idle`), `waitingFor` (`permission prompt`, `input needed`, `sandbox request`, `worker request`, `dialog open`), `id`, `pid`, `sessionId`, `name`, `cwd`, `kind`, `startedAt`.
- **Not stable:** "The files under `~/.claude/jobs/<id>/` are not a stable interface." nerve should move from `jobs/*/state.json` to `agents --json`.
- **No CLI to send input.** There is no "send message to background session" command. Input happens through `attach` (a TTY, which a PTY bridge on the Mac could drive), Remote Control, or channels.
- **Idle sessions get unloaded.** Sessions that are finished and unattached for about an hour stop, and resume when you reply or attach.

### 1.7 Agent SDK / headless (`-p --input-format stream-json`)

This gives full programmatic control, but only for sessions **you start**. You cannot attach it to an existing interactive TUI. See §3.

## 2. Moving a session to another machine

- **Storage:** `<config>/projects/<slug>/<session-id>.jsonl`. The slug is the cwd with every non-alphanumeric character replaced by `-`. If that is longer than 200 characters, it's truncated and a hash is added. `CLAUDE_CODE_PROJECT_DIR_NAME` can override it.
- **How `--resume` finds a session:** `claude --resume <id>` searches the current project and its worktrees first. In other projects it resolves the ID only if exactly one project holds it, so "a hand-copied duplicate makes Claude Code report not-found". `claude --resume <path/to/file.jsonl>` resumes straight from a file.
- **Copying to another host is officially supported** (Agent SDK sessions docs, "Resume across hosts"):
  - Either "Move the session file": restore `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` on the new host before calling resume.
  - Or attach a `sessionStore` / `session_store` adapter that mirrors transcripts.
  - Either way, the `cwd` must match the original run's.
  - In practice: put the repo at the same absolute path on the Pi, or write the file into the slug for the Pi's path and resume by file path. **[UNCERTAIN]** whether old absolute paths inside tool calls confuse later turns. They're only text, so probably harmless.
- **Subagent transcripts** (`<session-id>/subagents/agent-*.jsonl` + `.meta.json`) and `tool-results/` belong to the session directory. Copy them so expanded subagent views and large tool outputs survive.
  - **[UNCERTAIN]** whether resume needs them. The main transcript holds the Agent tool results.
- **file-history** (`<config>/file-history/<session-id>/`) holds checkpoint backups for `/rewind`. It's only needed if you want rewind to work on the target. Todos/tasks (`tasks/`) are optional.
- **Code state is not carried.** Push the branch or worktree through git, or rsync.
- **`--fork-session`** with `--resume` or `--continue` gives the conversation a new session ID. Use it for handoff so both machines don't append to the same ID; the docs warn against resuming one session in two places without forking.
- **`--bg --resume <id>`** continues a session in the background on the new host.
- **Teleport is cloud to local only:**
  - `claude --teleport [session]` or `/teleport` pulls a claude.ai/code cloud session into your terminal. It checks the repo, fetches and checks out the branch, and needs the same account.
  - "you can't push an existing terminal session to the cloud."
  - The desktop app's "Open in → Cloud" carries the conversation over **as a summary** only.
  - `--cloud <id|url>` with `-p` queues a message into an existing cloud session.
  - There is **no local-to-local teleport.** Handoff between your own machines is: copy the jsonl and its directory, sync the code, then `claude --resume <id> [--fork-session]`.
- **Self-hosted environments** (`--environment ccpool_…`) run *cloud* sessions on your own runners. They're Team and Enterprise only, in beta, and work from GitHub checkouts. The docs point Pro and Max users who want "your own always-on machine" to Remote Control instead.

## 3. Agent SDK as a controllable worker on a Pi

TypeScript `@anthropic-ai/claude-agent-sdk`, Python `claude-agent-sdk`. Both drive the same CLI over stream-json.

- **Streaming input:** pass an async iterable of `SDKUserMessage`s to `query({prompt})`. You can push new instructions at any time, and they queue between turns. This is what Happy's "remote mode" does with a `PushableAsyncIterable` around `query()`.
- **`canUseTool(toolName, input, {signal, suggestions})`** returns `{behavior:"allow", updatedInput}` or `{behavior:"deny", message, interrupt?}`. In Python it's `PermissionResultAllow(updated_input=…)` / `PermissionResultDeny`.
  - Before v2.1.207 an allow without `updatedInput` was rejected, so always include it.
  - AskUserQuestion arrives here as well. Return `updatedInput` containing `questions` + `answers` (question text → label) and an optional `response`.
  - This is the cleanest way to send prompts to a phone and wait as long as needed.
- **`interrupt()`** ends the current turn cleanly. `setPermissionMode()` and `setModel()` change settings mid-session. SIGTERM leaves the turn unrecorded, while SIGINT or `interrupt()` finishes it. `CLAUDE_CODE_RESUME_INTERRUPTED_TURN=1` continues the interrupted turn on resume.
- **Resume and fork:**
  - `options.resume = <sessionId>`, with `forkSession: true` / `fork_session=True`.
  - `listSessions()` and `getSessionMessages()` (Python `list_sessions` / `get_session_messages`).
  - `sessionStore` for transcripts shared across hosts.
- **CLI equivalent:** `claude -p --input-format stream-json --output-format stream-json --verbose [--include-partial-messages --include-hook-events --forward-subagent-text --replay-user-messages] --permission-prompt-tool mcp__x__approve`. Use `--permission-prompts none` for unattended jobs.
  - The init event has a `capabilities` array (for example `interrupt_receipt_v1`) for feature detection.
  - SDK and `-p` sessions don't appear in `/resume` or `--continue`, but `--resume <id>` works.
- **Auth for unattended workers:** an API key or a `setup-token` OAuth token works for the SDK. Remote Control and dev channels do not work there.
- **[UNCERTAIN]** I did not verify Raspberry Pi (linux-arm64) support for the native build. Node or Bun plus the SDK's bundled CLI should work.

## 4. Third-party projects

Star counts were taken from the GitHub API today.

| Project | How it sends input / reads output | Transport | E2E | Handoff | Maturity |
|---|---|---|---|---|---|
| **Happy** (slopus/happy) | `happy claude` wraps the CLI. Local mode spawns the real `claude` TUI and tracks the session ID through a SessionStart hook. Remote mode switches to the Agent SDK `query()` with streaming input, `--resume`, and `canUseTool` for phone approvals. A daemon can start sessions remotely. | happy-server relay (self-hostable package `happy-server-self-host`) | Yes; keys paired by QR. Provider keys sent via `happy connect` are *not* E2E. | Switches local↔phone on the same machine; sessions show on the phone automatically | 24.1k★, active |
| **Paseo** (getpaseo/paseo) | Local daemon manages agents; desktop, mobile, web and CLI clients over WebSocket; answers permissions; TypeScript SDK; `/paseo-handoff` between agents | Optional E2E relay, or direct TCP / Tailscale / VPN; Docker | Yes (relay) | Between agents, and to remote daemon hosts | 20.3k★, active |
| **CloudCLI / claudecodeui** (siteboon) | Web UI that reads `~/.claude/projects`, runs Claude, includes a shell | Self-hosted Node server, or hosted CloudCLI Cloud | No (TLS / reverse proxy) | No | 14.0k★, active |
| **Opcode** (formerly Claudia; winfunc/opcode) | Tauri desktop GUI; manages sessions and background agents | Local only | n/a | No | 22.4k★ |
| **Claude Squad** (smtg-ai) | tmux session per agent plus git worktrees; send-keys | Local | n/a | No | 8.6k★ |
| **VibeTunnel** (amantus-ai) | Browser terminal on PTYs | Tailscale or ngrok recommended | Via the tunnel | No | 4.7k★, last push Aug 2026 |
| **Crystal → Nimbalyst** (stravu) | Desktop app, parallel worktrees | Local | n/a | No | 3.1k★, renamed |
| **Omnara** (omnara-ai) | Has pivoted to a "self-hostable managed agent platform": API, Postgres event log and workers running the agent loop; approvals over the API | Omnara Cloud or self-hosted | **[UNCERTAIN]** | No | 2.9k★ |
| **ccmanager** (kbwo) | Own PTY manager (no tmux), status hooks, AI auto-approval | Local | n/a | No | 1.3k★, active |
| **Claude-Code-Remote** (JessyTsui) | Claude hooks for notifications; replies by email, Telegram or Discord are injected through a PTY or tmux | ngrok webhooks / SMTP | No | No | 1.3k★, unmaintained since Dec 2025 |
| **cui** (wbopan) | Web UI | n/a | n/a | n/a | **Archived**; README sends users to Remote Control and Claude Code on the web |
| **Kanna** (jakemor) | Web UI on the Agent SDK; JSONL event-sourced store; WebSocket | `--host` on a Tailscale IP, `--password` | No | No | 709★, active |
| **YepAnywhere** (kzahel) | Self-hosted web UI using existing CLI session history; approvals, questions, push notifications | Direct, or a free public E2E relay | Yes | Switch devices mid-work | 535★, active |
| **sled** (layercodedev) | Phone UI talking to agents over ACP | Tailscale; Cloudflare Durable Objects variant | Via Tailscale | No | 306★, last push Jan 2026 |
| **ccremote** (generativereality) | tmux polling every 2 s; Discord approvals; quota auto-continue | Discord | No | No | 48★; README now says use Remote Control |
| **CodeDeck+** (deymosh) | Bridge exposes sessions; phone approves tools and answers questions | **Nostr** NIP-44 encrypted DMs over any relays, NIP-42 auth, Tor | Yes | No | New, 0★, Android only |
| **agentnoise** (nvk) | Workers drive agents via stream-json; "work-chat handoff" | White Noise (Nostr/MLS messenger) | Yes | Chat-thread handoff | 21★ |
| **9remote**, **handmux**, **quicktui** | PTY / tmux in the phone browser | WebRTC P2P (9remote: X25519 + AES-GCM, split-key pairing) | 9remote yes | No | 55–163★, 2026 entrants |
| **247-claude-code-remote** (QuivrHQ) | Terminal on Fly.io VMs | Tailscale | Via Tailscale | No | 75★ |
| **remote-agentic-coding-system** (coleam00) | Slack, Telegram and GitHub bots | Bot APIs | No | No | 351★ |
| **Conductor** | Closed-source Mac app for parallel worktree agents | Local | n/a | No | **[UNCERTAIN]**; not re-checked |

**The pattern:** the strong products (Happy, Paseo, Kanna, YepAnywhere, agentnoise) **own the process** through the SDK or stream-json, so injecting input and approving prompts is first-class. Monitor-only tools fall back to tmux or PTY keystrokes. Several older remote tools (cui, ccremote) now point users to Anthropic's Remote Control.

## 5. Recommended control surface for nerve

nerve watches sessions it didn't start, so the options split into "attach to someone else's session" and "nerve starts the worker".

**Send an instruction**
1. **A channel server that nerve ships**, for example `nerve-channel` over MCP stdio, started via `--dangerously-load-development-channels server:nerve`. nerve's Mac process POSTs to it, gated by sender identity, and it emits `notifications/claude/channel`. Two-way via a `reply` tool. Works on interactive sessions without Anthropic's relay.
   - Cost: the session must be launched with the flag and a confirmation, and the feature is a research preview.
2. **tmux send-keys** for sessions in tmux panes. Universal but screen-scraped, and nerve already knows the panes.
3. **Remote Control.** Fine for a person with a phone, but nerve can't drive it programmatically. nerve could show RC status (`bridgeSessionId`) and add a "start with `--remote-control`" action.
4. **asyncRewake hook.** A command hook that waits for a nerve message and exits 2 wakes an idle Claude with stderr as context. Hacky, but no flags needed once installed.
5. **Cross-session socket.** Don't use it: the protocol is undocumented, and messages are labelled as not from the user.

**Answer a permission prompt or AskUserQuestion**
1. **For nerve-launched workers, use the SDK `canUseTool`.** It's the only path that handles both, with no timeout games.
2. **Channel permission relay** for tool approvals in interactive sessions. It races the local dialog, which is the nicest UX, but AskUserQuestion is not covered **[UNCERTAIN]**.
3. **Blocking PermissionRequest hook** (allow/deny, plus `updatedPermissions` for "always") and **a PreToolUse hook on `AskUserQuestion`** returning `updatedInput.answers`.
   - Both are installed in settings and talk to nerve over a local socket.
   - The 600 s hook timeout applies, and the local dialog is probably delayed while the hook waits **[UNCERTAIN]**. Fall back to "ask" or no output on timeout.
4. **tmux keystrokes**, the last resort.
- **Detecting that a prompt is pending:** use `claude agents --json` `waitingFor` and the Notification hook (`permission_prompt` ~6 s, `elicitation_dialog`, `idle_prompt`, `agent_needs_input`).

**Read progress, diffs and subagent work**
1. **Keep reading transcripts** (`projects/<slug>/<id>.jsonl` and `<id>/subagents/*.jsonl`), following `Sanitised` and `claude_roots()`. Edit/Write `tool_use` inputs and `tool-results/` give the diffs.
2. **Move job state to `claude agents --json --all`**, which is the documented-stable API. Keep `sessions/<pid>.json` (`status`, `name`, `kind`) for interactive sessions. Run it in the discovery worker, never on the UI tick.
3. **Optional hooks feed:** PostToolUse, Stop and SubagentStop hooks append to a nerve event log, which is lower latency than polling.
4. **For workers:** `--output-format stream-json --include-hook-events --forward-subagent-text`.
- **Getting it to the Pi or phone:** a small nerve HTTP/WebSocket server bound to Tailscale (the most common pattern above). If going over the internet, add E2E encryption modelled on Happy, Paseo or YepAnywhere (key pairing by QR, relay only forwards ciphertext).

**Handoff**
1. **Stop the session on the source machine.**
2. **Sync the code** with git push of the branch or worktree.
3. **Copy the session files:** `<id>.jsonl` and the `<id>/` folder (subagents, tool-results), and optionally `file-history/<id>/`, into the target's `claude_roots()/projects/<slug-for-target-cwd>/`.
4. **On the target:** `claude --resume <id> --fork-session` (or `--bg --resume`, or SDK `resume` + `forkSession`). This is the officially documented "resume across hosts" path; the cwd must match.
   - Use the same absolute repo path, or rewrite the slug and resume by file path.
5. **Do not use teleport.** It is cloud → local only. Desktop "Open in Cloud" only moves a summary.
6. **For SDK workers:** implement a `sessionStore` adapter shared between the Mac and the Pi.

**Overall:** nerve stays a passive monitor for sessions it didn't start (transcripts + `agents --json`). It adds an optional "nerve-controlled" launch mode: interactive sessions with a nerve channel and a hook bundle, or headless SDK workers on the Pi with `canUseTool`. That second mode is where sending input, answering prompts and handoff become reliable. Remote Control stays the zero-effort option for users with a claude.ai login.

## Sources

**Official docs**
- https://code.claude.com/docs/en/remote-control
- https://code.claude.com/docs/en/channels
- https://code.claude.com/docs/en/channels-reference
- https://code.claude.com/docs/en/hooks
- https://code.claude.com/docs/en/sessions
- https://code.claude.com/docs/en/agent-view (background sessions, `claude agents --json`, supervisor)
- https://code.claude.com/docs/en/cross-session-messaging
- https://code.claude.com/docs/en/claude-code-on-the-web (teleport, `--cloud`)
- https://code.claude.com/docs/en/desktop (Open in → Cloud)
- https://code.claude.com/docs/en/self-hosted-environments
- https://code.claude.com/docs/en/headless
- https://code.claude.com/docs/en/env-vars
- https://code.claude.com/docs/en/agent-sdk/user-input (`canUseTool`, AskUserQuestion answers)
- https://code.claude.com/docs/en/agent-sdk/sessions (resume across hosts, sessionStore, fork)
- https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode
- https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md
- https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins (telegram, discord, imessage, fakechat)

**Third-party**
- https://github.com/slopus/happy (incl. `packages/happy-cli/src/claude/claudeRemote.ts`, `claudeLocal.ts`)
- https://github.com/getpaseo/paseo
- https://github.com/siteboon/claudecodeui
- https://github.com/winfunc/opcode
- https://github.com/smtg-ai/claude-squad
- https://github.com/amantus-ai/vibetunnel
- https://github.com/stravu/crystal
- https://github.com/omnara-ai/omnara
- https://github.com/kbwo/ccmanager
- https://github.com/JessyTsui/Claude-Code-Remote
- https://github.com/wbopan/cui
- https://github.com/jakemor/kanna
- https://github.com/kzahel/yepanywhere
- https://github.com/layercodedev/sled
- https://github.com/generativereality/ccremote
- https://github.com/deymosh/codedeck-plus
- https://github.com/nvk/agentnoise
- https://github.com/decolua/9remote
- https://github.com/handmux/handmux
- https://github.com/QuivrHQ/247-claude-code-remote
- https://github.com/coleam00/remote-agentic-coding-system
- https://github.com/andyrewlee/awesome-agent-orchestrators
