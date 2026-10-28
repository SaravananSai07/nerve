//! nsync-lab: measurement harness for nerve's cross-device sync research.
//!
//! Every experiment is a ping/pong between two devices so RTT is measured on
//! one clock (no cross-device clock skew in the numbers).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

mod iroh_lab;
mod nostr_lab;
mod stats;

#[derive(Parser)]
#[command(name = "nsync-lab")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create (or show) a named Nostr keypair under ~/.nsync-lab/<name>.key
    Keys { name: String },
    /// Answer pings: decrypt, echo back to sender (encrypted) on the same kind.
    Pong {
        #[arg(long)]
        key: String,
        #[arg(long, value_delimiter = ',')]
        relays: Vec<String>,
        #[arg(long, default_value_t = 600)]
        secs: u64,
    },
    /// Send N pings to a peer and report RTT stats.
    Ping {
        #[arg(long)]
        key: String,
        /// Peer public key (hex or npub)
        #[arg(long)]
        peer: String,
        #[arg(long, value_delimiter = ',')]
        relays: Vec<String>,
        #[arg(long, default_value_t = 30)]
        n: u32,
        /// Payload size in bytes (before encryption)
        #[arg(long, default_value_t = 256)]
        size: usize,
        /// Event kind: 21059 (ephemeral) or 1059-ish regular kinds, etc.
        #[arg(long, default_value_t = 21059)]
        kind: u16,
        #[arg(long, default_value_t = 400)]
        interval_ms: u64,
        /// Wrap in NIP-59 gift wrap (kind 1059) instead of a plain NIP-44 event
        #[arg(long)]
        giftwrap: bool,
    },
    /// Probe the largest event each relay accepts.
    Sizeprobe {
        #[arg(long)]
        key: String,
        #[arg(long, value_delimiter = ',')]
        relays: Vec<String>,
        #[arg(long, default_value_t = 20001)]
        kind: u16,
    },
    /// Run an embedded relay (nostr-relay-builder) on 0.0.0.0:<port>, in memory.
    Relay {
        #[arg(long, default_value_t = 7777)]
        port: u16,
    },
    /// iroh: accept echo connections; prints the endpoint id to dial.
    IrohPong {
        /// Disable the n0 relay so only direct paths can be used
        #[arg(long)]
        no_relay: bool,
        #[arg(long, default_value_t = 600)]
        secs: u64,
    },
    /// iroh: dial a peer by endpoint id and measure RTT over one QUIC stream.
    IrohPing {
        peer: String,
        #[arg(long, default_value_t = 30)]
        n: u32,
        #[arg(long, default_value_t = 256)]
        size: usize,
        /// Bulk transfer size in MiB after the RTT loop (0 to skip)
        #[arg(long, default_value_t = 0)]
        bulk_mib: usize,
        /// Disable discovery of LAN/direct addresses: relay-only path
        #[arg(long)]
        relay_only: bool,
        #[arg(long)]
        no_relay: bool,
    },
}

pub(crate) fn key_dir() -> Result<PathBuf> {
    let d = dirs_home()?.join(".nsync-lab");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME not set")
}

pub(crate) fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

pub(crate) fn elapsed_ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

pub(crate) fn pad(size: usize) -> String {
    use rand::Rng;
    // Random alphanumerics so relays/compression can't shrink it.
    let mut rng = rand::rng();
    (0..size)
        .map(|_| rng.sample(rand::distr::Alphanumeric) as char)
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Keys { name } => nostr_lab::keys(&name),
        Cmd::Pong { key, relays, secs } => {
            nostr_lab::pong(&key, &relays, Duration::from_secs(secs)).await
        }
        Cmd::Ping {
            key,
            peer,
            relays,
            n,
            size,
            kind,
            interval_ms,
            giftwrap,
        } => {
            if relays.is_empty() {
                bail!("--relays required");
            }
            nostr_lab::ping(
                &key,
                &peer,
                &relays,
                n,
                size,
                kind,
                Duration::from_millis(interval_ms),
                giftwrap,
            )
            .await
        }
        Cmd::Sizeprobe { key, relays, kind } => nostr_lab::sizeprobe(&key, &relays, kind).await,
        Cmd::Relay { port } => nostr_lab::relay(port).await,
        Cmd::IrohPong { no_relay, secs } => {
            iroh_lab::pong(no_relay, Duration::from_secs(secs)).await
        }
        Cmd::IrohPing {
            peer,
            n,
            size,
            bulk_mib,
            relay_only,
            no_relay,
        } => iroh_lab::ping(&peer, n, size, bulk_mib, relay_only, no_relay).await,
    }
}
