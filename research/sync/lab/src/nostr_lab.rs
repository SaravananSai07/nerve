use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nostr::nips::nip44;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{elapsed_ms, key_dir, now_ms, pad, stats};

#[derive(Serialize, Deserialize)]
struct Probe {
    seq: u32,
    t: u128,
    pad: String,
}

pub(crate) fn load_keys(name: &str) -> Result<Keys> {
    let path = key_dir()?.join(format!("{name}.key"));
    if let Ok(s) = std::fs::read_to_string(&path) {
        return Ok(Keys::parse(s.trim())?);
    }
    let k = Keys::generate();
    std::fs::write(&path, k.secret_key().to_secret_hex())?;
    Ok(k)
}

pub(crate) fn keys(name: &str) -> Result<()> {
    let k = load_keys(name)?;
    println!("{} {}", k.public_key().to_hex(), k.public_key().to_bech32()?);
    Ok(())
}

async fn client(keys: &Keys, relays: &[String]) -> Result<Client> {
    let _ = keys;
    let c = Client::builder().build();
    for r in relays {
        c.add_relay(r.as_str()).await.with_context(|| format!("add {r}"))?;
    }
    let t = Instant::now();
    c.connect().and_wait(Duration::from_secs(10)).await;
    eprintln!("connected to {} relays in {:.0} ms", relays.len(), elapsed_ms(t));
    Ok(c)
}

fn parse_pk(s: &str) -> Result<PublicKey> {
    Ok(PublicKey::parse(s)?)
}

pub(crate) async fn pong(key: &str, relays: &[String], dur: Duration) -> Result<()> {
    let keys = load_keys(key)?;
    let c = client(&keys, relays).await?;
    let me = keys.public_key();
    // Plain NIP-44 probes on any kind, addressed to us with a p-tag; plus gift wraps.
    let f = Filter::new().pubkey(me).limit(0);
    c.subscribe(f).await?;
    eprintln!("pong listening as {}", me.to_hex());
    let mut rx = c.notifications();
    let deadline = Instant::now() + dur;
    let mut seen = std::collections::HashSet::new();
    while Instant::now() < deadline {
        let n = match tokio::time::timeout(Duration::from_secs(5), rx.next()).await {
            Ok(Some(n)) => n,
            Ok(None) => break,
            Err(_) => continue,
        };
        let ClientNotification::Event { event, .. } = n else {
            continue;
        };
        if !seen.insert(event.id) || event.pubkey == me && event.kind != Kind::GiftWrap {
            continue;
        }
        if event.kind == Kind::GiftWrap {
            let Ok(un) = UnwrappedGift::from_gift_wrap(&keys, &event) else {
                continue;
            };
            if un.sender == me {
                continue;
            }
            let rumor = EventBuilder::new(Kind::Custom(14), un.rumor.content.clone())
                .finalize_unsigned(me);
            let gw = GiftWrapBuilder::new(un.sender, rumor).finalize(&keys)?;
            let _ = c.send_event(&gw).await;
            continue;
        }
        let Ok(plain) = nip44::decrypt(keys.secret_key(), &event.pubkey, &event.content) else {
            continue;
        };
        let ct = nip44::encrypt(keys.secret_key(), &event.pubkey, &plain, nip44::Version::V2)?;
        let b = EventBuilder::new(event.kind, ct)
            .tag(Tag::public_key(event.pubkey))
            .tag(Tag::event(event.id))
            .tag(Tag::expiration(Timestamp::now() + 600u64));
        let _ = c.send_event(&b.finalize(&keys)?).await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn ping(
    key: &str,
    peer: &str,
    relays: &[String],
    n: u32,
    size: usize,
    kind: u16,
    interval: Duration,
    giftwrap: bool,
) -> Result<()> {
    let keys = load_keys(key)?;
    let peer = parse_pk(peer)?;
    let c = client(&keys, relays).await?;
    let me = keys.public_key();
    c.subscribe(Filter::new().pubkey(me).limit(0)).await?;
    let mut rx = c.notifications();

    let sent_at: std::sync::Arc<tokio::sync::Mutex<HashMap<u32, Instant>>> = Default::default();
    let rtts: std::sync::Arc<tokio::sync::Mutex<HashMap<u32, f64>>> = Default::default();
    let per_relay_ok: std::sync::Arc<tokio::sync::Mutex<HashMap<String, (u32, u32)>>> =
        Default::default();

    // Receiver task: first arrival of each seq wins.
    let (sa, rt, kc, cc) = (sent_at.clone(), rtts.clone(), keys.clone(), c.clone());
    let recv = tokio::spawn(async move {
        while let Some(n) = rx.next().await {
            let ClientNotification::Event { event, .. } = n else {
                continue;
            };
            let plain = if event.kind == Kind::GiftWrap {
                match UnwrappedGift::from_gift_wrap(&kc, &event) {
                    Ok(u) if u.sender != kc.public_key() => u.rumor.content,
                    _ => continue,
                }
            } else {
                if event.pubkey == kc.public_key() {
                    continue;
                }
                match nip44::decrypt(kc.secret_key(), &event.pubkey, &event.content) {
                    Ok(p) => p,
                    Err(_) => continue,
                }
            };
            let Ok(p) = serde_json::from_str::<Probe>(&plain) else {
                continue;
            };
            let t0 = sa.lock().await.get(&p.seq).copied();
            if let Some(t0) = t0 {
                rt.lock().await.entry(p.seq).or_insert_with(|| elapsed_ms(t0));
            }
        }
    });

    let mut publish_ms = Vec::new();
    for seq in 0..n {
        let body = serde_json::to_string(&Probe {
            seq,
            t: now_ms(),
            pad: pad(size),
        })?;
        let t0 = Instant::now();
        sent_at.lock().await.insert(seq, t0);
        let out = if giftwrap {
            let rumor = EventBuilder::new(Kind::Custom(14), body).finalize_unsigned(me);
            let gw = GiftWrapBuilder::new(peer, rumor).finalize(&keys)?;
            c.send_event(&gw).await.map(|o| (o.success, o.failed))
        } else {
            let ct = nip44::encrypt(keys.secret_key(), &peer, &body, nip44::Version::V2)?;
            let b = EventBuilder::new(Kind::Custom(kind), ct)
                .tag(Tag::public_key(peer))
                .tag(Tag::expiration(Timestamp::now() + 600u64));
            c.send_event(&b.finalize(&keys)?).await.map(|o| (o.success, o.failed))
        };
        publish_ms.push(elapsed_ms(t0));
        match out {
            Ok((ok, failed)) => {
                let mut pr = per_relay_ok.lock().await;
                for (r, _) in ok {
                    pr.entry(r.to_string()).or_default().0 += 1;
                }
                for (r, why) in failed {
                    let e = pr.entry(r.to_string()).or_default();
                    e.1 += 1;
                    if e.1 == 1 {
                        eprintln!("relay {r} rejected: {why}");
                    }
                }
            }
            Err(e) => eprintln!("send error: {e}"),
        }
        tokio::time::sleep(interval).await;
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    recv.abort();
    let label = format!(
        "nostr kind={} giftwrap={giftwrap} size={size} relays={}",
        if giftwrap { 1059 } else { kind },
        relays.join(",")
    );
    stats::report(&label, n, rtts.lock().await.values().copied().collect());
    stats::report("  publish-ack(OK from all relays)", n, publish_ms);
    for (r, (ok, bad)) in per_relay_ok.lock().await.iter() {
        println!("  relay {r}: accepted={ok} rejected={bad}");
    }
    Ok(())
}

pub(crate) async fn sizeprobe(key: &str, relays: &[String], kind: u16) -> Result<()> {
    let keys = load_keys(key)?;
    for r in relays {
        let c = client(&keys, std::slice::from_ref(r)).await?;
        let mut line = format!("{r}:");
        for kib in [8usize, 32, 60, 64, 100, 128, 200, 256, 500, 1000] {
            let b = EventBuilder::new(Kind::Custom(kind), pad(kib * 1024))
                .tag(Tag::expiration(Timestamp::now() + 300u64));
            let res = c.send_event(&b.finalize(&keys)?).await;
            let ok = matches!(&res, Ok(o) if !o.success.is_empty());
            line.push_str(&format!(" {kib}K={}", if ok { "ok" } else { "NO" }));
            if !ok {
                if let Ok(o) = res {
                    if let Some((_, why)) = o.failed.iter().next() {
                        line.push_str(&format!("({})", why.chars().take(60).collect::<String>()));
                    }
                }
                break;
            }
        }
        println!("{line}");
        c.disconnect().await;
    }
    Ok(())
}

pub(crate) async fn relay(port: u16) -> Result<()> {
    let relay = LocalRelay::builder()
        .addr("0.0.0.0".parse()?)
        .port(port)
        .build();
    relay.run().await?;
    eprintln!("relay listening on ws://0.0.0.0:{port}");
    tokio::signal::ctrl_c().await?;
    Ok(())
}
