# Nostr as a device-sync transport for nerve: research report

*Researched 2026-10-09. I checked spec text against the raw NIP files. I queried crate versions from the crates.io API and relay limits from live NIP-11 documents (`curl -H 'Accept: application/nostr+json'`). Items I could not verify are marked **[UNVERIFIED]**.*

## 0. Bottom line

- **Yes, Nostr can carry nerve's device sync without a WireGuard or Tailscale mesh.** Every peer opens an outbound WSS connection to one or more relays. No device needs inbound reachability, port forwarding or NAT hole-punching.
- **The protocol primitives match the use case closely:**
  - replaceable and addressable events for "current state";
  - ephemeral events for live progress;
  - NIP-44 and NIP-59 for encrypted commands;
  - NIP-40 for TTL;
  - NIP-42 and NIP-70 for access control.
- **Someone has already built this.** CodeDeck+ is a phone controller for Claude Code. It runs over Nostr relays with NIP-44 and uses almost exactly the kind-class split recommended here (details in §4).
- **There are three real constraints:**
  1. **Event size.** Public relays cap messages at roughly 128 KB to 1 MB. strfry's default `maxEventSize` is 64 KiB. Large diffs therefore need chunking or encrypted Blossom blobs.
  2. **No atomicity.** Relays give no compare-and-swap or ordering guarantee, so "job claim" needs a deterministic tie-break or a coordinator.
  3. **Metadata.** Relays see IPs, timestamps, kinds and (unless you gift-wrap) pubkeys.
- **iOS background delivery is the weakest link.** It needs an APNs bridge. Android can hold a socket open (Amethyst, Pokey).

---

## 1. Relevant NIPs: exact semantics

### 1.1 NIP-01: kind classes ([01.md](https://github.com/nostr-protocol/nips/blob/master/01.md))

| Class | Range | Relay semantics |
|---|---|---|
| Regular | `1000 ≤ n < 10000`, `4 ≤ n < 45`, `1`, `2` | Expected to be stored |
| Replaceable | `10000 ≤ n < 20000`, `0`, `3` | Only the latest per (pubkey, kind) MUST be kept |
| Ephemeral | `20000 ≤ n < 30000` | Not expected to be stored |
| Addressable | `30000 ≤ n < 40000` | Only the latest per (kind, pubkey, `d` tag) MUST be kept |

- **Ties:** when two events have the same `created_at`, the one with the lowest id (lexical order) is retained.
- **Messages:** `EVENT` / `REQ` / `CLOSE` from the client; `EVENT` / `OK` / `EOSE` / `CLOSED` / `NOTICE` from the relay.
- **Acknowledgement:** relays MUST answer every `EVENT` with `OK`. That gives nerve a per-relay write ack.
- **Subscription IDs:** at most 64 characters.
- **Filters:** conditions within one filter are ANDed; multiple filters in one `REQ` are ORed. `limit: 0` means "live only, no stored events". Only single-letter tags are indexed, and only their first value.

These are the building blocks for nerve. Use `#d`, `#p` and a custom single-letter tag (for example `#s` for session id) for routing.

### 1.2 NIP-09: deletion ([09.md](https://github.com/nostr-protocol/nips/blob/master/09.md))

- Kind 5 carries `e` and/or `a` tags plus `k` tags.
- Relays SHOULD delete referenced events that have the same pubkey. For an `a` tag they delete all versions up to the deletion's `created_at`.
- Deletion is advisory. Other relays and clients may still hold copies.
- NIP-90 reuses kind 5 to cancel a job.

### 1.3 NIP-40: expiration ([40.md](https://github.com/nostr-protocol/nips/blob/master/40.md))

- The `["expiration", "<unix>"]` tag tells relays they SHOULD NOT serve the event after that time and SHOULD reject it if it is already expired on arrival.
- Relays MAY keep it on disk indefinitely. The spec says explicitly that this is not a security feature.
- Clients should send it only to relays that list 40 in `supported_nips`. Every public relay I probed lists 40 (see §3).

### 1.4 NIP-42: relay AUTH ([42.md](https://github.com/nostr-protocol/nips/blob/master/42.md))

- The relay sends `["AUTH", challenge]`. The client replies with a signed kind 22242 event carrying `relay` and `challenge` tags, timestamped within about 10 minutes.
- Relays MUST NOT broadcast kind 22242.
- Rejections use the `auth-required:` and `restricted:` prefixes on `OK` or `CLOSED`.
- AUTH is what lets a relay serve your events only to you.

### 1.5 NIP-44 v2: encryption ([44.md](https://github.com/nostr-protocol/nips/blob/master/44.md))

- **Construction:**
  - secp256k1 ECDH, using the unhashed x-coordinate;
  - HKDF-SHA256 with salt `nip44-v2`;
  - a random 32-byte nonce per message;
  - ChaCha20;
  - HMAC-SHA256 over nonce ‖ ciphertext;
  - base64 encoding of `version ‖ nonce ‖ ct ‖ mac`.
- **Padding:** power-of-two-based buckets, minimum 32 bytes.
- **Audit:** Cure53, December 2023.

**Correction to the brief's assumption about the 65535-byte maximum.** The current spec text says plaintext may be up to 4,294,967,295 bytes. A 2-byte u16 prefix is used below 65536. At 65536 and above, an extended prefix applies (two zero bytes, then a u32), and test vectors cover lengths 65535, 65536 and 65537. The original 2023 spec capped plaintext at 65535. **[UNVERIFIED]** I did not check whether rust-nostr 0.45 and other implementations accept the extended prefix. Plan for 64 KiB per encrypted payload regardless, because relay size limits bind first.

**What NIP-44 does not provide:**
- forward secrecy or post-compromise security;
- deniability;
- metadata protection (`created_at`, IPs and sizes are visible, sizes only partly padded).

**Security note.** [RUSTSEC-2026-0216](https://rustsec.org/advisories/RUSTSEC-2026-0216.html) is a remote DoS (a panic) in `nostr::nip44::decrypt` when a crafted payload decrypts to 0 or 1 bytes. Affected versions are `<0.44.5` and `0.45.0-alpha.1` through `alpha.4`. The current 0.45.x is patched.

### 1.6 NIP-59: gift wrap ([59.md](https://github.com/nostr-protocol/nips/blob/master/59.md))

- **Layers:**
  - rumor: an unsigned event;
  - seal: kind 13, NIP-44-encrypted, signed by the real author, tags empty;
  - gift wrap: kind 1059, signed by a throwaway key, `p`-tagged to the recipient.
- **Kind 21059 (ephemeral gift wrap):** relays MUST NOT store it, and it is meant for real-time use. It was merged into NIP-59 on 2026-05-28 ([PR #2245](https://github.com/nostr-protocol/nips/pull/2245), authored by the ContextVM team).
- **Timestamps:** the seal and wrap SHOULD be backdated by a random amount (up to 2 days in the sample code), always into the past.
- **Relay guidance:** relays SHOULD gate 1059 behind AUTH and serve it only to the `p`-tagged recipient.
- **What relays see:** a one-time pubkey, the kind, the recipient `p`, a fuzzed time and ciphertext. They do not see the author.

### 1.7 NIP-17: private DMs ([17.md](https://github.com/nostr-protocol/nips/blob/master/17.md))

- Kind 14 (text) and kind 15 (an encrypted file: AES-GCM key and nonce plus hashes) travel as gift-wrapped rumors.
- The recipient advertises kind 10050 listing their DM relays. Senders MUST publish only to those relays.
- Disappearing messages use `expiration` on the wrap and the seal.
- Groups above about 10 participants are impractical because each recipient gets a separate wrap.

**Relevance to nerve:** any stock Nostr messenger (Amethyst, 0xchat and others) can read and answer NIP-17 DMs. That is a zero-custom-app phone path.

### 1.8 NIP-46: remote signing ([46.md](https://github.com/nostr-protocol/nips/blob/master/46.md))

- Kind 24133 carries NIP-44-encrypted JSON-RPC: `connect`, `sign_event`, `get_public_key`, `nip44_encrypt` / `nip44_decrypt`, `switch_relays`, `ping` and `logout`.
- Sessions start from either a `bunker://` or a `nostrconnect://` URI.
- Permissions look like `sign_event:4`.

**Relevance:** the Mac or a phone signer (Amber, nsec.app) can hold the user's root key while devices hold only session keys. That is useful for authorising device enrolment.

### 1.9 NIP-65: relay lists ([65.md](https://github.com/nostr-protocol/nips/blob/master/65.md))

- Kind 10002 lists `r` tags marked `read` or `write`.
- Recommended size is 2–4 relays per category.
- Publish to the author's write relays and to the read relays of anyone tagged.

### 1.10 NIP-70: protected events ([70.md](https://github.com/nostr-protocol/nips/blob/master/70.md))

- An event carrying the `["-"]` tag may be published only by its author, after AUTH.
- Relays MUST reject such events by default.
- The tag stops third parties from rebroadcasting your session snapshots to other relays. Every public relay I probed lists NIP-70.

### 1.11 NIP-78: app data ([78.md](https://github.com/nostr-protocol/nips/blob/master/78.md))

- Kind 30078 is addressable. The `d` tag is free-form ("app name and context"), and content and tags can be anything.
- Kind 78 is the regular-event variant.
- Relays SHOULD require AUTH and serve these events only to their author. In practice, public relays don't enforce this (none of the strfry relays probed list 42), so encrypt the content.

### 1.12 NIP-90: Data Vending Machines ([90.md](https://github.com/nostr-protocol/nips/blob/master/90.md))

- **Kinds:** requests 5000–5999, results 6000–6999 (always the request kind + 1000), feedback 7000.
- **Feedback `status` values:** `payment-required`, `processing`, `error`, `success`, `partial`.
- **Request tags:** `i` (input types `url` / `event` / `job` / `text`), `output`, `param`, `bid`, `relays`, `p`.
- **Encrypted requests:** put the encrypted `i` and `param` tags in `content` and add `["encrypted"]`. **Note:** the spec still says NIP-04 for this.
- **Cancellation:** a kind 5 deletion.
- **Status:** NIP-90 is now flagged **unrecommended** in the [NIPs README](https://github.com/nostr-protocol/nips/blob/master/README.md) ("this got totally out of control, prefer use-case-specific microstandards").

**Advice:** copy the request → feedback → result shape, not the kinds or the payment machinery.

### 1.13 NIP-29: relay-based groups ([29.md](https://github.com/nostr-protocol/nips/blob/master/29.md))

- A group is a random id whose membership and writes are enforced by one relay.
- Kinds 9000–9030 are moderation; kinds 39000+ are metadata.
- This could model a "fleet" if you run your own relay, but it ties trust to that relay. Not needed for a single user.

### 1.14 NIP-EE and Marmot (MLS)

- NIP-EE is **unrecommended, superseded by Marmot** ([README](https://github.com/nostr-protocol/nips/blob/master/README.md)).
- Marmot (status "adopted"; the [README](https://github.com/marmot-protocol/marmot) calls the MIP-era documents deprecated):
  - Nostr keys for identity;
  - MLS (RFC 9420) for group key agreement;
  - kinds 443 (KeyPackage), 444 (Welcome), 445 (Group Event) and 10051.
- The Rust implementation is `mdk-core` 0.8.0, which requires **MSRV 1.90** ([crates.io](https://crates.io/crates/mdk-core)). `whitenoise-rs` is now archived on GitHub.
- Marmot provides forward secrecy and post-compromise security, unlike NIP-44.
- **Overkill for a handful of a single user's devices**, and its MSRV conflicts with nerve's 1.85.

### 1.15 NIP-B7 Blossom and NIP-96

- **NIP-B7 ([B7.md](https://github.com/nostr-protocol/nips/blob/master/B7.md)):** blobs are addressed by SHA-256, and a kind 10063 server list lets clients find them.
- **NIP-96** is unrecommended, replaced by Blossom.
- **Use for nerve:** encrypt large diffs or transcripts locally, upload the ciphertext, and send `{url, sha256, key, nonce}` inside an encrypted event. This mirrors NIP-17 kind 15.
- `nostr-blossom` 0.45.1 exists ([crates.io](https://crates.io/crates/nostr-blossom)).

### 1.16 NIP-11: relay info ([11.md](https://github.com/nostr-protocol/nips/blob/master/11.md))

The `limitation` object can contain:

| Field | Meaning |
|---|---|
| `max_message_length` | Bytes of incoming JSON; effectively the maximum event size |
| `max_content_length` | Characters in `content` |
| `max_subscriptions` | Subscriptions per connection |
| `max_limit` | Clamp applied to a filter's `limit` |
| `max_subid_length` | Maximum subscription-id length |
| `max_event_tags` | Maximum number of tags |
| `min_pow_difficulty` | Required NIP-13 proof of work |
| `auth_required` | Relay requires NIP-42 AUTH |
| `payment_required` | Relay requires payment |
| `restricted_writes` | Writes are limited |
| `created_at_lower_limit` / `created_at_upper_limit` | Accepted timestamp window |
| `default_limit` | Events returned when no `limit` is given |

nerve should fetch each relay's NIP-11 document at startup and size its chunks to the smallest `max_message_length` across the relays it uses.

### 1.17 NIP-77: negentropy ([77.md](https://github.com/nostr-protocol/nips/blob/master/77.md))

- `NEG-OPEN` / `NEG-MSG` / `NEG-CLOSE` / `NEG-ERR` run range-based set reconciliation over a filter. They exchange only IDs; you then fetch with `REQ` or push with `EVENT`.
- **Use for nerve:** a device that has been offline catches up on stored command and result history cheaply.
- Only some relays advertise it (offchain.pub and auth.nostr1.com among those probed).
- The `negentropy` crate is at 0.5.1 and is a dependency of `nostr-sdk`.

### 1.18 NIP-C7 chats ([C7.md](https://github.com/nostr-protocol/nips/blob/master/C7.md))

- Kind 9 is a chat message; replies use a `q` tag.
- Public. Not useful for nerve except perhaps as a NIP-29 group chat UI.

### 1.19 Newer and agent-related proposals (all unmerged unless stated)

- **ContextVM: "NIP-XX: MCP JSON-RPC over Nostr"** ([PR #2246](https://github.com/nostr-protocol/nips/pull/2246), open since 2026-02).
  - Uses a single ephemeral kind, **25910**, for all MCP traffic.
  - Public server announcements use replaceable kinds **11316** (server), **11317** (tools), **11318** (resources), **11319** (resource templates) and **11320** (prompts), per [CEP-6](https://docs.contextvm.org/reference/ceps/cep-6/).
  - Encryption is defined in CEP-4. CEP-19 covers ephemeral gift wraps, which became 21059.
  - [CEP-22](https://docs.contextvm.org/reference/ceps/cep-22/) defines oversized-payload chunking through MCP progress notifications. Its example uses chunks of up to 65,536 bytes for a 10 MiB transfer.
  - Spec: [ctxvm-draft-spec](https://docs.contextvm.org/reference/spec/ctxvm-draft-spec/).
- **NIP-XX "AI Agent Messages"** ([PR #2226](https://github.com/nostr-protocol/nips/pull/2226), open) proposes:
  - prompt, response, streaming-delta, status, tool-telemetry, error and cancel kinds;
  - a replaceable `ai.info` discovery event (31340);
  - `seq`-based stream reconciliation;
  - NIP-44 payloads.

  **Worth reading for nerve's stream design.**
- **Other open agent PRs:**
  - NIP-AE "Agents" ([#2220](https://github.com/nostr-protocol/nips/pull/2220));
  - NIP-C1 "Agent TUI Messages" ([#2295](https://github.com/nostr-protocol/nips/pull/2295)): buttons and cards inside NIP-17 DM content, a good fit for "approve / deny" prompts on stock clients;
  - NIP-AC "DVM Agent Coordination" ([#2253](https://github.com/nostr-protocol/nips/pull/2253)), with heartbeat kind 30333;
  - LLM/AI Agents ([#1929](https://github.com/nostr-protocol/nips/pull/1929)).

  None is merged. **Do not depend on their kind numbers.**
- **NIP-9a "Relay Push"** ([PR #2194](https://github.com/nostr-protocol/nips/pull/2194), [draft text](https://github.com/coracle-social/nips/blob/635449ebfea6ee25da570e3642c9762e78b02904/9a.md)):
  - kind 30390 registers a filter plus an HTTP `callback`;
  - the relay POSTs matching event ids (optionally the full event);
  - intended for FCM, APNs or VAPID gateways.
- **Kind registry:** the README states that its kind table "is not exhaustive" and points to [registry-of-kinds](https://github.com/nostr-protocol/registry-of-kinds). Check for collisions there before choosing kinds.

### 1.20 NIP table: use in nerve sync

| NIP | What it gives | Use in nerve sync |
|---|---|---|
| 01 | Kind classes, REQ/OK/EOSE | Core transport; `OK` = per-relay write ack |
| 09 | Deletion request | Cancel jobs, retract stale snapshots |
| 11 | Relay limits | Auto-size chunks; check support for 40/42/70/77 |
| 13 | Proof of work | Possibly needed on strict relays for wrapped events |
| 17 | Gift-wrapped DMs, kind 10050 | Fallback UI: alerts and questions in Amethyst or 0xchat |
| 40 | Expiration | TTL on commands, questions, claims |
| 42 | AUTH | Required on a private relay; lets the relay serve 1059 only to the recipient |
| 44 | Encryption | All payloads |
| 46 | Remote signing | Root key in a phone signer authorises device enrolment |
| 59 | Gift wrap 1059 / 21059 | Hide which device talks to which; live commands |
| 65 | Relay lists | Fleet relay set |
| 70 | Protected `["-"]` | Prevent rebroadcast of snapshots |
| 77 | Negentropy | Catch-up after offline |
| 78 | 30078 app data | Encrypted device and session state |
| 90 | DVM (unrecommended) | Borrow the request/feedback/result pattern only |
| B7 | Blossom | Large diffs and transcripts as encrypted blobs |
| 29 / EE / Marmot | Groups / MLS | Not needed for one user; Marmot if forward secrecy is required |
| 9a (draft) | Relay push | iOS / PWA push |

---

## 2. Rust ecosystem

### 2.1 rust-nostr (now under the `nostrdevkit` GitHub org)

Source: [repo](https://github.com/rust-nostr/nostr), which redirects to `nostrdevkit/nostr`. Versions and MSRV come from the crates.io API.

| Crate | Version | MSRV | Notes |
|---|---|---|---|
| `nostr` | 0.45.5 (2026-09-11) | 1.85.0 | Features include `nip04`, `nip06`, `nip44`, `nip46`, `nip47`, `nip49`, `nip59`, `nip60`, `nip98`, `all-nips`, `pow-multi-thread` |
| `nostr-sdk` | 0.45.4 (2026-09-19) | 1.85.0 | Features: TLS backends (`ring` default, `aws_lc_rs`, `native-tls`, `rustls-*`) and **`local-relay`**, which enables `nostr_sdk::local_relay::{LocalRelay, MockRelay}` ([docs.rs](https://docs.rs/nostr-sdk/0.45.4/nostr_sdk/)). Modules: `client`, `relay`, `authenticator` (NIP-42), `monitor`, `policy`, `proxy`, `transport` |
| `nostr-relay-builder` | 0.44.1 stable; 0.45.0-alpha.3 latest | 1.85.0 | Appears to have been folded into `nostr-sdk`'s `local-relay` feature in 0.45 **[UNVERIFIED]** |
| `nostr-lmdb` / `nostr-ndb` / `nostr-sqlite` / `nostr-memory` | 0.45.2–0.45.3 | 1.85.0 | Database backends behind the `nostr-database` traits |
| `nostr-connect` | 0.45.2 | 1.85.0 | NIP-46 |
| `nostr-blossom` | 0.45.1 | 1.85.0 | Blossom client |
| `contextvm-sdk` | 0.2.2 | **1.88** | MCP-over-Nostr; builds on `rmcp` |
| `mdk-core` | 0.8.0 | **1.90** | Marmot |

- **Project status:** the README says "These libraries are in ALPHA state", with breaking API changes expected.
- **NIP coverage:** the README marks ✅ for 01, 09, 11, 13, 17, 40, 42, 44, 46, 59, 65, 70, 77, 78, 90 and B7, among others. NIP-29 is ❌.
- **Fit with nerve:** the MSRV of 1.85 matches nerve's exactly, and the crates are MIT-licensed.
- **Cost:** `nostr-sdk` pulls in tokio, hyper and async-wsocket. Isolate it in the discovery worker or a separate sync thread so the UI thread rule is preserved.

### 2.2 Relays

- **strfry** (C++, LMDB, negentropy; 731 stars, last push 2026-09) ([repo](https://github.com/hoytech/strfry)). Default [strfry.conf](https://github.com/hoytech/strfry/blob/master/strfry.conf) values:
  - `maxEventSize = 65536`
  - `maxWebsocketPayloadSize = 131072`
  - `rejectEphemeralEventsOlderThanSeconds = 60`
  - `ephemeralEventsLifetimeSeconds = 300`
  - `rejectEventsNewerThanSeconds = 900`

  **Implication:** NIP-59 backdating is fine for 1059, but a 21059 whose wrap is backdated more than 60 seconds will be rejected by default strfry. Do not backdate ephemeral wraps.
- **nostr-rs-relay** (Rust, SQLite/Postgres; 720 stars). The crates.io release (0.8.12) is from 2023, but the [repo](https://github.com/scsibug/nostr-rs-relay) was pushed in 2026-05.
- **rnostr** (Rust; 124 stars, pushed 2026-06) ([repo](https://github.com/rnostr/rnostr)).
- **khatru** (Go framework). The [repo](https://github.com/fiatjaf/khatru) is archived and in maintenance mode; development continues in `fiatjaf.com/nostr/khatru`.
- **Embedded:** `nostr_sdk::local_relay::LocalRelay` runs a full relay in-process. A Raspberry Pi can host the fleet relay with this or with strfry.

### 2.3 nak CLI ([repo](https://github.com/fiatjaf/nak); 420 stars, active 2026-10)

Install with `brew install nak`, or `go install github.com/fiatjaf/nak@latest`, or the release binaries. It is not currently installed on this Mac. Useful commands:

```sh
nak serve --negentropy                     # local test relay at ws://localhost:10547
nak key generate; nak key public <hex>
nak event -k 30078 -d nerve:device:mac -c '<ciphertext>' --tag expiration=$(($(date +%s)+120)) ws://localhost:10547
nak req -k 30078 -a <pubkey> -t d=nerve:device:mac ws://localhost:10547
nak req -k 25001 --stream ws://localhost:10547   # watch live; flag name not verified
nak event -c 'approve' | nak gift wrap --sec <k> -p <devicepub> | nak event wss://relay
nak req -p <me> -k 1059 relay | nak gift unwrap --sec <k> --from <sender>
nak sync relayA relayB                     # negentropy relay-to-relay
nak bunker --sec ... relay                 # NIP-46 signer for testing
nak blossom --server <host> --sec <k> upload file
```

**[UNVERIFIED]** I took `--stream` from memory; check `nak req --help`.

---

## 3. Public relay realities

### 3.1 Live NIP-11 probe, 2026-10-09

| Relay | Software | max_message_length | max_subs | NIPs of interest |
|---|---|---|---|---|
| relay.damus.io | strfry 1.1.0+ | **1,000,000** | 200 | 9, 40, **59**, 70 |
| relay.primal.net | strfry | **1,000,000** | 20 | 9, 40, 70 |
| nos.lol | strfry 1.1.3 | 131,072 | 20 | 9, 40, 70 |
| nostr.mom | strfry 1.1.3 | 131,072 | 50 | 9, 40, 70 |
| offchain.pub | strfry 1.1.0 | 131,072 | 20 | 40, 70, **77** |
| relay.snort.social | memlay 0.2.0 | 524,288 | 300 | 1, 9, 11 only; `idle_timeout: 10` |
| nostr.wine (paid) | custom | 524,288 | 50 | 40, **42**, 70; `payment_required`, admission 18,888,000 msat (about 18.9k sats) |
| auth.nostr1.com | strfry | 262,200 | 80 | **42**, 77, 17; `auth_required: true` |
| relay.nostr.band | n/a | n/a | n/a | Timed out |

- **relay.nostr.band** is widely reported as down or abandoned, and clients have been replacing it ([search results](https://github.com/vitorpamplona/amethyst)) **[UNVERIFIED in detail]**. Do not use it.
- **Rate limits** are not advertised in NIP-11 by these relays. strfry has configurable per-connection limits; plan for `rate-limited:` OK responses and back off. **[UNVERIFIED]** I have no exact figures.
- **Ephemeral events:** strfry forwards ephemeral events to live subscribers and keeps them only briefly (`ephemeralEventsLifetimeSeconds = 300`). It rejects them if they are more than 60 seconds old. I did not publish test events to public relays, because that would be posting publicly.
- **Kind 1059 acceptance:** relays that support NIP-17 accept it (Damus advertises 59). Some messaging relays restrict it: one report says White Noise's relays refuse REQs for 21059 and cap filters per REQ **[UNVERIFIED]**.
- **Kind 4 (NIP-04)** is advertised by every strfry relay. It is deprecated; do not use it.
- **Reliability:** public relays are free and best-effort. Any of them can drop you, rate-limit you or prune data.
- **Recommendation:**
  1. Write to two or three relays.
  2. One of them should be either a paid or AUTH-required relay, or your own relay on the Pi that is publicly reachable over WSS (for example behind Cloudflare Tunnel, which needs only outbound connectivity from the Pi).
  3. Dedupe by event id on receipt.

### 3.2 Metadata leakage

Relays see:
- your IP;
- connection timing;
- event kinds;
- `created_at`;
- tag values (`p`, `d`, custom tags);
- ciphertext sizes (bucketed by padding);
- the author pubkey, unless gift-wrapped.

A third party subscribed to the same relay sees everything except your IP. A custom kind such as `25xxx` also fingerprints "this is nerve traffic".

**Mitigations:**
1. NIP-44 for all content.
2. NIP-59 or 21059 wraps, so the author is hidden behind a throwaway key.
3. Opaque `d` tags (HMAC of the session id with a fleet secret).
4. AUTH relays that serve events only to the author or recipient (NIP-42, plus the NIP-78 and NIP-59 guidance).
5. NIP-70 `["-"]`, to block rebroadcast.
6. A self-hosted relay, so IPs go only to you.
7. Optionally Tor or a SOCKS proxy (`nostr_sdk::proxy`).

---

## 4. Existing projects

| Project | What it does | Nostr usage | Maturity |
|---|---|---|---|
| **CodeDeck+** ([repo](https://github.com/deymosh/codedeck-plus)) | Android app plus bridge that controls **Claude Code**, OpenCode and DeepSeek Harness on a laptop or VPS. Handles plan approval, permission cards and answering agent questions | Relays only "store and forward ciphertext"; NIP-44; NIP-42 AUTH. [PROTOCOL.md](https://github.com/deymosh/codedeck-plus/blob/HEAD/docs/PROTOCOL.md) uses **30515** (addressable session-list heartbeat, `d` = machine), **24515** (ephemeral live output and usage), **4515** (stored phone→bridge commands, NIP-40 expiry of 1 hour). Wire protocol v11 | Very young: 0 stars, no releases. **The closest prior art to nerve sync; read the protocol doc** |
| **Code Call / nostr-codex-phone** ([repo](https://github.com/tidley/nostr-codex-phone)) | Android and Linux clients drive an OpenCode worker: git status, diffs, files, calls | NIP-17/NIP-59 gift-wrapped DMs; Blossom for locally encrypted media; QR plus 6-digit pairing; Rust worker binary | 4 stars, v0.3.47, 412 commits |
| **ContextVM** ([docs](https://docs.contextvm.org/), [TS SDK](https://github.com/ContextVM/sdk), [rs-sdk](https://github.com/ContextVM/rs-sdk)) | MCP JSON-RPC over Nostr, plus gateway and proxy CLIs | Kind 25910 (ephemeral), announcements 11316–11320, 21059 wraps, CEP-22 chunking | Draft NIP PR open; the 21059 change was merged upstream; small user base (15 and 2 stars) |
| **Block Buzz** ([repo](https://github.com/block/buzz)) | Self-hosted workspace where "it's a Nostr relay"; agents (Goose, Codex, Claude Code via `buzz-acp`) are members with their own keys | Relay with Postgres, Redis and S3; NIP-34 git events | 35.7k stars, desktop v0.5.27 (2026-10-06), Windows build labelled "alpha-unsigned". Heavyweight team product rather than a device-sync layer |
| **Goose Nostr session sharing** | Shared sessions through an encrypted Nostr link | Removed from goose core on 2026-09-30 ([PR #12453](https://github.com/aaif-goose/goose/pull/12453), [issue #12169](https://github.com/aaif-goose/goose/issues/12169)) | The stated reason was that a throwaway key in the link meant "no way to validate the authenticity of an imported session". **Lesson: bind shared state to long-term device keys** |
| **TENEX** ([repo](https://github.com/tenex-chat/tenex)) | Multi-agent dev environment built on Nostr | Custom kinds **[UNVERIFIED]** | 39 stars, last push 2026-05 |
| **OpenClaw Nostr channel** ([docs](https://docs.openclaw.ai/channels/nostr)) | Chat with an OpenClaw agent through Nostr DMs | NIP-04 only, single account, DMs only. Community NIP-17 plugins exist: [Dash1971](https://github.com/Dash1971/openclaw-nostr-nip17), [Ink-North](https://github.com/Ink-North/nostr-nip17-plugin), [fabianfabian](https://github.com/fabianfabian/openclaw-nostr-nip17) | Plugin-level |
| **Clawstr** ([site](https://clawstr.com), [repo](https://github.com/clawstr/clawstr)) | Social network where only AI agents post | NIP-22 comments on NIP-73 identifiers, NIP-32 AI labels | Not relevant to control |
| **Shakespeare** ([repo](https://github.com/soapbox-pub/shakespeare)) | AI website builder for Nostr apps | Not agent control | 23 stars |
| **DVMCP / nostr-dvm** | MCP tools exposed as NIP-90 DVMs (`@dvmcp/bridge`); Python DVM framework | NIP-90 | Superseded by ContextVM ([DVMCP](https://github.com/gzuuus/dvmcp)) |
| **nostrclaw** ([repo](https://github.com/rzazo24/nostrclaw)) | MCP server that lets Claude inspect relays and publish (via NIP-46) | NIP-11, 45, 46 | Experimental |
| **Marmot / White Noise** | MLS group chat | Kinds 443–445 | `whitenoise-rs` archived; Amethyst Marmot work in [PR #4073](https://github.com/vitorpamplona/amethyst/pull/4073) |

I found no "nostr-ssh" or "nostr tunnel" project that is maintained and relevant. **[UNVERIFIED]** I did not search exhaustively.

Anthropic's own Claude Code Remote Control does not use Nostr; it goes through Anthropic's API over outbound HTTPS. It is the obvious non-Nostr comparison.

---

## 5. NAT traversal and latency

- **No inbound connectivity needed.** NIP-01 is a client-to-relay WebSocket protocol, and both the agent host and the controller are clients that dial out. That works behind CGNAT, mobile carriers and hotel Wi-Fi. A relay on the Pi is the only thing that needs to be reachable, and a tunnel can make it reachable without opening inbound ports.
- **Latency.** A message costs sender → relay plus relay → subscriber over already-open sockets, with no extra handshake per message.
  - Measured from this Mac to Damus, nos.lol, Primal and nostr.mom: TCP connect about 0.19–0.23 s and TLS about 0.26–0.48 s. That is one-time setup cost.
  - Steady-state one-way delivery should be roughly two relay-path RTT halves plus relay processing. My estimate is about 100–400 ms on public relays. **[UNVERIFIED]** I found no rigorous benchmark.
  - A Pi relay on the LAN gives LAN latency when both peers are home.
- **Keepalive.** Mobile networks and some relays (snort's `idle_timeout: 10`) close idle sockets. Send pings and re-subscribe with `since` and negentropy after a reconnect.
- **Sync ordering.** Clock skew matters: strfry rejects events stamped more than 900 seconds in the future, and ephemeral events older than 60 seconds.

---

## 6. Mobile clients

- **Android:**
  - Amethyst has an always-on notification relay service ([PR #4373](https://github.com/vitorpamplona/amethyst/pull/4373)) and supports NIP-17. If nerve sends questions and alerts as NIP-17 DMs to the user's personal npub, the user can answer from Amethyst or 0xchat with no custom app.
  - Pokey ([repo](https://github.com/KoalaSat/pokey)) provides "pull notifications": it holds relay sockets and raises Android notifications. 56 stars, last push 2025-07.
- **iOS:** apps cannot keep WebSockets open in the background, so push needs a server.
  - Damus uses [notepush](https://github.com/damus-io/notepush), a relay that sends to APNs with NIP-98 auth. It is marked "WIP! Experimental!".
  - The generic route is the NIP-9a draft: a kind 30390 registration and an HTTP callback to your own FCM, APNs or VAPID gateway.
  - Without either, the iOS user sees updates only when the app is open.
- **PWA:** use `nostr-tools` (npm 2.25.2) for live view while the page is open. For background, use Web Push (VAPID) fed by a small service on the Pi that subscribes to the relay and forwards. The Pi can do this because it is always on. **This is likely the cheapest cross-platform "approve permission prompt" path.**

---

## 7. Recommended event design for nerve

### 7.1 Keys and encryption model

- **Device keys:** each device (Mac, Pi, phone) has its own keypair, stored in the OS keychain or a `0600` file under the nerve config directory.
- **Fleet roster:** a **fleet roster** event lists the authorised device pubkeys, with names and capabilities. It is signed by a root key, which can live in a NIP-46 signer on the phone. Devices accept commands only from pubkeys in the latest roster.
- **Encryption:** NIP-44 per recipient. With fewer than 5 devices, encrypt each message separately to each target.
  - Alternative: a shared fleet key that every device holds. Simpler, but compromising one device exposes all traffic.
- **Hiding sender and receiver:** wrap commands in 21059 (live) or 1059 (durable).
- **Relay set:** a NIP-65-style relay list kept in the roster.
- **Kind collisions:** the custom kinds below are illustrative. Check them against [registry-of-kinds](https://github.com/nostr-protocol/registry-of-kinds).
- **Common tags:** every event carries `["-"]` (NIP-70) where the relay supports it, and a protocol version tag, `["v","1"]`.

### 7.2 Event kinds

| Purpose | Kind (class) | Addressing / tags | Content (NIP-44) | Lifetime |
|---|---|---|---|---|
| Fleet roster | 30078 (addressable) | `d`=`nerve:fleet`; signed by root | `{devices:[{pub,name,caps,relays}]}`, encrypted to each device or to self | Durable |
| Device presence (state) | 30078 | `d`=`nerve:dev:<hmac(id)>`, `expiration`=now+3×interval | `{host, os, caps:{gpu,arch,ram,tools}, load, sessions:n, nerve_ver}` | Replaced every 30–60 s |
| Heartbeat (cheap) | 25500 (ephemeral) | `p`=fleet, or none | Tiny ping with seq | Not stored |
| Session snapshot | 30078 (or custom 30500) | `d`=`nerve:sess:<hmac(session_id)>`, `expiration` once ended | `{state, cwd, branch, model, tokens, last_tool, pending_question_id, subagents:[...]}` | Latest only; refresh on change |
| Progress stream | 21059 wrapping a rumor of kind 25501, or bare 25501 if you accept visible authors | `p`=target viewer(s), `s`=opaque session tag | `{seq, ts, delta:[...]}`; the receiver detects gaps by `seq` and re-reads the snapshot | Ephemeral; strfry drops after 300 s, rejects if more than 60 s old |
| Command / instruction | 1059 wrapping a rumor of kind 4501 (durable), or 21059 (live-only) | Wrap `p`=target device; rumor tags `e`/`s`, `expiration`=now+1 h | `{cmd:"send_input"\|"interrupt"\|"approve"\|..., session, payload, nonce}` | Durable until expiry, so an offline device picks it up |
| Command ack / result | 1059 → rumor 4502 | `e`=command rumor id | `{ok, error?, output_ref?}` | 1 h–1 d |
| Question (permission or ask) | 1059 → rumor 4503, plus a mirrored NIP-17 kind 14 DM to the user's npub for stock clients | `s`, `q` id, `expiration`=question timeout | `{qid, tool, input_summary, options:["allow","deny","allow_always"], diff_ref?}` | Until answered or expired |
| Answer | 1059 → rumor 4504 | `e`=question id | `{qid, choice, text?}`; the first valid answer wins, others are acked as "already answered" | Short |
| Diff / subagent payload | Blossom blob (encrypted locally) referenced from 4505, or chunked rumors `{chunk:i/n, sha256}` | `e`=parent | `{url, sha256, key, nonce, mime}` | Durable; delete with NIP-09 |
| Handoff | 1059 → rumor 4506 (offer), 4507 (accept), 4508 (complete) | `e` chain | Offer: `{session, transcript_ref (Blossom), git:{remote,branch,sha}, cwd_hint}`. The target pulls, resumes (`claude --resume` with the imported transcript **[UNVERIFIED]**), then publishes a new snapshot; the source marks the old snapshot `state:"handed_off"` | Durable |
| Job request | 1059 → rumor 5500 (pattern borrowed from NIP-90) | `["req_caps","arch=arm64","gpu"]` and similar, `expiration` | `{job_id, spec, repo, prompt, constraints}` broadcast to all capable devices | Durable until claimed or expired |
| Job claim | 1059 → rumor 5501 | `e`=request | `{job_id, device, eta}` | Short |
| Job feedback | 7000-style status (`processing` / `partial` / `error` / `success`) as rumor 5502 | `e`=request | NIP-90 status vocabulary | Short |
| Job result | 1059 → rumor 6500 | `e`=request | `{job_id, summary, artifacts:[blossom refs]}` | Durable |

### 7.3 Claim arbitration

Relays give no compare-and-swap, so use one of these:
- **(a) Coordinator:** a single coordinator (for example the always-on Pi) issues an `assign` event. **Simplest and deterministic.**
- **(b) Deterministic tie-break:** collect claims for N seconds and award the job to the lowest `hash(job_id ‖ device_pub)` among claimants. Every device computes the same winner.

I recommend (a), with (b) as fallback.

### 7.4 Large payloads

- Keep each event at or below 60 KiB after base64. That fits strfry's 64 KiB default and nos.lol's 128 KB.
- For more, use encrypted Blossom blobs, or chunk following the CEP-22 approach (`seq/total` plus a final hash).

### 7.5 Catch-up after offline

- Use `REQ` with `since=last_seen` on the wrapped kinds addressed to me (`#p`).
- Use NIP-77 negentropy where the relay supports it.
- Re-read all `nerve:sess:*` snapshots.

### 7.6 Implementation note for nerve's architecture

- Run `nostr-sdk` in its own tokio thread, the "sync worker". It feeds the existing state model the same way the discovery worker does.
- Inbound fields such as hostnames, session titles and transcript snippets must be wrapped in `Sanitised`.
- Before acting on any inbound `interrupt` or `kill` command, re-validate the PID as the CLAUDE.md rule requires.
- Treat remote command execution as a security boundary: allowlist the roster pubkeys, enforce expiry, and dedupe nonces.

---

## 8. Uncertainties

- Whether rust-nostr 0.45 implements the NIP-44 extended (more than 64 KiB) length prefix.
- Exact public-relay rate limits and how each one treats 21059 or custom ephemeral kinds. Test with `nak serve`, then a throwaway key on a relay you control, before using public relays.
- Real-world delivery latency on public relays.
- Whether `nostr-relay-builder` is fully superseded by `nostr-sdk`'s `local-relay` feature.
- Maturity of every agent-over-Nostr project listed: they are 0–40 stars, except Buzz.
- Exact `claude --resume` semantics for an imported transcript in a handoff.

---

## Sources

**NIPs and specs**
- https://github.com/nostr-protocol/nips/blob/master/README.md
- https://github.com/nostr-protocol/nips/blob/master/01.md
- https://github.com/nostr-protocol/nips/blob/master/09.md
- https://github.com/nostr-protocol/nips/blob/master/11.md
- https://github.com/nostr-protocol/nips/blob/master/17.md
- https://github.com/nostr-protocol/nips/blob/master/29.md
- https://github.com/nostr-protocol/nips/blob/master/40.md
- https://github.com/nostr-protocol/nips/blob/master/42.md
- https://github.com/nostr-protocol/nips/blob/master/44.md
- https://github.com/nostr-protocol/nips/blob/master/46.md
- https://github.com/nostr-protocol/nips/blob/master/59.md
- https://github.com/nostr-protocol/nips/blob/master/65.md
- https://github.com/nostr-protocol/nips/blob/master/70.md
- https://github.com/nostr-protocol/nips/blob/master/77.md
- https://github.com/nostr-protocol/nips/blob/master/78.md
- https://github.com/nostr-protocol/nips/blob/master/90.md
- https://github.com/nostr-protocol/nips/blob/master/B7.md
- https://github.com/nostr-protocol/nips/blob/master/C7.md
- https://github.com/nostr-protocol/registry-of-kinds

**NIP pull requests and drafts**
- https://github.com/nostr-protocol/nips/pull/2245
- https://github.com/nostr-protocol/nips/pull/2246
- https://github.com/nostr-protocol/nips/pull/2226
- https://github.com/nostr-protocol/nips/pull/2220
- https://github.com/nostr-protocol/nips/pull/2295
- https://github.com/nostr-protocol/nips/pull/2253
- https://github.com/nostr-protocol/nips/pull/1929
- https://github.com/nostr-protocol/nips/pull/2194
- https://github.com/coracle-social/nips/blob/635449ebfea6ee25da570e3642c9762e78b02904/9a.md

**ContextVM**
- https://docs.contextvm.org/
- https://docs.contextvm.org/reference/spec/ctxvm-draft-spec/
- https://docs.contextvm.org/reference/ceps/cep-6/
- https://docs.contextvm.org/reference/ceps/cep-22/
- https://github.com/ContextVM/sdk
- https://github.com/ContextVM/rs-sdk

**Marmot**
- https://github.com/marmot-protocol/marmot
- https://github.com/marmot-protocol/mdk

**Rust crates and security**
- https://github.com/rust-nostr/nostr
- https://crates.io/crates/nostr
- https://crates.io/crates/nostr-sdk
- https://docs.rs/nostr-sdk/0.45.4/nostr_sdk/
- https://crates.io/crates/nostr-relay-builder
- https://crates.io/crates/nostr-lmdb
- https://crates.io/crates/nostr-ndb
- https://crates.io/crates/nostr-blossom
- https://crates.io/crates/contextvm-sdk
- https://crates.io/crates/mdk-core
- https://rustsec.org/advisories/RUSTSEC-2026-0216.html

**Relays and tools**
- https://github.com/hoytech/strfry
- https://github.com/hoytech/strfry/blob/master/strfry.conf
- https://github.com/scsibug/nostr-rs-relay
- https://github.com/rnostr/rnostr
- https://github.com/fiatjaf/khatru
- https://github.com/fiatjaf/nak
- Live NIP-11 documents: https://relay.damus.io, https://nos.lol, https://relay.primal.net, https://nostr.mom, https://offchain.pub, https://relay.snort.social, https://nostr.wine, https://auth.nostr1.com

**Agent-over-Nostr projects**
- https://github.com/deymosh/codedeck-plus
- https://github.com/deymosh/codedeck-plus/blob/HEAD/docs/PROTOCOL.md
- https://github.com/tidley/nostr-codex-phone
- https://github.com/block/buzz
- https://github.com/aaif-goose/goose/pull/12453
- https://github.com/aaif-goose/goose/issues/12169
- https://github.com/tenex-chat/tenex
- https://docs.openclaw.ai/channels/nostr
- https://github.com/Dash1971/openclaw-nostr-nip17
- https://github.com/Ink-North/nostr-nip17-plugin
- https://github.com/fabianfabian/openclaw-nostr-nip17
- https://clawstr.com
- https://github.com/clawstr/clawstr
- https://github.com/soapbox-pub/shakespeare
- https://github.com/gzuuus/dvmcp
- https://github.com/rzazo24/nostrclaw

**Mobile and push**
- https://github.com/vitorpamplona/amethyst/pull/4373
- https://github.com/vitorpamplona/amethyst/pull/4073
- https://github.com/KoalaSat/pokey
- https://github.com/damus-io/notepush
- https://www.npmjs.com/package/nostr-tools
