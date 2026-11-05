//! Input that takes over the keys: dialogs, confirmations and paste.

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyModifiers};

use super::{App, Confirm};
use crate::sync::folders::{Others, When};
use crate::tui::dialog::{Connect, Dialog, FolderStep, ReviewStep, StartStep, WhoMode};
use crate::tui::editor::Edit;

/// Add `item` to `set`, or take it out if it is there.
pub(super) fn toggle(set: &mut BTreeSet<String>, item: &str) {
    if !set.remove(item) {
        set.insert(item.to_string());
    }
}

impl App {
    /// A paste lands whole in a prompt box, line breaks and all; any other
    /// text field takes it as typed, one line.
    pub(super) fn paste(&mut self, text: &str) {
        let field = self.ui.dialog.is_some() || self.ui.finding;
        let editor = match &mut self.ui.dialog {
            Some(Dialog::Start { step: StartStep::Prompt, prompt, .. }) => Some(prompt),
            Some(_) => None,
            None => self.ui.compose.as_mut().filter(|_| !self.ui.finding && self.ui.confirm.is_none()),
        };
        match editor {
            Some(e) => e.insert(text),
            // Into a text field; never as keys into a list, where letters act.
            None if field => {
                for c in text.chars().filter(|c| !c.is_control()) {
                    self.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
                }
            }
            None => {}
        }
    }

    pub(super) fn confirm_key(&mut self, c: Confirm, code: KeyCode) {
        match (c, code) {
            (Confirm::Stop { key, .. }, KeyCode::Char('y' | 'Y')) => self.stop(&key),
            (Confirm::Capture { key }, KeyCode::Char('y' | 'Y')) => self.capture(&key),
            (Confirm::StopSync { name, path }, KeyCode::Char('y' | 'Y')) => self.stop_sync(name, path),
            (Confirm::Forget { device }, KeyCode::Char('y' | 'Y')) => self.forget(device),
            (Confirm::Capture { key }, KeyCode::Char('d' | 'D')) => {
                self.prefs.preview_flicker_accepted = true;
                self.prefs.save(&self.paths);
                self.capture(&key);
            }
            // Anything else, Enter included, keeps things as they are.
            _ => {}
        }
    }

    pub(super) fn dialog_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(mut d) = self.ui.dialog.take() else { return };
        let down = matches!(code, KeyCode::Char('j') | KeyCode::Down);
        let up = matches!(code, KeyCode::Char('k') | KeyCode::Up);
        if code == KeyCode::Esc {
            match &mut d {
                Dialog::AddFolder { step, when, i, .. } if *step != FolderStep::Folder => {
                    *step = step.back().unwrap_or(FolderStep::Folder);
                    *i = if *step == FolderStep::When { *when } else { 0 };
                }
                Dialog::Review { step: step @ ReviewStep::Rules, i, .. } => (*step, *i) = (ReviewStep::Where, 0),
                Dialog::Start { step: step @ StartStep::Folder, .. } => *step = StartStep::Device,
                Dialog::Start { step: step @ StartStep::Prompt, .. } => *step = StartStep::Folder,
                Dialog::Connect(Connect::Enter { .. }) => d = Dialog::Connect(Connect::Choose(1)),
                Dialog::Connect(Connect::Pick { code, .. }) => {
                    d = Dialog::Connect(Connect::Enter { buf: std::mem::take(code), err: None })
                }
                Dialog::Connect(Connect::Code { .. }) => return self.uninvite(),
                _ => return,
            }
            self.ui.dialog = Some(d);
            return;
        }
        match &mut d {
            Dialog::Keys => return,
            Dialog::Notify { devices, set, i, .. } => {
                if down {
                    *i = (*i + 1).min(devices.len().saturating_sub(1));
                } else if up {
                    *i = i.saturating_sub(1);
                } else if code == KeyCode::Char(' ') {
                    toggle(set, &devices[*i].0);
                } else if code == KeyCode::Enter {
                    return self.save_notify(d);
                }
            }
            Dialog::Keep { folder, .. } => {
                if down {
                    *folder = true;
                } else if up {
                    *folder = false;
                } else if code == KeyCode::Enter {
                    return self.keep_private(d);
                }
            }
            Dialog::Rename { buf, .. } => match code {
                KeyCode::Enter => return self.commit_rename(d),
                KeyCode::Backspace => {
                    buf.pop();
                }
                // Matches the width the box draws.
                KeyCode::Char(c) if buf.chars().count() < 48 => buf.push(c),
                _ => {}
            },
            Dialog::Handoff { peers, .. } => {
                if let KeyCode::Char(c @ '1'..='9') = code {
                    if let Some(peer) = peers.get(c as usize - '1' as usize).cloned() {
                        return self.handoff(d, peer);
                    }
                }
            }
            Dialog::Start { step: step @ StartStep::Device, devices, i, .. } => {
                if down || up {
                    *i = if down { (*i + 1).min(devices.len().saturating_sub(1)) } else { i.saturating_sub(1) };
                } else if code == KeyCode::Enter {
                    *step = StartStep::Folder;
                }
            }
            Dialog::Start { step: step @ StartStep::Folder, dir, .. } => match code {
                KeyCode::Backspace => {
                    dir.pop();
                }
                KeyCode::Char(c) if dir.len() + c.len_utf8() <= 1024 => dir.push(c),
                KeyCode::Enter if !dir.trim().is_empty() => *step = StartStep::Prompt,
                _ => {}
            },
            Dialog::Start { prompt, .. } => {
                if prompt.key(code, mods) == Edit::Submit && !prompt.text().trim().is_empty() {
                    return self.start_session(d);
                }
            }
            Dialog::AddFolder { step, cands, pick, devices, set, when, i } => {
                let n = match step {
                    FolderStep::Folder => cands.len(),
                    FolderStep::Devices => devices.len(),
                    FolderStep::When | FolderStep::Rules => When::ALL.len(),
                };
                if down {
                    *i = (*i + 1).min(n.saturating_sub(1));
                } else if up {
                    *i = i.saturating_sub(1);
                } else if code == KeyCode::Char(' ') && *step == FolderStep::Devices {
                    // This device always keeps its copy.
                    let dev = devices[*i].0.clone();
                    if dev != self.me {
                        toggle(set, &dev);
                    }
                } else if code == KeyCode::Enter {
                    match step {
                        FolderStep::Folder if cands.is_empty() => return,
                        FolderStep::Folder => (*pick, *step, *i) = (*i, FolderStep::Devices, 0),
                        FolderStep::Devices if set.len() < 2 => {}
                        FolderStep::Devices => (*step, *i) = (FolderStep::When, 0),
                        FolderStep::When => (*when, *step, *i) = (*i, FolderStep::Rules, 0),
                        FolderStep::Rules => return self.add_folder(d),
                    }
                }
            }
            Dialog::Review { key, folder: None, .. } => match code {
                KeyCode::Char('y') => return self.decide(&key.clone(), true, None, None),
                KeyCode::Char('n') => return self.decide(&key.clone(), false, None, None),
                _ => {}
            },
            Dialog::Review { key, folder: Some(f), step, i, .. } => {
                let choosing = *step == ReviewStep::Where;
                let typing = choosing && *i == 1;
                match code {
                    KeyCode::Down if choosing => *i = 1,
                    KeyCode::Up if choosing => *i = 0,
                    KeyCode::Char('j') | KeyCode::Down if !typing => {
                        *i = (*i + 1).min(if choosing { 1 } else { Others::ALL.len() - 1 })
                    }
                    KeyCode::Char('k') | KeyCode::Up if !typing => *i = i.saturating_sub(1),
                    KeyCode::Char('n') if !typing => return self.decide(&key.clone(), false, None, None),
                    KeyCode::Backspace if typing => {
                        f.other.pop();
                    }
                    KeyCode::Char(c) if typing && f.other.chars().count() < 200 => f.other.push(c),
                    KeyCode::Enter if choosing => {
                        if *i == 0 {
                            f.other.clear();
                        }
                        (*step, *i) = (ReviewStep::Rules, 0);
                    }
                    KeyCode::Enter => {
                        let at = Some(f.spot(1).to_string()).filter(|s| *s != f.lands);
                        return self.decide(&key.clone(), true, at, Some(Others::ALL[*i]));
                    }
                    _ => {}
                }
            }
            Dialog::Rules { name, i, .. } => {
                if down || up {
                    *i = if down { (*i + 1).min(Others::ALL.len() - 1) } else { i.saturating_sub(1) };
                } else if code == KeyCode::Enter {
                    return self.set_rules(name.clone(), *i);
                }
            }
            Dialog::Versions { list, i, .. } => {
                if down || up {
                    *i = if down { (*i + 1).min(list.len().saturating_sub(1)) } else { i.saturating_sub(1) };
                } else if code == KeyCode::Enter {
                    if let Some((id, _)) = list.get(*i) {
                        return self.restore(id.clone());
                    }
                }
            }
            Dialog::Who { link, devices, set, mode, i } => {
                if down || up {
                    *i = if down { (*i + 1).min(devices.len() + 1) } else { i.saturating_sub(1) };
                } else if code == KeyCode::Char(' ') || code == KeyCode::Enter {
                    match *i {
                        0 => *mode = WhoMode::AllLater,
                        1 => *mode = WhoMode::AllNow,
                        n if code == KeyCode::Char(' ') || *mode != WhoMode::Picked => {
                            if *mode != WhoMode::Picked {
                                set.clear();
                            }
                            *mode = WhoMode::Picked;
                            let dev = &devices[n - 2];
                            toggle(set, dev);
                        }
                        _ => {}
                    }
                    if code == KeyCode::Enter && !(*mode == WhoMode::Picked && set.is_empty()) {
                        let picked: Vec<String> = set.iter().cloned().collect();
                        return match link.take() {
                            None => self.invite(match mode {
                                WhoMode::AllLater => "all".into(),
                                WhoMode::AllNow => "now".into(),
                                WhoMode::Picked => picked.join(","),
                            }),
                            Some(dev) => self.link(
                                dev,
                                match mode {
                                    WhoMode::AllLater => Vec::new(),
                                    WhoMode::AllNow => devices.clone(),
                                    WhoMode::Picked => picked,
                                },
                            ),
                        };
                    }
                }
            }
            Dialog::Connect(c) => match c {
                Connect::Choose(i) => {
                    if down || up {
                        *i = usize::from(down);
                    } else if code == KeyCode::Enter {
                        return match (*i, self.sync.is_some()) {
                            (0, false) => self.prepare_first(),
                            (0, true) => self.connect_who(),
                            _ => {
                                self.ui.dialog = Some(Dialog::Connect(Connect::Enter { buf: String::new(), err: None }))
                            }
                        };
                    }
                }
                Connect::Code { code: Some(c), .. } if code == KeyCode::Char('c') => {
                    let c = c.clone();
                    self.ui.dialog = Some(d);
                    return self.say(if crate::platform::copy_to_clipboard(&c) {
                        "Copied the code."
                    } else {
                        "Couldn't reach the clipboard. Select the code instead."
                    });
                }
                Connect::Enter { buf, err } => match code {
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) if buf.len() < 2000 && !c.is_whitespace() => buf.push(c),
                    KeyCode::Enter => match crate::sync::connect::Invite::decode(buf) {
                        Err(e) => *err = Some(format!("That code doesn't work: {e}.")),
                        Ok(inv) if inv.d.is_empty() => return self.join(std::mem::take(buf), None),
                        Ok(inv) => {
                            let offer: Vec<String> = inv.d.iter().map(crate::util::sanitize::clean).collect();
                            d = Dialog::Connect(Connect::Pick {
                                code: std::mem::take(buf),
                                from: crate::util::sanitize::clean(&inv.n),
                                set: offer.iter().cloned().collect(),
                                offer,
                                i: 0,
                            });
                        }
                    },
                    _ => {}
                },
                Connect::Pick { code: c, offer, set, i, .. } => {
                    if down || up {
                        *i = if down { (*i + 1).min(offer.len().saturating_sub(1)) } else { i.saturating_sub(1) };
                    } else if code == KeyCode::Char(' ') {
                        toggle(set, &offer[*i]);
                    } else if code == KeyCode::Enter {
                        return self.join(std::mem::take(c), Some(set.iter().cloned().collect()));
                    }
                }
                _ => {}
            },
        }
        self.ui.dialog = Some(d);
    }
}
