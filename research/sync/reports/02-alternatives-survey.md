# Alternatives to Nostr for syncing nerve across Mac, Raspberry Pi and Android
*Research date: 2026-10-09. Crate versions, release dates and MSRVs come from the crates.io API, queried today. Other claims come from vendor docs or my own knowledge, and are marked "(unverified)" where I did not re-check them today.*
---
## 0. Two findings that change the framing
**1. Claude Code already has a first-party cross-device feature.** It is called Remote Control, and it covers several of nerve's requirements. `claude remote-control`, or `/remote-control`, connects a local session to claude.ai/code and to the Claude iOS and Android apps. From there you can:
- watch progress, including subagents and workflows;
- send messages;
- answer permission prompts and `AskUserQuestion` dialogs, which stay open until you answer them;
- attach files.
How it connects:
- The local process "makes outbound HTTPS requests only and never opens inbound ports". All traffic goes through the Anthropic API over TLS.
- It needs a Pro, Max, Team or Enterprise subscription. API-key authentication is not supported.
- Server mode can run several sessions from one process.
Source: https://code.claude.com/docs/en/remote-control
What it does **not** cover:
- device-to-device session handoff with git working state;
- per-device sync status;
- placing jobs on devices by capability;
- self-hosting, or working offline from Anthropic;
- traffic that never leaves your own devices. Anthropic routes everything, so this is not end-to-end encryption in the self-hosted sense.
**Recommendation:** nerve should *complement* Remote Control (fleet view, handoff, job placement, diffs) rather than rebuild phone steering. Where a session already has Remote Control on, nerve could offer a "deep link into Remote Control" path.
**2. MSRV is a hard constraint.** nerve's MSRV is 1.85. Most of the serious networking crates now need more:
| Crate | Minimum Rust |
|---|---|
| iroh 1.3.0 | 1.91 |
| libp2p 0.57 | 1.88 |
| async-nats 0.50 | 1.88 |
| matrix-sdk 0.19 | 1.96 |
| automerge 0.12 | 1.90 |
| tailscale (tailscale-rs) 0.6.1 | 1.92 |
| rmcp 3.5 | 1.88 |
| agent-client-protocol 3.3 | 1.88 |
| redis 1.7 | 1.88 |
| magic-wormhole 0.8 | 1.92 |
These fit within 1.85: nostr-sdk 0.45.4 (1.85), str0m 0.24 (1.85), quinn 0.11.12 (1.85), tokio-tungstenite 0.30 (1.85), axum 0.8.9 (1.80), zenoh 1.10.1 (1.75) and rumqttc 0.25.1 (1.64).
So either:
- the sync layer goes in a **separate binary or crate** (for example a `nerve-sync` daemon) with its own MSRV, or
- nerve raises its MSRV to about 1.91.
The Nostr design under evaluation fits the current MSRV. Most alternatives do not.
---
## 1. Survey (40 options)
### A. P2P / QUIC
**1. iroh** (n0). QUIC connections addressed by public key ("dial keys, not IPs").
- **Connectivity:** hole punching first; falls back to relays (HTTPS/WebSocket over 443). The relay never sees plaintext; the key is never sent to it.
- **Status:** 1.0 shipped 2026-06-15. Now `iroh` 1.3.0 (2026-09-28, MSRV 1.91).
- **Public relays:** free, rate-limited, "hobby use"; only the latest stable release is officially supported. Relays for v0.9x and 1.0-rc builds were scheduled to stop on 2026-09-30, and 0.35x on 2026-12-31. Public-relay users are forced onto a treadmill of iroh upgrades.
- **Self-hosting:** run `iroh-relay` yourself. A Pi works, but it needs a public port or tunnel; n0 says one relay handles about 60k connections.
- **Offline:** no store-and-forward.
- **Mobile/browser:** FFI bindings for Swift and Kotlin exist (unverified current state); there is a WASM/browser build that is relay-only (unverified).
- **Fit:** (a) excellent; (b) excellent; (c) via iroh-docs; (d) excellent via iroh-blobs.
- **Sources:** https://www.iroh.computer/blog/v1 · https://docs.iroh.computer/concepts/relays · https://docs.iroh.computer/iroh-services/relays/public
**2. iroh-gossip** 0.101.0 (2026-06-15, MSRV 1.91). Epidemic broadcast trees (HyParView/PlumTree) per topic over iroh. Good for fan-out of progress events. Still pre-1.0, unlike core iroh.
**3. iroh-docs** 0.101.0. Multi-writer key-value replicas (range-based set reconciliation, Willow-inspired) with blob references. A fit for durable state such as session index and device status. Pre-1.0; n0 has signalled less focus on it than on core (unverified).
**4. iroh-blobs** 0.103.1 (2026-10-06). BLAKE3-verified, resumable, content-addressed transfer. An ideal fit for transcripts, diffs and git bundles. `sendme` 0.36 is a CLI built on it.
**5. rust-libp2p** 0.57.0 (2026-09-11, MSRV 1.88).
- **Connectivity:** TCP/QUIC/WebRTC/WebTransport. Circuit Relay v2 (`libp2p-relay` 0.22) plus DCUtR hole punching (`libp2p-dcutr` 0.15). You must run your own relay, which needs a public address. Kademlia/mDNS for discovery.
- **Messaging:** gossipsub (`libp2p-gossipsub` 0.50) for pub/sub.
- **Encryption:** Noise/TLS on links; no application-level offline store.
- **Mobile:** Kotlin and Swift exist via other implementations; a Rust FFI build is DIY.
- **Assessment:** powerful but heavy. For three personal devices it is a lot of machinery compared with iroh.
- https://github.com/libp2p/rust-libp2p
**6. Holepunch** (Hypercore / Hyperswarm / HyperDHT / Pear / Bare).
- **Connectivity:** a DHT handles hole punching, including the "random NAT" case; DHT nodes can relay.
- **Data:** Hypercore append-only logs with Noise encryption. Store-and-forward only if some peer stays online (a "blind peer" or seeder).
- **Mobile:** strong. The Bare runtime runs on iOS and Android, and Keet ships on it.
- **Rust:** `hypercore` 0.16.0 (datrs, 2026-08-07) is maintained. `hyperswarm` 0.1.0 (2026-06-19) is brand new, with 6 downloads. The ecosystem is JavaScript-first, so nerve would effectively need a Node or Bare sidecar.
- https://docs.pears.com · https://github.com/datrs/hypercore
**7. Veilid.** Privacy-first, onion-routed P2P application framework with a DHT and private routes; no ports needed.
- **Crate:** `veilid-core` 0.5.7 (2026-07-19, MSRV 1.89). Flutter bindings for mobile.
- **Assessment:** high latency, small community (2.8k recent downloads). Better suited to anonymity than to a fleet dashboard.
- https://veilid.com
**8. Willow / Earthstar.** Willow is a family of specs for path-addressed, capability-secured (Meadowcap) data with range sync; Earthstar is moving onto it.
- **Crates:** `willow25` 0.7.9 and `willow-data-model` 0.8.0 (Codeberg, worm-blossom); about 1–2k downloads.
- **Transport:** you bring your own (iroh is a natural pairing).
- **Assessment:** elegant but early; no mobile story yet.
- https://willowprotocol.org
**9. Reticulum** (with LXMF messaging). A cryptographic mesh stack that runs over anything, including LoRa, TCP and I2P. LXMF propagation nodes provide store-and-forward. Mobile clients exist (Sideband, Python).
- **Rust:** two young ports: `reticulum` 0.1.0 (Beechat, 2025-10) and `reticulum-rs` / `lxmf` 0.13.0 (FreeTAKTeam, 2026-10-03, MSRV 1.85).
- **Internet use:** needs a reachable TCP transport node.
- **Assessment:** fascinating for off-grid use; overkill here.
- https://reticulum.network
**10. Yggdrasil.** Encrypted IPv6 overlay mesh in Go. You peer with public nodes over outbound TCP, so it works without WireGuard. Every device gets a stable IPv6 address, and nerve would just bind a socket.
- **Caveat:** it is a global network. You must firewall, because every Yggdrasil node can reach you.
- **Android:** app exists. No Rust crate of note (`yggdrasil` 0.1.0 dates from 2019).
- **Status:** still marked alpha.
- https://yggdrasil-network.github.io
**11. WebRTC datachannels.**
- **Crates:** `webrtc` (webrtc-rs) 0.21.0, 2026-09-19, MSRV unset; `str0m` 0.24.1, 2026-10-03, MSRV 1.85, sans-IO.
- **Connectivity:** ICE/STUN/TURN. Needs a signalling channel (this is where Nostr fits well) and a TURN server for symmetric NAT (coturn runs on a Pi but needs ports).
- **Strengths:** DTLS encryption built in; the **best browser and phone story**, because any mobile browser can join with no app.
- **Weaknesses:** no offline support. You assemble everything yourself.
- https://github.com/algesten/str0m · https://github.com/webrtc-rs/webrtc
**12. magic-wormhole / croc.** One-shot PAKE code-phrase transfers through a mailbox server and a transit relay; both can be self-hosted.
- **Crates:** `magic-wormhole` 0.8.1 (MSRV 1.92). croc is Go only.
- **Fit:** a UX pattern for pairing ("type this code on your phone"), not ongoing sync. Good inspiration for device enrolment.
- https://github.com/magic-wormhole/magic-wormhole.rs
**13. Syncthing.** Go block-level file sync.
- **Connectivity:** global discovery servers plus relays (both can be self-hosted); TLS with device-ID pinning; no inbound ports needed.
- **Mobile:** the Android app was discontinued upstream in late 2024; a community fork, "Syncthing-Fork", continues (unverified current state).
- **Rust:** none for the protocol; the `syncthing` 0.6 crate is only a REST client.
- **Fit:** honestly a viable *blob and transcript* layer. nerve writes snapshots to a folder and Syncthing ships them. Useless for commands or realtime.
- https://docs.syncthing.net
### B. Brokers and messaging
**14. MQTT** (Mosquitto, rumqttd, EMQX, NanoMQ). The classic option for the Pi: Mosquitto runs well there. Every client dials out to the broker, so only the broker needs reachability.
- **Rust:** `rumqttc` 0.25.1 (MSRV 1.64, but last release 2025-11-21, so slowing); `rumqttd` 0.20.0 (2025-09).
- **Strengths:** retained messages give "last-known state per device" for free; QoS 1/2 with persistent sessions gives limited store-and-forward; MQTT-over-WebSocket works in a phone browser; Android clients are plentiful.
- **Weaknesses:** TLS to the broker only, so payloads need your own end-to-end encryption; blobs are awkward (default maximum is about 256 MB, but it is not designed for blobs).
- **Fit:** (a) and (b) excellent; (c) fair; (d) poor.
- https://mosquitto.org · https://github.com/bytebeamio/rumqtt
**15. NATS + JetStream.** A single Go binary that is very Pi-friendly.
- **Leaf nodes:** a laptop can run a local leaf that bridges to the Pi hub when online and keeps working offline.
- **JetStream:** durable streams and consumers (store-and-forward), plus a KV store and an object store for blobs.
- **Rust:** `async-nats` 0.50.0 (2026-07-20, MSRV 1.88), 5.6M recent downloads, official.
- **Mobile/browser:** WebSocket via nats.ws.
- **Security:** TLS plus NKeys/JWT auth; no end-to-end encryption.
- **Assessment:** the most complete single brokered option. (a)(b)(c)(d) are all good; the object store handles transcripts.
- https://docs.nats.io/running-a-nats-service/configuration/leafnodes · https://github.com/nats-io/nats.rs
**16. Zenoh** (Eclipse). Pub/sub/query protocol with peer mode (multicast scouting on a LAN), client mode, and routers that you can chain.
- **Rust:** `zenoh` 1.10.1 (2026-09-07, **MSRV 1.75**). It is written in Rust natively.
- **Storage:** "storages" plugins with RocksDB/filesystem backends.
- **Mobile:** an Android/Kotlin binding exists, with reconnect and declaration restore in recent releases.
- **Connectivity:** routers need a reachable endpoint (or a Tailscale address); TLS/QUIC transports.
- **Assessment:** the best MSRV fit among the strong options. Learning curve is moderate.
- https://zenoh.io · https://github.com/eclipse-zenoh/zenoh-kotlin
**17. Redis Streams / Valkey.** `XADD`/`XREADGROUP` give durable logs with consumer groups; pub/sub is fine for realtime.
- **Rust:** `redis` 1.7.1 (MSRV 1.88).
- **Weaknesses:** no auth model you would expose to the internet, so it must sit behind Tailscale or a tunnel; no browser access without a bridge.
- **Assessment:** fine for the Pi on a tailnet; weak otherwise.
**18. ntfy** (+ UnifiedPush). An HTTP pub/sub notification server in Go; self-hosting on a Pi is trivial. It stores messages with a configurable cache time (store-and-forward) and supports SSE/WebSocket/JSON streams.
- **Android:** the ntfy app is a UnifiedPush distributor, so it gives real **push to the phone without Google**.
- **Encryption:** server-side; ntfy's end-to-end encryption feature is long-planned and not shipped (unverified).
- **Rust:** `ntfy` 0.9.1 (MSRV 1.85; publish client).
- **Fit:** (b) for phone wake-ups ("agent needs approval"). Not a sync layer.
- https://docs.ntfy.sh · https://unifiedpush.org
**19. Matrix.** Federated rooms with Megolm/Olm end-to-end encryption (`vodozemac` 0.11.1), store-and-forward via the homeserver, and excellent mobile clients (Element X).
- **Rust SDK:** `matrix-sdk` 0.19.1 (MSRV **1.96**).
- **Homeservers:**
  - conduwuit was archived in 2025.
  - **Continuwuity** is the community-run successor, currently v26.8.x.
  - **Tuwunel** is another fork with 0.5.x security releases.
  - Both are Rust and light enough for a Pi.
- **Device verification:** cross-signing, so you get end-to-end encryption to your own devices out of the box.
- **Assessment:** the "free phone UI" option. A bot posts progress to a room, and you reply `/approve 3`. Heavy SDK; federation adds attack surface (you can disable it).
- https://continuwuity.org · https://github.com/matrix-construct/tuwunel
**20. XMPP.** Mature; MUC and PubSub (XEP-0060), OMEMO end-to-end encryption, Prosody/ejabberd run on a Pi, and Android has Conversations.
- **Rust:** `tokio-xmpp` 6.0.0 / `xmpp` 0.7.0 (2026-06). Usable, but no OMEMO in Rust that I could confirm.
- **Assessment:** a phone-UI path like Matrix, with lighter servers and a weaker Rust story.
**21. SimpleX.** No user IDs; SMP relays (self-hostable) with store-and-forward and double-ratchet end-to-end encryption; good Android app.
- **Rust:** none of substance (`simplex-chat` 0.2.0, 2024; the core is Haskell). A bot needs the CLI over WebSocket.
- **Assessment:** privacy-excellent but integration-awkward.
- https://simplex.chat/docs/server.html
**22. Briar.** Tor/Bluetooth/Wi-Fi P2P messenger on Android; Briar Mailbox offers store-and-forward.
- **Assessment:** no Rust; Android-centric. Not a fit.
**23. Secure Scuttlebutt.** Append-only signed feeds with gossip via pubs. The ecosystem is largely dormant; `kuska-ssb` was last released 2022. **Not recommended.**
**24. Keet.** Holepunch's consumer app. It is not a library; you would use the Holepunch stack instead (#6).
**25. Signal protocol libraries.** `libsignal` (Signal's official, AGPL, not on crates.io; consumed via git). `vodozemac` is a better-maintained Olm/Megolm alternative. The `presage` Signal client (bot via linked device) is fragile and against Signal's terms in spirit.
- **Assessment:** only relevant for adding end-to-end encryption on top of a broker, where MLS (OpenMLS) or simple age/X25519 sealed boxes are simpler.
### C. CRDT and local-first sync
**26. Automerge.** JSON-like CRDT with compact binary history.
- **Crates:** `automerge` 0.12.0 (MSRV 1.90); `samod` 0.15.0 (2026-09-30), a Rust repo compatible with JS automerge-repo. `automerge-repo` 0.3 is stale; use samod.
- **Sync server:** a sync server exists (JS). The protocol is transport-agnostic and runs over WebSocket or iroh.
- **Fit:** (c) excellent for session metadata and job queues. History grows for append-heavy logs like transcripts, so keep those out.
- https://automerge.org · https://github.com/alexjg/samod
**27. Yjs / y-crdt.** `yrs` 0.28.0 (2026-09-17), 2M downloads, a JS-compatible port. `y-sweet` 0.9.1 is a standalone Rust Yjs server (2025-09, slowing).
- **Assessment:** best browser interop. Fit like Automerge.
**28. Loro.** Rust-native CRDT (text, list, map, tree, movable list) with a fast snapshot format.
- **Crates:** `loro` 1.16.2 (2026-09-21); `loro-protocol` 0.3 for sync.
- **Mobile:** Swift/Kotlin bindings via FFI (unverified depth).
- **Assessment:** strong technically; the sync-server story is younger.
**29. cr-sqlite / Corrosion.** cr-sqlite adds CRDT columns to SQLite. Its development slowed after the author joined Rocicorp (unverified current status), and it is not on crates.io as a crate.
- **Corrosion** (fly.io) is gossip-replicated SQLite built on cr-sqlite with SWIM/QUIC. It is designed for datacentre clusters; there is no mobile story. **Not a fit.**
- https://github.com/superfly/corrosion
**30. Electric SQL.** Postgres → shape streams over HTTP to clients. Needs Postgres (possible on a Pi, but heavy). Read-path sync only; you handle writes. Not a fit for a Rust daemon.
**31. PowerSync.** Postgres/Mongo/MySQL → SQLite on clients, with a self-hostable service.
- **Rust:** a new **experimental** `powersync` 0.1.0 (2026-09-28).
- **Assessment:** heavy for three personal devices.
**32. Ditto.** Commercial P2P database (BLE, LAN, Wi-Fi Aware, cloud) with excellent mobile SDKs.
- **Rust:** `dittolive-ditto` 5.1.0 (2026-10-08, MSRV 1.91).
- **Assessment:** licensing and pricing for a personal OSS tool is the blocker; closed source.
**33. Jazz / Triplit / Replicache / Zero.** All TypeScript-first.
- Jazz: `jazz-tools` 2.0.0-alpha on crates.io (Jazz 2 is moving to a Rust core).
- Triplit: company wound down in 2025 and open-sourced (unverified).
- Replicache is in maintenance in favour of Zero, which is Postgres-based.
- **Assessment:** none fit a Rust daemon today. Watch Jazz 2.
**34. CouchDB / PouchDB.** Battle-tested multi-master HTTP replication with offline support. CouchDB runs on a Pi, and PouchDB runs in a phone browser. Rust clients are thin (CouchDB is just HTTP). Revision-tree conflicts are manual.
- **Assessment:** viable for durable state; dated.
**35. Turso / libSQL embedded replicas.** `libsql` 0.9.30 (2026-06-02). Embedded replicas sync from a primary (sqld can be self-hosted). Turso is pivoting to its Rust rewrite ("Turso Database") with its own sync (unverified current state).
- **Assessment:** single-writer primary; OK for a Pi-hub model.
**36. Litestream / rqlite.**
- Litestream streams SQLite to S3/SFTP for backup and DR, not multi-device sync.
- rqlite is Raft SQLite over HTTP (`rqlite-rs` 0.6.2) and needs a quorum, which is wrong for intermittently connected laptops and phones.
- **Assessment:** neither fits.
### D. Overlay networks
**37. Tailscale** (+ tsnet, Serve, Funnel, Taildrop / `tailscale file`).
- **Connectivity:** WireGuard mesh, NAT traversal via DERP relays (no inbound ports), ACLs, MagicDNS.
- **Serve and Funnel:** Serve exposes `localhost:PORT` to the tailnet with automatic HTTPS. Funnel exposes it to the public internet.
- **Taildrop:** `tailscale file cp/get` moves blobs between devices.
- **Mobile:** excellent Android app.
- **Rust:**
  - `tailscale` 0.6.1 is the **official tailscale-rs**: "preview, experimental", monthly pre-1.0 releases, MSRV 1.92, talks to peers through DERP. Direct connections are in forks (unverified for upstream).
  - `libtailscale` 0.2.2 binds the Go tsnet library through C, which adds a Go toolchain to the build.
- **Assessment:** the simplest "everything just works" baseline. Nerve exposes plain HTTP + SSE/WebSocket on the tailnet.
- https://github.com/tailscale/tailscale-rs · https://tailscale.com/kb/1312/serve
**38. Headscale / NetBird / ZeroTier / Nebula / innernet.**
- **Headscale:** self-hosted Tailscale control server; it runs on a Pi, but the Pi then needs a public endpoint.
- **NetBird:** WireGuard mesh with a self-hostable management server, TURN, and an Android app.
- **ZeroTier:** its own L2 protocol rather than WireGuard; libzt SDK, root-server based, Android app.
- **Nebula:** Slack's certificate-based mesh. Needs a "lighthouse" with a public UDP port; mobile via Defined Networking's app.
- **innernet:** Rust and WireGuard, Linux/macOS only, no mobile, and it needs a reachable coordinator.
- **Assessment:** all deliver "IP connectivity, then use HTTP". None removes the need for *some* reachable node.
**39. Tunnels: Cloudflare Tunnel / ngrok / bore / frp.** Outbound-only tunnels publish the Pi's HTTP service on a public hostname.
- Cloudflare Tunnel is free; add Cloudflare Access for authentication. It terminates TLS at Cloudflare, so it is **not end-to-end encrypted**.
- ngrok has a free tier with limits.
- `bore-cli` 0.6.0 (Rust) and frp (Go) need your own VPS.
- **Assessment:** gives the phone a URL with no app installed. Pair it with application-layer encryption.
### E. Hosted realtime
**40. Cloudflare Durable Objects / PartyKit.** One stateful actor per room with WebSocket hibernation and built-in SQLite storage; PartyKit now lives inside Cloudflare.
- **Connectivity:** a Rust daemon connects out over WebSocket (`tokio-tungstenite`, MSRV 1.85).
- **Cost:** realistically about $0–5 a month for this volume.
- **Encryption:** not end-to-end unless you encrypt payloads.
- **Assessment:** a good "zero infra on the Pi" relay and mailbox, written in TypeScript or as a Rust→WASM worker (`worker` crate).
**41. Firebase RTDB / Supabase Realtime / PocketBase.**
- **Firebase:** FCM is the most reliable Android push. RTDB is fine, but Rust clients are unofficial (`firebase-rs`).
- **Supabase Realtime:** Postgres plus broadcast channels; self-hosting is heavy.
- **PocketBase:** a single Go binary with SQLite and SSE realtime that runs perfectly on a Pi, plus an admin UI. Rust SDK is stale (`pocketbase-sdk` 0.1.1, 2023), but the REST/SSE API is trivial to call.
- **Assessment:** PocketBase is a dark-horse "Pi hub" choice.
**42. Ably / Pusher.** Hosted pub/sub with history and presence; Rust SDKs are unofficial or limited. Listed for completeness; vendor lock-in for a personal tool.
### F. Agent-specific protocols
**43. A2A** (Agent2Agent).
- **Status:** v1.0 in March 2026 (1.0.1 in May); moved into the Linux Foundation's Agentic AI Foundation on 2026-08-17. IBM's older "ACP" (Agent Communication Protocol) was merged into A2A in 2025.
- **Protocol:** JSON-RPC/gRPC/REST over HTTP; Agent Cards (now signed) for capability discovery; tasks with streaming (SSE) and push notifications.
- **Rust:** `a2a-rs` 0.10.0 (community, MSRV 1.96).
- **Relevance:** the **Agent Card + task lifecycle model maps directly onto "job distribution by capability"**. It solves no NAT problem; it assumes HTTP reachability.
- https://a2a-protocol.org
**44. MCP** (streamable HTTP / remote MCP). `rmcp` 3.5.1 (official, MSRV 1.88). nerve could expose an MCP server ("list sessions", "send instruction", "get diff"), so *a Claude on device Y can steer device X*. Elicitation covers question prompts. It is a tool interface, not a sync layer, and it needs reachability (tailnet or tunnel).
**45. ContextVM.** MCP carried over Nostr events; `contextvm-sdk` 0.2.2 (MSRV 1.88). This is the natural bridge if the Nostr design wins: nerve as an MCP server reachable over relays with no ports. Young (226 downloads).
**46. ACP** (Agent Client Protocol, Zed). Editor ↔ agent JSON-RPC over stdio; `agent-client-protocol` 3.3.0 (released today). It is local, not network. **Relevant as the *session-control vocabulary*:** prompts, permission requests, tool-call updates and diffs. That is exactly nerve's command set, and could be tunnelled over any transport above.
- https://agentclientprotocol.com
**47. AG-UI.** Agent ↔ frontend event stream (SSE/WebSocket) with event types for text deltas, tool calls, state snapshots and deltas. `ag-ui` 0.4.5 (community, MSRV 1.85). A ready-made **schema for the progress stream to the phone UI**.
**48. Agent Network Protocol (ANP).** DID-based (did:wba) identity plus meta-protocol negotiation for open-internet agents. Mostly Python, no Rust crate, early. **Not relevant** for a single-owner fleet.
---
## 2. Comparison table
Key:
- **Inbound** = does at least one node need a reachable port?
- **S&F** = store-and-forward (offline delivery).
- **Fit columns:** a = realtime stream, b = commands, c = durable state, d = blobs. ●●● strong, ●● OK, ● weak, – none.
| # | Option | Inbound? | E2EE | S&F | Rust crate (ver / MSRV) | Phone | Pi host | a | b | c | d |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | iroh + n0 relays | No (443 relay) | Yes (QUIC/TLS, key-pinned) | No | iroh 1.3.0 / 1.91 | FFI (?) | relay needs port | ●●● | ●●● | – | – |
| 2 | iroh-gossip | as iroh | Yes | No | 0.101 / 1.91 | as iroh | – | ●●● | ●● | – | – |
| 3 | iroh-docs | as iroh | Yes | peers only | 0.101 / 1.91 | as iroh | yes | ● | ●● | ●●● | ●● |
| 4 | iroh-blobs | as iroh | Yes | peers only | 0.103.1 / 1.91 | as iroh | yes | – | – | ● | ●●● |
| 5 | rust-libp2p | Relay needs port | Yes (Noise) | No | 0.57 / 1.88 | DIY | yes | ●●● | ●● | ● | ●● |
| 6 | Holepunch | No (DHT) | Yes | via seeders | hypercore 0.16 (JS-first) | Bare ●●● | yes | ●● | ●● | ●●● | ●●● |
| 7 | Veilid | No | Yes | DHT values | 0.5.7 / 1.89 | Flutter | yes | ● | ●● | ●● | ● |
| 8 | Willow | BYO | caps + BYO | peers | willow25 0.7.9 | none | yes | – | ● | ●●● | ●● |
| 9 | Reticulum / LXMF | TCP node | Yes | LXMF prop. nodes | lxmf 0.13 / 1.85 | Sideband | yes | ● | ●● | ● | ● |
| 10 | Yggdrasil | No (peer out) | Yes (link) | No | none | app | yes | n/a (IP layer) | | | |
| 11 | WebRTC (str0m) | TURN for sym-NAT | Yes (DTLS) | No | str0m 0.24.1 / 1.85 | browser ●●● | coturn | ●●● | ●●● | – | ●● |
| 12 | magic-wormhole | No (mailbox) | Yes (PAKE) | short | 0.8.1 / 1.92 | apps | yes | – | ● | – | ●● |
| 13 | Syncthing | No (relays) | Yes (TLS pinned) | peers | none (REST only) | fork | yes | – | – | ●● | ●●● |
| 14 | MQTT / Mosquitto | Broker | No (TLS only) | QoS / retained | rumqttc 0.25.1 / 1.64 | ●●● | ●●● | ●●● | ●●● | ●● | ● |
| 15 | NATS + JetStream | Hub (or leaf) | No | ●●● | async-nats 0.50 / 1.88 | ws | ●●● | ●●● | ●●● | ●●● | ●● |
| 16 | Zenoh | Router | No (TLS) | storages | zenoh 1.10.1 / 1.75 | Kotlin | ●●● | ●●● | ●●● | ●● | ●● |
| 17 | Redis / Valkey Streams | Yes (tailnet) | No | ●● | redis 1.7.1 / 1.88 | no | ●●● | ●● | ●● | ●● | ● |
| 18 | ntfy + UnifiedPush | Server | No | cache | ntfy 0.9.1 / 1.85 | push ●●● | ●●● | ● | ●● | – | – |
| 19 | Matrix (Continuwuity / Tuwunel) | Homeserver | Yes (Megolm) | ●●● | matrix-sdk 0.19.1 / 1.96 | Element ●●● | ●● | ●● | ●●● | ●● | ●● |
| 20 | XMPP | Server | OMEMO (not Rust) | ●●● | tokio-xmpp 6.0 | Conversations | ●●● | ●● | ●● | ● | ● |
| 21 | SimpleX | Relay | Yes | ●●● | none | ●●● | ●● | ● | ●● | – | ● |
| 22 | Briar | No | Yes | Mailbox | none | Android | – | – | ● | – | – |
| 23 | SSB | Pub | Yes | ●● | kuska-ssb (2022) | dead | – | – | ● | ● | – |
| 25 | libsignal / vodozemac / OpenMLS | n/a | Yes | n/a | vodozemac 0.11.1 / 1.89 | – | – | E2EE add-on | | | |
| 26 | Automerge + samod | BYO | BYO | via server | automerge 0.12 / 1.90 | JS | ●● | ● | ●● | ●●● | ● |
| 27 | Yjs (yrs / y-sweet) | Server | No | server | yrs 0.28 | JS ●●● | ●● | ●● | ● | ●●● | ● |
| 28 | Loro | BYO | BYO | BYO | loro 1.16.2 | FFI | – | ● | ● | ●●● | ● |
| 29 | cr-sqlite / Corrosion | Cluster | No | ● | n/a | no | ● | – | – | ●● | – |
| 30–31 | Electric / PowerSync | Server + PG | No | ●● | powersync 0.1 (exp) | ●● | heavy | ● | – | ●●● | ● |
| 32 | Ditto | No (P2P + cloud) | Yes | ●● | dittolive-ditto 5.1 / 1.91 | ●●● | n/a | ●● | ●● | ●●● | ●● |
| 33 | Jazz / Triplit / Zero | Server | varies | ●● | alpha / none | JS | ● | ● | ● | ●●● | ● |
| 34 | CouchDB / PouchDB | Server | No | ●●● | HTTP | Pouch | ●● | ● | ● | ●●● | ●● |
| 35 | libSQL replicas | Primary | No | ●● | libsql 0.9.30 | ● | ●● | – | – | ●● | ● |
| 36 | Litestream / rqlite | — | — | — | rqlite-rs 0.6.2 | — | ●● | – | – | ● | – |
| 37 | Tailscale (+Serve/Funnel/Taildrop) | No (DERP) | Yes (WG) | Taildrop queues | tailscale 0.6.1 / 1.92 (exp.) | ●●● | ●●● | transport | | | ●● |
| 38 | Headscale / NetBird / ZeroTier / Nebula / innernet | coordinator / lighthouse | Yes | No | mostly none | varies | ●● | transport | | | |
| 39 | CF Tunnel / ngrok / bore / frp | No (outbound) | No (TLS ends at edge) | No | bore-cli 0.6 | browser | ●●● | transport | | | |
| 40 | Durable Objects / PartyKit | No (hosted) | No | ●●● (DO SQLite) | tungstenite / `worker` | browser | n/a | ●●● | ●●● | ●● | ● |
| 41 | PocketBase / Supabase / Firebase | Server | No | ●●● | REST | ●● / FCM ●●● | PB ●●● | ●● | ●● | ●●● | ●● |
| 42 | Ably / Pusher | No | opt. | history | unofficial | ●● | n/a | ●●● | ●● | ● | – |
| 43 | A2A | needs HTTP | No | push-notif cfg | a2a-rs 0.10 / 1.96 | – | – | ●● | ●●● (tasks) | ● | ● |
| 44 | MCP (remote) | needs HTTP | No | No | rmcp 3.5.1 / 1.88 | via Claude | – | ● | ●●● | – | ● |
| 45 | ContextVM | No (Nostr) | NIP-44 | relay | contextvm-sdk 0.2.2 / 1.88 | Nostr clients | relay | ●● | ●●● | ● | ● |
| 46 | ACP (Zed) | stdio (local) | n/a | n/a | agent-client-protocol 3.3 / 1.88 | – | – | schema | schema | | |
| 47 | AG-UI | needs HTTP | No | No | ag-ui 0.4.5 / 1.85 | web | – | schema ●●● | ● | ● | – |
| 48 | ANP | HTTP | DID | No | none | – | – | – | ● | – | – |
| — | Claude Code Remote Control | No (outbound 443) | TLS via Anthropic | yes | n/a (built in) | official app ●●● | n/a | ●●● | ●●● | – | ● |
Rows 24 (Keet) is omitted from the table because it is an app, not a library; see #6.
---
## 3. Top-6 shortlist
The target is a Rust TUI daemon on Mac and Pi, reachable from a phone.
**1. iroh (+ iroh-blobs, optionally iroh-gossip / iroh-docs).**
- **For:** the best pure-P2P story in Rust. Key-addressed, hole punching, relay fallback over 443, no inbound ports, end-to-end encrypted, works with or without Tailscale, 1.0 API stability, and content-addressed blobs that are perfect for session handoff (transcript + `git bundle` + diff).
- **Costs:** MSRV 1.91. No store-and-forward, so a sleeping laptop misses commands unless the Pi acts as an always-on peer. The public-relay version treadmill. The phone needs a native app using FFI, or a relay-only browser build.
**2. NATS + JetStream with the hub on the Pi.**
- **For:** one Go binary solves realtime, commands, durable log, KV ("device X last synced at T") and the object store. Leaf nodes let the Mac keep working offline and catch up. `async-nats` is official and heavily used.
- **Costs:** the Pi must be reachable (Tailscale, Cloudflare Tunnel or a port forward). No end-to-end encryption; add age or X25519 sealing if the hub is exposed beyond the tailnet. MSRV 1.88.
**3. Zenoh.**
- **For:** Rust-native, MSRV 1.75 (fits nerve today), peer mode on a LAN with no infrastructure, a router on the Pi for WAN, query/queryable fits "get diff for session S" naturally, storages give durable state, and there is a Kotlin binding for Android.
- **Costs:** more conceptual surface than MQTT. A WAN router still needs reachability. Smaller community than NATS.
**4. Tailscale + plain HTTP (axum) with SSE/WebSocket and an AG-UI-shaped event schema.**
- **For:** the lowest-effort design that reliably works. Each nerve daemon serves `/events` (SSE) and `/cmd` on its tailnet IP; `tailscale serve` gives HTTPS; the phone opens a PWA. Taildrop or HTTP handles blobs. MSRV-friendly (axum 1.80, tungstenite 1.85).
- **Costs:** fails the "without WireGuard mesh" goal. Tailscale's control plane is a third party unless you run Headscale. Embedding via tailscale-rs is experimental, so in practice you depend on the system tailscaled.
**5. Matrix (Continuwuity or Tuwunel on the Pi, Element X on the phone).**
- **For:** you get a polished, end-to-end encrypted phone UI, push notifications and store-and-forward with **zero mobile development**. One room per session; threads for subagents; commands as messages or reactions; diffs as file attachments.
- **Costs:** matrix-sdk MSRV 1.96 and a large dependency tree (isolate it in a sidecar binary). The homeserver needs reachability. Chat is a clumsy UI for structured state.
**6. MQTT (Mosquitto on the Pi) + ntfy/UnifiedPush.**
- **For:** the boring, proven option. Retained topics give per-device status, QoS 1 persistent sessions queue commands for sleeping devices, WebSocket gives the phone a browser view, and `rumqttc` fits MSRV.
- **Costs:** blobs are poor (use HTTP or Syncthing alongside). End-to-end encryption is DIY. rumqttc releases have slowed (last 2025-11).
**Honourable mentions:**
- **WebRTC/str0m:** the best browser and phone P2P, but you must build signalling (Nostr could do it) and TURN.
- **Syncthing:** as a zero-code blob and handoff layer.
- **Cloudflare Durable Objects:** a no-Pi-infrastructure mailbox.
- **Automerge (samod):** if you want a CRDT job board.
---
## 4. Recommended layered designs
Treat the problem as four planes:
1. **Identity and pairing**
2. **Signalling, commands and push**
3. **Realtime stream**
4. **Bulk and durable state**
Then pick per plane. Whatever transport you choose, use **ACP's vocabulary** for session control (prompt, permission request and response, tool-call update, diff) and **AG-UI-style events** for the progress stream. That keeps the transport swappable and gives the phone UI a known schema.
**Design A: "Nostr for signalling + iroh for bulk."** Strongest no-mesh, no-port design.
- Nostr carries:
  - small encrypted control events (NIP-44/NIP-17), giving relay store-and-forward for commands to sleeping devices;
  - device presence and "last synced" heartbeats (replaceable events);
  - iroh endpoint IDs and tickets.
- When two devices need to talk live (streaming a transcript, handoff), they exchange an iroh `NodeAddr` over Nostr and open a QUIC connection. iroh-blobs moves `git bundle` + transcript JSONL + diffs with BLAKE3 verification.
- Optionally ContextVM to expose nerve as an MCP server over the same relays.
- Run your own iroh-relay (and Nostr relay, for example `nostr-rs-relay` or strfry) on a small VPS, or on the Pi behind a Cloudflare Tunnel. A tunnel works for the WebSocket-based Nostr relay; for iroh-relay, check that QUIC address discovery degrades gracefully.
- Phone: a Nostr client for commands (Amethyst/0xchat-style DMs work out of the box), plus a native app later for rich views.
- **Assessment:** best long-term architecture. Highest build cost. Needs a separate MSRV-1.91 crate.
**Design B: "NATS on the Pi."** Most features per line of code.
- Pi runs `nats-server` with JetStream. Mac runs a leaf node (offline-tolerant).
- Subjects:
  - `nerve.<device>.sessions.<id>.events` → stream with limits
  - `nerve.<device>.cmd` → work-queue stream
  - KV bucket `devices` → last-synced and capabilities
  - object store `handoff` → bundles
- Job distribution: devices publish capabilities into KV (`ios_sim=true`); jobs go to `nerve.jobs.<capability>` with queue groups. This is A2A-Agent-Card semantics in miniature.
- Reachability: Tailscale today; Cloudflare Tunnel (WebSocket listener) for the no-mesh case. Add payload sealing.
- Phone: a PWA over nats.ws, plus ntfy for push.
**Design C: "Tailscale + HTTP/SSE"** (ship-it-this-week baseline).
- Each daemon exposes axum `/v1/sessions`, `/v1/events` (SSE, AG-UI-shaped), `/v1/cmd`, `/v1/blobs/:hash`.
- The phone uses a PWA over `tailscale serve`.
- "Last synced" is kept by having each daemon poll its peers.
- Fits MSRV 1.85 with no new heavy dependencies.
- It is also the natural *local API* that designs A and B wrap. Build it first regardless.
**Design D: "Remote Control for steering, nerve for the fleet."**
- Rely on Claude Code Remote Control for phone chat and approvals; it already handles prompts, questions and subagent progress on the official app.
- nerve adds only what Remote Control lacks: the cross-device dashboard, handoff (bundle + transcript copy + `claude --resume` on the target), capability-based job placement, and diff browsing.
- This shrinks the custom command surface a lot. The downside is dependence on Anthropic and on a subscription (no API-key auth).
**Suggested path:** C first (local API + schema) → D (integrate Remote Control) → A or B for the no-mesh WAN layer. Choose B if you are happy running the Pi as a hub; choose A if you want no hub and end-to-end encryption.
**Handoff specifics, independent of transport:**
- Use `git bundle create` covering `HEAD` plus `git stash create` for the working tree (or a `git diff --binary` patch), plus the session's JSONL transcript from the source device's `claude_roots()` dir.
- On the target, rewrite the project path, because the transcript dir is keyed by the encoded cwd.
- Hash-address everything (iroh-blobs or NATS object store).
---
## 5. Risks and uncertainties
1. **MSRV.** Almost every serious option needs Rust 1.88–1.96 (table above). Decide now between a separate sync crate or binary, or an MSRV bump. Keeping the TUI at 1.85 while a `nerve-sync` sidecar floats is the cleanest option.
2. **Injecting input into a running Claude Code session is the real hard part, not transport.** Answering permission prompts remotely needs either:
   - Remote Control;
   - hooks (a `PermissionRequest`/`PreToolUse` hook that blocks on nerve's channel);
   - tmux `send-keys` (fragile; impossible for Ghostty tabs without AppleScript keystrokes);
   - the Agent SDK.
   Validate this before choosing a network stack.
3. **Security.** A remote "send instruction to agent" channel is remote code execution on your machine. Requirements:
   - per-device keys and explicit pairing (wormhole-style code);
   - signed commands with replay protection (nonce plus expiry);
   - an allow-list of command types;
   - re-validating the PID (per CLAUDE.md) before acting.
   Brokers without end-to-end encryption (MQTT, NATS, Redis, Cloudflare) need payload sealing if anything leaves the tailnet.
4. **Third-party relay dependence:**
   - n0 public relays are rate-limited with no SLA, and drop old versions (0.9x/rc relays scheduled off on 2026-09-30, 0.35x on 2026-12-31).
   - Tailscale's control plane is third party unless you run Headscale.
   - Cloudflare terminates TLS at its edge.
   - Remote Control routes through Anthropic.
5. **Mobile background limits.** Android kills background sockets. Any realtime design still needs push for the "agent is waiting on you" case: FCM, or UnifiedPush via ntfy, or Matrix push. P2P options (iroh, libp2p, Holepunch) do not solve this on their own.
6. **Pi reachability.** Every hub design (NATS, Zenoh router, MQTT, Matrix, Headscale, self-hosted iroh-relay) needs the Pi reachable. Behind CGNAT, that means Tailscale, Cloudflare Tunnel or a VPS. Only iroh with public relays, Holepunch, Nostr with public relays, Yggdrasil and Remote Control avoid it.
7. **Maturity flags:**
   - tailscale-rs is explicitly experimental.
   - iroh-gossip and iroh-docs are pre-1.0.
   - samod, willow25, the Rust hyperswarm, the reticulum ports, ContextVM, ag-ui and a2a-rs all have small download counts.
   - rumqttc's release cadence has slowed.
   - conduwuit is archived; prefer Continuwuity or Tuwunel.
   - Statements marked "(unverified)" above (iroh mobile FFI depth, ntfy end-to-end encryption, Syncthing Android fork, cr-sqlite status, Triplit, Turso's rewrite) should be checked before committing.
8. **Transcript size and privacy.** JSONL transcripts can be many MB and contain secrets. Blob layers must be encrypted at rest on any hub, with retention limits.
---
## Sources
- Claude Code Remote Control: https://code.claude.com/docs/en/remote-control
- crates.io API (versions, dates, MSRV, queried 2026-10-09): https://crates.io/api/v1/crates/
- iroh 1.0: https://www.iroh.computer/blog/v1
- iroh relays: https://docs.iroh.computer/concepts/relays
- iroh public relays: https://docs.iroh.computer/iroh-services/relays/public
- iroh custom relays: https://docs.iroh.computer/connecting/custom-relays
- iroh repo: https://github.com/n0-computer/iroh
- iroh 1.0 coverage: https://www.techtimes.com/articles/318490/20260616/peer-peer-library-iroh-10-ships-dial-devices-key-not-ip-address.htm
- tailscale-rs: https://github.com/tailscale/tailscale-rs
- Tailscale Serve: https://tailscale.com/kb/1312/serve
- rust-libp2p: https://github.com/libp2p/rust-libp2p
- Holepunch / Pear docs: https://docs.pears.com
- Rust hypercore: https://github.com/datrs/hypercore
- Rust hyperswarm: https://github.com/cowlicks/hyperswarm
- Veilid: https://veilid.com
- Willow: https://willowprotocol.org
- willow_rs: https://codeberg.org/worm-blossom/willow_rs
- Reticulum: https://reticulum.network
- Reticulum-rs (Beechat): https://github.com/BeechatNetworkSystemsLtd/Reticulum-rs
- LXMF-rs: https://github.com/FreeTAKTeam/LXMF-rs
- Yggdrasil: https://yggdrasil-network.github.io
- str0m: https://github.com/algesten/str0m
- webrtc-rs: https://github.com/webrtc-rs/webrtc
- magic-wormhole.rs: https://github.com/magic-wormhole/magic-wormhole.rs
- Syncthing docs: https://docs.syncthing.net
- Mosquitto: https://mosquitto.org
- rumqtt: https://github.com/bytebeamio/rumqtt
- NATS leaf nodes: https://docs.nats.io/running-a-nats-service/configuration/leafnodes
- nats.rs: https://github.com/nats-io/nats.rs
- Zenoh: https://zenoh.io
- zenoh-kotlin: https://github.com/eclipse-zenoh/zenoh-kotlin
- ntfy: https://docs.ntfy.sh
- UnifiedPush: https://unifiedpush.org
- Continuwuity: https://continuwuity.org
- Tuwunel: https://github.com/matrix-construct/tuwunel
- SimpleX server docs: https://simplex.chat/docs/server.html
- Automerge: https://automerge.org
- samod: https://github.com/alexjg/samod
- y-sweet: https://github.com/drifting-in-space/y-sweet
- Loro sync protocol: https://github.com/loro-dev/protocol
- Corrosion: https://github.com/superfly/corrosion
- PowerSync native SDK: https://github.com/powersync-ja/powersync-native
- Jazz 2: https://github.com/garden-co/jazz2
- A2A protocol: https://a2a-protocol.org
- A2A / ACP history: https://zuplo.com/blog/agent-protocol-stack-mcp-a2a-acp-2026
- A2A joins AAIF: https://nerdleveltech.com/a2a-protocol-joins-agentic-ai-foundation
- a2a-rs: https://github.com/emillindfors/a2a-rs
- Agent Client Protocol: https://agentclientprotocol.com
- AG-UI Rust SDK: https://github.com/KimSoungRyoul/ag-ui-rust
- ContextVM Rust SDK: https://github.com/ContextVM/rs-sdk
- Rust ntfy client: https://github.com/shadowylab/ntfy
