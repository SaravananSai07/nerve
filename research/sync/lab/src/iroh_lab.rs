use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use iroh::endpoint::{presets, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointId, RelayMode, SecretKey};

use crate::{elapsed_ms, key_dir, pad, stats};

const ALPN: &[u8] = b"nerve-lab/echo/0";

fn secret(name: &str) -> Result<SecretKey> {
    let p = key_dir()?.join(format!("iroh-{name}.key"));
    if let Ok(s) = std::fs::read_to_string(&p) {
        let b: [u8; 32] = hex::decode(s.trim())?
            .try_into()
            .map_err(|_| anyhow!("bad key"))?;
        return Ok(SecretKey::from_bytes(&b));
    }
    let k = SecretKey::generate();
    std::fs::write(&p, hex::encode(k.to_bytes()))?;
    Ok(k)
}

#[derive(Debug, Clone)]
struct Echo;

impl ProtocolHandler for Echo {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        eprintln!("accepted {}", conn.remote_id());
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            tokio::spawn(async move {
                // Length-prefixed frames, echoed until the peer finishes.
                let mut len = [0u8; 4];
                while recv.read_exact(&mut len).await.is_ok() {
                    let n = u32::from_be_bytes(len) as usize;
                    let mut buf = vec![0u8; n];
                    if recv.read_exact(&mut buf).await.is_err() {
                        break;
                    }
                    // Bulk frames (flag bit) are acknowledged with 4 bytes only.
                    if n > 1 << 20 {
                        let _ = send.write_all(&(n as u32).to_be_bytes()).await;
                        let _ = send.write_all(&[]).await;
                        let _ = send.write_all(b"").await;
                        continue;
                    }
                    let _ = send.write_all(&len).await;
                    let _ = send.write_all(&buf).await;
                }
                let _ = send.finish();
            });
        }
        Ok(())
    }
}

pub(crate) async fn pong(no_relay: bool, dur: Duration) -> Result<()> {
    let mut b = Endpoint::builder(presets::N0)
        .secret_key(secret("pong")?)
        .alpns(vec![ALPN.to_vec()]);
    if no_relay {
        b = b.relay_mode(RelayMode::Disabled);
    }
    let ep = b.bind().await?;
    let t = Instant::now();
    ep.online().await;
    eprintln!("online in {:.0} ms", elapsed_ms(t));
    println!("{}", ep.id());
    println!("addr: {:?}", ep.addr());
    let router = Router::builder(ep).accept(ALPN, Echo).spawn();
    tokio::time::sleep(dur).await;
    router.shutdown().await?;
    Ok(())
}

pub(crate) async fn ping(
    peer: &str,
    n: u32,
    size: usize,
    bulk_mib: usize,
    relay_only: bool,
    no_relay: bool,
) -> Result<()> {
    let peer: EndpointId = peer.parse()?;
    let mut b = Endpoint::builder(presets::N0).secret_key(secret("ping")?);
    if relay_only {
        b = b.clear_ip_transports();
    }
    if no_relay {
        b = b.relay_mode(RelayMode::Disabled);
    }
    let ep = b.bind().await?;
    let t = Instant::now();
    // Dial by key only: address lookup (n0 DNS/pkarr) resolves relay + direct addrs.
    let conn = ep.connect(peer, ALPN).await?;
    println!("connect (handshake incl. lookup) {:.0} ms", elapsed_ms(t));
    let (mut send, mut recv) = conn.open_bi().await?;
    let mut rtts = Vec::new();
    let payload = pad(size).into_bytes();
    for i in 0..n {
        let t0 = Instant::now();
        send.write_all(&(payload.len() as u32).to_be_bytes()).await?;
        send.write_all(&payload).await?;
        let mut len = [0u8; 4];
        recv.read_exact(&mut len).await?;
        let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
        recv.read_exact(&mut buf).await?;
        rtts.push(elapsed_ms(t0));
        if i == 0 || i == n - 1 {
            for p in conn.paths().iter() {
                println!(
                    "  path[{}] {:?} selected={} relay={} rtt={:?}",
                    i,
                    p.remote_addr(),
                    p.is_selected(),
                    p.is_relay(),
                    p.rtt()
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    stats::report(
        &format!("iroh size={size} relay_only={relay_only} no_relay={no_relay}"),
        n,
        rtts,
    );
    if bulk_mib > 0 {
        let chunk = pad(4 << 20).into_bytes();
        let total = bulk_mib << 20;
        let t0 = Instant::now();
        let mut sent = 0;
        while sent < total {
            let c = &chunk[..chunk.len().min(total - sent)];
            // > 1 MiB frames are acked with just their length.
            let c = if c.len() <= 1 << 20 { &chunk[..(1 << 20) + 1] } else { c };
            send.write_all(&(c.len() as u32).to_be_bytes()).await?;
            send.write_all(c).await?;
            let mut ack = [0u8; 4];
            recv.read_exact(&mut ack).await?;
            sent += c.len();
        }
        let s = t0.elapsed().as_secs_f64();
        println!(
            "RESULT iroh bulk {} MiB in {:.2}s = {:.1} MiB/s",
            sent >> 20,
            s,
            (sent as f64 / (1 << 20) as f64) / s
        );
    }
    send.finish()?;
    conn.close(0u32.into(), b"bye");
    ep.close().await;
    Ok(())
}
