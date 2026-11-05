//! What waits for a person's yes, and the device group it guards:
//!
//! - a device joining with a code this one showed;
//! - a folder another device shares that no folder rule here covers.
//!
//! Approvals sit in this device's state, so any connected device can
//! answer them (`Body::Decide`). They are kept in `<sync>/approvals.json`.
//! A yes to a join introduces the new device to the devices it joins;
//! `forget` and `link` change the group later.

use nostr_sdk::prelude::PublicKey;
use serde::{Deserialize, Serialize};

use super::{Agent, Inbound};
use crate::sync::folders;
use crate::sync::protocol::{self, Approval, Asked, Body, Decision, Met};
use crate::sync::{connect, fleet};

/// What the UI and CLI ask about devices and approvals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum FleetReq {
    /// Show a code for a new device. `with`, as `nerve connect --with`:
    /// `all`, `now` or device names.
    Invite { with: String },
    /// Withdraw the code, unused.
    Uninvite { secret: String },
    /// Answer an approval held here or on another device (an id prefix).
    Decide(Decision),
    /// Stop trusting a device, here and on every other device.
    Forget { device: String },
    /// Connect `device` with these devices (none: with all of them, and
    /// with devices that join later).
    Link { device: String, with: Vec<String> },
    /// Put back a version a sync replaced (an id prefix).
    Restore { id: String },
}

/// One id among many by a unique prefix (approval and version ids).
pub(super) fn by_prefix<'a, T>(items: &'a [T], id: &str, of: impl Fn(&T) -> &str) -> Result<&'a T, String> {
    let hits: Vec<&T> = items.iter().filter(|t| !id.is_empty() && of(t).starts_with(id)).collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("nothing matches {id}")),
        _ => Err(format!("{id} matches more than one; give more of it")),
    }
}

fn approvals_file(sync_dir: &std::path::Path) -> std::path::PathBuf {
    sync_dir.join("approvals.json")
}

impl Agent {
    pub(super) fn approvals_load(&mut self) {
        self.approvals = std::fs::read_to_string(approvals_file(&self.ctx.sync_dir))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // A join can't outlive its code: the joiner stopped waiting.
        let stale = crate::util::now_secs().saturating_sub(connect::INVITE_TTL_SECS);
        self.approvals.retain(|a| !matches!(a.what, Asked::Join { .. }) || a.at >= stale);
    }

    fn approvals_save(&mut self) {
        if let Err(e) = std::fs::write(
            approvals_file(&self.ctx.sync_dir),
            serde_json::to_string(&self.approvals).unwrap_or_default(),
        ) {
            self.note(format!("could not save approvals.json: {e}"));
        }
        self.publish_now = true;
    }

    /// Hold `what` until a person answers it.
    pub(super) fn hold(&mut self, from: &PublicKey, what: Asked) {
        let id = protocol::new_id();
        let line = match &what {
            Asked::Join { name, words, .. } => format!("{name} asks to connect ({words})"),
            Asked::Folder { set, .. } => {
                format!("{} asks to sync {}", self.peer_name(from).unwrap_or_default(), set.name)
            }
        };
        self.note(format!("{line}: waiting for a yes (nerve allow {})", &id[..6]));
        self.approvals.push(Approval { id, at: crate::util::now_secs(), from: from.to_hex(), what });
        self.approvals_save();
    }

    /// `from` no longer asks to share folder `name`.
    pub(super) fn unhold(&mut self, from: &PublicKey, name: &str) {
        let (hex, before) = (from.to_hex(), self.approvals.len());
        self.approvals.retain(|a| !(a.from == hex && matches!(&a.what, Asked::Folder { set, .. } if set.name == name)));
        if self.approvals.len() < before {
            self.approvals_save();
        }
    }

    pub(super) async fn fleet_request(&mut self, r: FleetReq) -> Result<String, String> {
        match r {
            FleetReq::Invite { with } => self.invite(&with),
            FleetReq::Uninvite { secret } => {
                connect::cancel(&self.ctx.sync_dir, &secret);
                self.invite = None;
                self.publish_now = true;
                Ok("code withdrawn".into())
            }
            FleetReq::Decide(d) => self.decide(d).await,
            FleetReq::Forget { device } => self.forget(&device, true).await,
            FleetReq::Link { device, with } => self.link(&device, with).await,
            FleetReq::Restore { id } => self.restore(&id),
        }
    }

    fn invite(&mut self, with: &str) -> Result<String, String> {
        let me = self.t.keys.public_key().to_hex();
        let f = &self.ctx.fleet;
        let (offer, later) = crate::sync::connect::offer(f, Some(with))?;
        let inv =
            connect::create(&self.ctx.sync_dir, &f.device, &me, &f.relays, offer, later).map_err(|e| e.to_string())?;
        let code = inv.encode();
        self.invite = Some((code.clone(), crate::util::now_secs() + connect::INVITE_TTL_SECS));
        self.publish_now = true;
        Ok(format!("code ready: {code}"))
    }

    /// A stranger proves it holds a code this device showed.
    pub(super) async fn on_pair_request(
        &mut self,
        inb: &Inbound,
        secret: &str,
        name: &str,
        relays: &[String],
        want: Option<&[String]>,
    ) {
        let re = inb.envelope.id.clone();
        let Some(t) = connect::consume(&self.ctx.sync_dir, secret) else {
            self.note("refused a join: its code is unknown, used or expired");
            // Tell the joiner rather than leave it waiting. Nothing about
            // this device is revealed beyond what the code already held.
            return self
                .ack(&inb.sender, &re, false, "that code was already used or has expired; ask for a new one")
                .await;
        };
        self.invite = None;
        let words = connect::words(&self.t.keys.public_key().to_hex(), &inb.sender.to_hex(), secret);
        let join = t.offer.into_iter().filter(|d| want.is_none_or(|w| w.contains(d))).collect();
        let name = connect::unique_name(name, |_| false);
        self.hold(&inb.sender, Asked::Join { name, words, join, later: t.later, relays: relays.to_vec(), re });
    }

    /// Answer an approval: ours, or one a peer holds.
    async fn decide(&mut self, mut d: Decision) -> Result<String, String> {
        // (holder: None here, id), from this device and every peer.
        let peers = self.view.lock().map(|v| v.peers.clone()).unwrap_or_default();
        let mut held: Vec<(Option<String>, String)> = self.approvals.iter().map(|a| (None, a.id.clone())).collect();
        held.extend(peers.iter().flat_map(|p| {
            p.state.iter().flat_map(|s| s.approvals.iter().map(|a| (Some(p.name.clone()), a.id.clone())))
        }));
        let (holder, id) = by_prefix(&held, &d.id, |(_, id)| id)?.clone();
        d.id = id;
        let Some(device) = holder else {
            let me = self.t.keys.public_key();
            return self.on_decide(&me, d).await;
        };
        let pk = self.peer_key(&device).ok_or_else(|| format!("unknown device {device}"))?;
        self.reply(&pk, Body::Decide(d)).await;
        Ok(format!("answer sent to {device}"))
    }

    /// A person answered an approval held here (`from`: where).
    pub(super) async fn on_decide(&mut self, from: &PublicKey, d: Decision) -> Result<String, String> {
        let i = self.approvals.iter().position(|a| a.id == d.id).ok_or("that request was already answered")?;
        let a = self.approvals.remove(i);
        self.approvals_save();
        let who = self.peer_name(from).unwrap_or_default();
        let asker = PublicKey::parse(&a.from).map_err(|e| e.to_string())?;
        match a.what {
            Asked::Join { name, join, later, relays, re, .. } if d.yes => {
                self.admit(asker, &name, join, later, &relays, &re).await
            }
            Asked::Join { name, re, .. } => {
                self.ack(&asker, &re, false, format!("{} refused the join", self.ctx.fleet.device)).await;
                Ok(format!("{who} refused {name}"))
            }
            Asked::Folder { set, lands, re } if d.yes => {
                // A path typed on another device: `~` is this device's home.
                let dest = crate::paths::untilde(&d.at.unwrap_or(lands));
                if !dest.is_absolute() {
                    return Err(format!("{} is not an absolute path", dest.display()));
                }
                let name = set.name.clone();
                let res = self.folder_join(&asker, set, dest).await;
                self.ack(&asker, &re, res.is_ok(), res.clone().unwrap_or_else(|e| e)).await;
                // The rules belong to the folder: every member gets them.
                if let (Ok(_), Some(o)) = (&res, d.others) {
                    self.folder_others(&name, o).await?;
                }
                res
            }
            Asked::Folder { set, re, .. } => {
                self.ack(&asker, &re, false, format!("{} refused to sync {}", self.ctx.fleet.device, set.name)).await;
                Ok(format!("{who} refused {}", set.name))
            }
        }
    }

    /// Trust the joiner, introduce it to the devices it joins, and tell it
    /// who they are.
    async fn admit(
        &mut self,
        pk: PublicKey,
        wanted: &str,
        join: Vec<String>,
        later: bool,
        relays: &[String],
        re: &str,
    ) -> Result<String, String> {
        let me = self.ctx.fleet.device.clone();
        let relays: Vec<String> = relays.iter().filter(|r| r.starts_with("ws://")).take(8).cloned().collect();
        let name = self.trust(&pk, wanted, later, relays).await?;
        let met = Met { name: wanted.to_string(), key: pk.to_hex(), later };
        let mut devices = Vec::new();
        for d in join {
            let Some(k) = self.peer_key(&d).filter(|k| *k != pk) else { continue };
            self.reply(&k, Body::Introduce(met.clone())).await;
            devices.push(Met { later: !self.ctx.fleet.closed.contains(&d), name: d, key: k.to_hex() });
        }
        let joined = devices.iter().map(|m| m.name.clone()).collect::<Vec<_>>();
        self.reply(&pk, Body::Welcome { re: re.to_string(), devices }).await;
        self.ack(&pk, re, true, format!("{me} now trusts '{name}'")).await;
        Ok(match joined.as_slice() {
            [] => format!("connected {name}"),
            _ => format!("connected {name}, and introduced it to {}", crate::util::text::list_join(&joined)),
        })
    }

    /// Add `pk` to the peers (a private ws:// relay it knows too), or
    /// update whether devices joining later meet it. Returns its name here.
    async fn trust(
        &mut self,
        pk: &PublicKey,
        wanted: &str,
        later: bool,
        relays: Vec<String>,
    ) -> Result<String, String> {
        let (pk, wanted) = (*pk, wanted.to_string());
        self.edit_fleet(move |f| {
            let name = match f.peer_keys().into_iter().find(|(_, k)| *k == pk) {
                Some((n, _)) => n,
                None => {
                    let taken = |n: &str| f.peers.contains_key(n) || n == f.device;
                    connect::unique_name(&wanted, taken)
                }
            };
            f.peers.insert(name.clone(), fleet::npub(&pk));
            f.closed.retain(|c| *c != name);
            if !later {
                f.closed.push(name.clone());
            }
            for r in relays {
                if !f.relays.contains(&r) {
                    f.relays.push(r);
                }
            }
            Ok(name)
        })
        .await
    }

    /// A device both of us trust vouches for another one.
    pub(super) async fn on_met(&mut self, from: &str, m: Met) {
        let Ok(pk) = PublicKey::parse(&m.key) else { return };
        if pk == self.t.keys.public_key() {
            return;
        }
        let known = self.peer_name(&pk);
        match self.trust(&pk, &m.name, m.later, Vec::new()).await {
            Ok(name) if known.is_none() => self.note(format!("connected to {name}, through {from}")),
            Ok(_) => {}
            Err(e) => self.note(format!("could not connect to {}: {e}", m.name)),
        }
    }

    /// Stop trusting `device` here, and with `tell`, everywhere else too.
    pub(super) async fn forget(&mut self, device: &str, tell: bool) -> Result<String, String> {
        let pk = self
            .peer_key(device)
            .filter(|k| *k != self.t.keys.public_key())
            .ok_or_else(|| format!("unknown device {device}"))?;
        let d = device.to_string();
        let gone = self
            .edit_fleet(move |f| {
                f.peers.remove(&d);
                f.closed.retain(|c| *c != d);
                f.folders.iter_mut().for_each(|c| c.with.retain(|w| *w != d));
                let gone: Vec<String> =
                    f.folders.iter().filter(|c| c.with.is_empty()).map(|c| c.name.clone()).collect();
                f.folders.retain(|c| !c.with.is_empty());
                Ok(gone)
            })
            .await?;
        for name in gone {
            folders::Book::forget(&self.ctx.sync_dir, &name);
        }
        let hex = pk.to_hex();
        self.approvals.retain(|a| a.from != hex);
        self.approvals_save();
        if tell {
            for (_, k) in self.peers.clone() {
                self.reply(&k, Body::Forget { key: hex.clone() }).await;
            }
        }
        Ok(format!("forgot {device}"))
    }

    /// A peer forgot a device: so do we. Never ourselves or the sender.
    pub(super) async fn on_forget(&mut self, from: &PublicKey, key: &str) {
        let Some(name) = PublicKey::parse(key).ok().filter(|k| k != from).and_then(|k| self.peer_name(&k)) else {
            return;
        };
        if name != self.ctx.fleet.device {
            let by = self.peer_name(from).unwrap_or_default();
            if let Ok(m) = self.forget(&name, false).await {
                self.note(format!("{m}: {by} forgot it"));
            }
        }
    }

    /// Introduce `device` to `with` (none: every device, now and later).
    async fn link(&mut self, device: &str, with: Vec<String>) -> Result<String, String> {
        let me = self.t.keys.public_key();
        let dk = self.peer_key(device).filter(|k| *k != me).ok_or_else(|| format!("unknown device {device}"))?;
        let all = with.is_empty();
        let others: Vec<String> =
            if all { self.peers.iter().map(|(n, _)| n.clone()).filter(|n| n != device).collect() } else { with };
        if all {
            self.trust(&dk, device, true, Vec::new()).await?;
        }
        let met = |name: &str, k: PublicKey, closed: &[String]| Met {
            name: name.to_string(),
            key: k.to_hex(),
            later: !closed.contains(&name.to_string()),
        };
        for o in &others {
            let ok = self.peer_key(o).filter(|k| *k != me && *k != dk).ok_or_else(|| format!("unknown device {o}"))?;
            let closed = self.ctx.fleet.closed.clone();
            self.reply(&ok, Body::Introduce(met(device, dk, &closed))).await;
            self.reply(&dk, Body::Introduce(met(o, ok, &closed))).await;
        }
        Ok(if all {
            format!("{device} connects to every device, now and later")
        } else {
            format!("{device} connects to {}", crate::util::text::list_join(&others))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_match_by_a_unique_prefix() {
        let ids = ["3f9a2c11".to_string(), "3fb00000".to_string(), "71bd0400".to_string()];
        assert_eq!(by_prefix(&ids, "71", |s| s).unwrap(), "71bd0400");
        assert_eq!(by_prefix(&ids, "3f9", |s| s).unwrap(), "3f9a2c11");
        assert!(by_prefix(&ids, "3f", |s| s).unwrap_err().contains("more than one"));
        assert!(by_prefix(&ids, "", |s| s).is_err(), "an empty id matches nothing");
        assert!(by_prefix(&ids, "zz", |s| s).is_err());
    }
}
