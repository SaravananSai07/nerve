//! Nostr transport: per-recipient NIP-44 encryption, state as addressable
//! events, everything else as stored, expiring messages. Knows nothing
//! about sessions; `agent` and `cli` give it envelopes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use nostr::nips::nip44;
use nostr_sdk::prelude::*;

use super::protocol::{self, Envelope, KIND_MSG, KIND_STATE};

const RATE_LIMIT_BACKOFF: Duration = Duration::from_millis(1500);
const RATE_LIMIT_PATIENCE: Duration = Duration::from_secs(120);

/// NIP-01 OK prefix `rate-limited:`.
fn is_rate_limited(why: &str) -> bool {
    why.contains("rate-limited")
}

pub(crate) struct Transport {
    pub(crate) client: Client,
    pub(crate) keys: Keys,
    /// Self-hosted (`ws://`) relays take traffic as fast as we send it;
    /// public relays rate-limit and ban bursts (measured: damus banned a
    /// key after ~20 events/s), so chunk trains are paced for them.
    paced: bool,
    /// created_at of the last state event per recipient. Addressable
    /// events tie-break on created_at, so a second publish within the same
    /// second is rejected by the relay; each one has to move forward.
    last_state: std::sync::Mutex<HashMap<PublicKey, u64>>,
}

/// One decrypted inbound envelope plus who really sent it (the event's
/// signing key, not the self-reported `from`).
pub(crate) struct Inbound {
    pub(crate) sender: PublicKey,
    pub(crate) created_at: u64,
    pub(crate) envelope: Envelope,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RelayHealth {
    pub(crate) url: String,
    pub(crate) connected: bool,
}

impl Transport {
    pub(crate) async fn connect(keys: Keys, relays: &[String]) -> Result<Self, String> {
        if relays.is_empty() {
            return Err("no relays configured (nerve sync init --relay ...)".into());
        }
        let client = Client::builder().build();
        for r in relays {
            client.add_relay(r.as_str()).await.map_err(|e| format!("relay {r}: {e}"))?;
        }
        client.connect().and_wait(Duration::from_secs(8)).await;
        let paced = relays.iter().any(|r| r.starts_with("wss://"));
        Ok(Self { client, keys, paced, last_state: Default::default() })
    }

    pub(crate) async fn health(&self) -> Vec<RelayHealth> {
        let relays = self.client.relays().await;
        let mut out: Vec<RelayHealth> = relays
            .iter()
            .map(|(url, r)| RelayHealth { url: url.to_string(), connected: r.status() == RelayStatus::Connected })
            .collect();
        out.sort_by(|a, b| a.url.cmp(&b.url));
        out
    }

    fn seal(&self, to: &PublicKey, env: &Envelope) -> Result<String, String> {
        nip44::encrypt(self.keys.secret_key(), to, env.encode(), nip44::Version::V2).map_err(|e| e.to_string())
    }

    async fn publish(&self, builder: EventBuilder) -> Result<usize, String> {
        self.publish_to(builder, None).await.map(|(n, _)| n)
    }

    /// How many relays took it, and its id (for a later deletion).
    async fn publish_to(&self, builder: EventBuilder, only: Option<Vec<String>>) -> Result<(usize, EventId), String> {
        let event = builder.finalize(&self.keys).map_err(|e| e.to_string())?;
        // A relay that rate-limits refills its tokens within seconds, so a
        // long chunk train waits it out instead of dropping the transfer.
        let mut waited = Duration::ZERO;
        loop {
            let out = match &only {
                Some(urls) => self.client.send_event(&event).to(urls.iter().map(String::as_str)).await,
                None => self.client.send_event(&event).await,
            }
            .map_err(|e| e.to_string())?;
            if !out.success.is_empty() {
                return Ok((out.success.len(), event.id));
            }
            let why = out.failed.values().next().map(|w| w.to_string()).unwrap_or_default();
            if !is_rate_limited(&why) || waited >= RATE_LIMIT_PATIENCE {
                return Err(format!("no relay accepted the event: {why}"));
            }
            tokio::time::sleep(RATE_LIMIT_BACKOFF).await;
            waited += RATE_LIMIT_BACKOFF;
        }
    }

    /// Replace this device's state as seen by `to`.
    pub(crate) async fn put_state(&self, to: &PublicKey, env: &Envelope) -> Result<usize, String> {
        let at = {
            let mut last = self.last_state.lock().unwrap_or_else(|e| e.into_inner());
            let prev = last.get(to).copied().unwrap_or(0);
            let at = Timestamp::now().as_secs().max(prev + 1);
            last.insert(*to, at);
            at
        };
        let b = EventBuilder::new(Kind::Custom(KIND_STATE), self.seal(to, env)?)
            .custom_created_at(Timestamp::from(at))
            .tag(Tag::identifier(protocol::state_d_tag(&to.to_hex())))
            .tag(Tag::public_key(*to))
            .tag(Tag::expiration(Timestamp::now() + protocol::STATE_TTL_SECS));
        self.publish(b).await
    }

    pub(crate) async fn send(&self, to: &PublicKey, env: &Envelope) -> Result<usize, String> {
        self.send_id(to, env).await.map(|(n, _)| n)
    }

    /// `send`, keeping the event's id for `delete`.
    pub(crate) async fn send_id(&self, to: &PublicKey, env: &Envelope) -> Result<(usize, EventId), String> {
        let b = EventBuilder::new(Kind::Custom(KIND_MSG), self.seal(to, env)?)
            .tag(Tag::public_key(*to))
            .tag(Tag::expiration(Timestamp::now() + protocol::MSG_TTL_SECS));
        let n = self.publish_to(b, None).await?;
        if self.paced {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(n)
    }

    /// NIP-09: ask the relays to drop events of ours whose work is done
    /// (a delivered transfer), instead of holding them until they expire.
    pub(crate) async fn delete(&self, ids: &[EventId]) -> Result<(), String> {
        for batch in ids.chunks(500) {
            let b = EventBuilder::new(Kind::EventDeletion, "")
                .tags(batch.iter().map(|id| Tag::event(*id)))
                .tag(Tag::expiration(Timestamp::now() + protocol::MSG_TTL_SECS));
            self.publish(b).await?;
        }
        Ok(())
    }

    /// Bulk (handoff packs, diffs): only via connected self-hosted relays
    /// when there are any. Public relays rate-limit chunk trains (and
    /// store them for everyone), a private relay on a private network does not.
    pub(crate) async fn send_bulk(&self, to: &PublicKey, env: &Envelope) -> Result<(usize, EventId), String> {
        let private: Vec<String> = self
            .health()
            .await
            .into_iter()
            .filter(|h| h.connected && h.url.starts_with("ws://"))
            .map(|h| h.url)
            .collect();
        if private.is_empty() {
            return self.send_id(to, env).await;
        }
        let b = EventBuilder::new(Kind::Custom(KIND_MSG), self.seal(to, env)?)
            .tag(Tag::public_key(*to))
            .tag(Tag::expiration(Timestamp::now() + protocol::MSG_TTL_SECS));
        self.publish_to(b, Some(private)).await
    }

    /// Live + stored traffic addressed to us from `authors`, since `since`.
    pub(crate) async fn subscribe(&self, authors: &[PublicKey], since: u64) -> Result<(), String> {
        let me = self.keys.public_key();
        let msgs = Filter::new()
            .kind(Kind::Custom(KIND_MSG))
            .pubkey(me)
            .authors(authors.iter().copied())
            .since(Timestamp::from(since));
        let states = Filter::new()
            .kind(Kind::Custom(KIND_STATE))
            .pubkey(me)
            .authors(authors.iter().copied());
        self.client.subscribe(msgs).await.map_err(|e| e.to_string())?;
        self.client.subscribe(states).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// While an invite is open: messages to us from *any* key, so a
    /// joining device's `PairRequest` gets through. The agent drops
    /// everything else from unknown keys.
    pub(crate) async fn subscribe_open(&self, since: u64) -> Result<SubscriptionId, String> {
        let f = Filter::new()
            .kind(Kind::Custom(KIND_MSG))
            .pubkey(self.keys.public_key())
            .since(Timestamp::from(since));
        self.client.subscribe(f).await.map(|o| o.value).map_err(|e| e.to_string())
    }

    pub(crate) async fn unsubscribe(&self, id: &SubscriptionId) {
        let _ = self.client.unsubscribe(id).await;
    }

    /// One-shot read of peers' latest states (CLI `status`).
    pub(crate) async fn fetch_states(&self, authors: &[PublicKey]) -> Vec<Inbound> {
        let f = Filter::new()
            .kind(Kind::Custom(KIND_STATE))
            .pubkey(self.keys.public_key())
            .authors(authors.iter().copied());
        let Ok(events) = self.client.fetch_events(f).timeout(Duration::from_secs(8)).await else {
            return Vec::new();
        };
        let mut latest: HashMap<PublicKey, Inbound> = HashMap::new();
        for e in events.into_iter() {
            if let Some(i) = self.open(&e) {
                let newer = latest.get(&i.sender).is_none_or(|o| o.created_at < i.created_at);
                if newer {
                    latest.insert(i.sender, i);
                }
            }
        }
        latest.into_values().collect()
    }

    /// Decrypt and parse; None for anything not meant for us or garbled.
    pub(crate) fn open(&self, event: &Event) -> Option<Inbound> {
        if event.verify().is_err() {
            return None;
        }
        let plain = nip44::decrypt(self.keys.secret_key(), &event.pubkey, &event.content).ok()?;
        let envelope = Envelope::decode(&plain).ok()?;
        Some(Inbound {
            sender: event.pubkey,
            created_at: event.created_at.as_secs(),
            envelope,
        })
    }

    pub(crate) async fn disconnect(&self) {
        self.client.disconnect().await;
    }
}

/// Envelope ids already handled, bounded, persisted so a restart doesn't
/// replay a stored `Send` into a live session twice.
pub(crate) struct Seen {
    path: std::path::PathBuf,
    order: VecDeque<String>,
    set: HashSet<String>,
}

const SEEN_MAX: usize = 4000;

impl Seen {
    pub(crate) fn load(path: std::path::PathBuf) -> Self {
        let order: VecDeque<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect();
        let set = order.iter().cloned().collect();
        Self { path, order, set }
    }

    /// True the first time an id is seen.
    pub(crate) fn first(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return false;
        }
        self.set.insert(id.to_string());
        self.order.push_back(id.to_string());
        while self.order.len() > SEEN_MAX {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        let body: String = self.order.iter().map(|s| format!("{s}\n")).collect();
        let _ = std::fs::write(&self.path, body);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seen_dedupes_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("seen");
        let mut s = Seen::load(p.clone());
        assert!(s.first("a"));
        assert!(!s.first("a"));
        let mut s2 = Seen::load(p);
        assert!(!s2.first("a"));
        assert!(s2.first("b"));
    }

    // Full encrypt → event → decrypt path, with no relay.
    #[test]
    fn sealed_envelope_opens_only_for_its_recipient() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (a, b, c) = (Keys::generate(), Keys::generate(), Keys::generate());
            let ta = Transport { client: Client::builder().build(), keys: a.clone(), paced: false, last_state: Default::default() };
            let tb = Transport { client: Client::builder().build(), keys: b.clone(), paced: false, last_state: Default::default() };
            let tc = Transport { client: Client::builder().build(), keys: c, paced: false, last_state: Default::default() };
            let env = Envelope::new("a", protocol::Body::SyncNow);
            let ev = EventBuilder::new(Kind::Custom(KIND_MSG), ta.seal(&b.public_key(), &env).unwrap())
                .tag(Tag::public_key(b.public_key()))
                .finalize(&a)
                .unwrap();
            let got = tb.open(&ev).expect("recipient opens");
            assert_eq!(got.sender, a.public_key());
            assert_eq!(got.envelope, env);
            assert!(tc.open(&ev).is_none(), "third party cannot");
        });
    }

    // Found running the one-machine demo: a manual sync right after the
    // startup publish was rejected because both events had the same
    // created_at.
    #[test]
    fn back_to_back_state_publishes_are_both_accepted() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let relay = LocalRelay::new();
            relay.run().await.unwrap();
            let url = relay.url().await.to_string();
            let t = Transport::connect(Keys::generate(), &[url]).await.unwrap();
            let to = Keys::generate().public_key();
            let env = Envelope::new("a", protocol::Body::SyncNow);
            for _ in 0..3 {
                assert_eq!(t.put_state(&to, &env).await, Ok(1));
            }
            relay.shutdown();
        });
    }

    // A delivered transfer's chunks stayed in the relay's memory for a
    // day; the sender now deletes them once the receiver confirms.
    #[test]
    fn a_deleted_event_is_gone_from_the_relay() {
        use nostr_sdk::prelude::IntoNostrDatabase;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let db = nostr_memory::MemoryDatabase::unbounded().into_nostr_database();
            let relay = LocalRelay::builder().database(db.clone()).build();
            relay.run().await.unwrap();
            let url = relay.url().await.to_string();
            let t = Transport::connect(Keys::generate(), &[url]).await.unwrap();
            let to = Keys::generate().public_key();
            let env = Envelope::new("a", protocol::Body::SyncNow);
            let (_, kept) = t.send_bulk(&to, &env).await.unwrap();
            let (_, done) = t.send_bulk(&to, &env).await.unwrap();
            let held = |ids: Vec<EventId>| {
                let db = db.clone();
                async move { db.query(Filter::new().ids(ids)).await.unwrap().len() }
            };
            assert_eq!(held(vec![kept, done]).await, 2);
            t.delete(&[done]).await.unwrap();
            assert_eq!(held(vec![done]).await, 0, "deleted");
            assert_eq!(held(vec![kept]).await, 1, "only what was named");
            // Nobody else may delete our events.
            let other = Transport::connect(Keys::generate(), &[relay.url().await.to_string()]).await.unwrap();
            assert!(other.delete(&[kept]).await.is_err() || held(vec![kept]).await == 1);
            assert_eq!(held(vec![kept]).await, 1);
            relay.shutdown();
        });
    }

    // Found pulling a 3.5 MB folder through the fleet relay: the train
    // outran the relay's per-minute limit and the transfer was dropped.
    #[test]
    fn a_train_longer_than_the_relay_limit_waits_instead_of_failing() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let relay = LocalRelay::builder().rate_limit(RateLimit { max_reqs: 500, notes_per_minute: 120 }).build();
            relay.run().await.unwrap();
            let url = relay.url().await.to_string();
            let t = Transport::connect(Keys::generate(), &[url]).await.unwrap();
            let to = Keys::generate().public_key();
            let env = Envelope::new("a", protocol::Body::SyncNow);
            for i in 0..125 {
                assert_eq!(t.send_bulk(&to, &env).await.map(|(n, _)| n), Ok(1), "message {i}");
            }
            relay.shutdown();
        });
    }
}
