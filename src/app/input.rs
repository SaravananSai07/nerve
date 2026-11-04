use std::collections::BTreeSet;

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};

use super::{App, Confirm};
use crate::sync::folders::{Others, When};
use crate::tui::dialog::{Connect, Dialog, FolderStep, ReviewStep, StartStep, WhoMode};
use crate::tui::model::Status;
use crate::tui::settings::Item;
use crate::tui::Page;

/// Add `item` to `set`, or take it out if it is there.
pub(super) fn toggle(set: &mut BTreeSet<String>, item: &str) {
    if !set.remove(item) {
        set.insert(item.to_string());
    }
}

/// Lines one wheel notch scrolls the transcript.
const WHEEL: usize = 3;
/// Lines PgUp and PgDn scroll it.
const PAGE: usize = 12;

impl App {
    pub(super) fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) => self.handle_key(k.code, k.modifiers),
            // The wheel scrolls the transcript under the pointer, and
            // moves through lists everywhere else, as arrows would.
            Event::Mouse(m) => {
                let up = match m.kind {
                    MouseEventKind::ScrollUp => true,
                    MouseEventKind::ScrollDown => false,
                    _ => return,
                };
                let free = self.ui.dialog.is_none()
                    && self.ui.confirm.is_none()
                    && self.ui.compose.is_none()
                    && !self.ui.finding;
                if free
                    && self.ui.page == Page::Sessions
                    && self.ui.open
                    && crate::tui::sessions::over_panel(self.ui.width, m.column)
                {
                    self.scroll_by(if up { WHEEL as isize } else { -(WHEEL as isize) });
                } else {
                    self.handle_key(if up { KeyCode::Up } else { KeyCode::Down }, KeyModifiers::NONE);
                }
            }
            _ => {}
        }
    }

    /// Scroll the open panel up (positive) or down; the panel clamps it.
    fn scroll_by(&mut self, by: isize) {
        self.ui.scroll.set(self.ui.scroll.get().saturating_add_signed(by));
    }

    pub(super) fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        // Ctrl+C always quits, whatever is open. Both cases, so Caps Lock
        // or Shift can't defeat it.
        if matches!(code, KeyCode::Char('c' | 'C')) && modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        if let Some(c) = self.ui.confirm.take() {
            self.confirm_key(c, code);
            return;
        }
        if self.ui.dialog.is_some() {
            self.dialog_key(code);
            return;
        }
        if self.ui.finding {
            self.find_key(code);
            return;
        }
        if self.ui.compose.is_some() {
            self.compose_key(code);
            return;
        }
        match code {
            KeyCode::Tab | KeyCode::BackTab => {
                let i = Page::ALL.iter().position(|p| *p == self.ui.page).unwrap_or(0);
                let n = Page::ALL.len();
                self.ui.page = Page::ALL[if code == KeyCode::Tab { (i + 1) % n } else { (i + n - 1) % n }];
                return;
            }
            KeyCode::Char('?') => {
                self.ui.dialog = Some(Dialog::Keys);
                return;
            }
            KeyCode::Char('q') => {
                self.should_quit = true;
                return;
            }
            _ => {}
        }
        match self.ui.page {
            Page::Sessions => self.sessions_key(code),
            Page::Sync => self.sync_key(code),
            Page::Settings => self.settings_key(code),
        }
    }

    fn sync_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.sync_move(1),
            KeyCode::Char('k') | KeyCode::Up => self.sync_move(-1),
            KeyCode::Char('s') => self.sync_now(),
            KeyCode::Char('a') => self.start_add_folder(),
            KeyCode::Char('c') => self.start_connect(),
            KeyCode::Char('x') => self.ask_stop_sync(),
            KeyCode::Char('o') => self.start_rules(),
            KeyCode::Char('v') => self.start_versions(),
            KeyCode::Esc => self.ui.page = Page::Sessions,
            _ => {}
        }
    }

    fn sessions_key(&mut self, code: KeyCode) {
        // A request under the cursor takes y, n and ↵.
        if let Some(key) = self.cur_row().filter(|r| r.status == Status::Request).map(|r| r.key.clone()) {
            match code {
                KeyCode::Char('y') => return self.decide(&key, true, None, None),
                KeyCode::Char('n') => return self.decide(&key, false, None, None),
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => return self.review(key),
                _ => {}
            }
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right if self.ui.cur.is_some() => self.ui.open = true,
            KeyCode::Char('h') | KeyCode::Left => self.ui.open = false,
            KeyCode::PageUp if self.ui.open => self.scroll_by(PAGE as isize),
            KeyCode::PageDown if self.ui.open => self.scroll_by(-(PAGE as isize)),
            KeyCode::Home if self.ui.open => self.ui.scroll.set(usize::MAX),
            KeyCode::End if self.ui.open => self.ui.scroll.set(0),
            KeyCode::Esc => {
                if self.ui.screen.take().is_none() {
                    if self.ui.open {
                        self.ui.open = false;
                    } else {
                        self.ui.find = None;
                    }
                }
            }
            KeyCode::Char('/') => {
                self.ui.find.get_or_insert_with(String::new);
                self.ui.finding = true;
                self.ui.open = false;
            }
            KeyCode::Char(c @ '1'..='9') => self.answer(c as usize - '1' as usize),
            KeyCode::Char('i') => self.start_compose(),
            KeyCode::Char('n') => self.open_notify(),
            KeyCode::Char('p') => self.private_key(),
            KeyCode::Char('t') => self.jump(),
            KeyCode::Char('x') => self.ask_stop(),
            KeyCode::Char('r') => self.start_rename(),
            KeyCode::Char('P') => self.ask_capture(),
            KeyCode::Char('H') => self.start_handoff(),
            KeyCode::Char('s') => self.start_session_dialog(),
            _ => {}
        }
    }

    fn move_cursor(&mut self, by: isize) {
        let order = self.order();
        if order.is_empty() {
            return;
        }
        let i = self.ui.cur.as_ref().and_then(|k| order.iter().position(|o| o == k)).unwrap_or(0);
        let next = (i as isize + by).clamp(0, order.len() as isize - 1) as usize;
        if self.ui.cur.as_ref() != Some(&order[next]) {
            self.ui.cur = Some(order[next].clone());
            self.ui.screen = None;
        }
    }

    fn settings_key(&mut self, code: KeyCode) {
        let view = self.settings_view();
        let n = view.items().len();
        // Another device under the cursor: l connects it to more, f forgets it.
        if let (Some(Item::Device(i)), KeyCode::Char(c @ ('f' | 'l'))) = (view.item(), code) {
            if let Some(d) = view.devices.get(i).map(|d| d.name.clone()).filter(|d| *d != self.me) {
                return if c == 'f' { self.ask_forget(d) } else { self.start_link(d) };
            }
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.ui.settings_cursor = (self.ui.settings_cursor + 1).min(n.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => self.ui.settings_cursor = self.ui.settings_cursor.saturating_sub(1),
            KeyCode::Esc => self.ui.page = Page::Sessions,
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l') => {
                let forward = matches!(code, KeyCode::Right | KeyCode::Char('l'));
                match view.item() {
                    Some(Item::Theme) => self.cycle_theme(forward),
                    Some(Item::Notify(_)) => self.ui.settings_col = u8::from(forward),
                    _ => {}
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter => match view.item() {
                Some(Item::Notify(i)) => self.toggle_notify_pref(&view.notify[i].device),
                Some(Item::Theme) => self.cycle_theme(true),
                Some(Item::Connect) => self.start_connect(),
                _ => {}
            },
            KeyCode::Char('u') => {
                if let Some(Item::Private(i)) = view.item() {
                    self.share_again(i);
                }
            }
            KeyCode::Char('c') => self.start_connect(),
            _ => {}
        }
    }

    fn find_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.ui.find = None;
                self.ui.finding = false;
            }
            KeyCode::Enter => {
                self.ui.finding = false;
                if self.ui.find.as_deref() == Some("") {
                    self.ui.find = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(q) = self.ui.find.as_mut() {
                    q.pop();
                }
            }
            KeyCode::Char(c) => self.ui.find.get_or_insert_with(String::new).push(c),
            _ => {}
        }
    }

    fn compose_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.ui.compose = None,
            KeyCode::Enter => {
                let text = self.ui.compose.take().unwrap_or_default();
                if !text.trim().is_empty() {
                    self.send_prompt(text);
                }
            }
            KeyCode::Backspace => {
                if let Some(t) = self.ui.compose.as_mut() {
                    t.pop();
                }
            }
            // Claude takes long prompts; this caps a runaway paste.
            KeyCode::Char(c) => {
                if let Some(t) = self.ui.compose.as_mut().filter(|t| t.chars().count() < 4000) {
                    t.push(c);
                }
            }
            _ => {}
        }
    }

    fn confirm_key(&mut self, c: Confirm, code: KeyCode) {
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

    fn dialog_key(&mut self, code: KeyCode) {
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
            Dialog::Start { step, dir, prompt, .. } => {
                let (buf, max) = match step {
                    StartStep::Folder => (dir, 1024),
                    _ => (prompt, crate::sync::handoff::PROMPT_MAX),
                };
                match code {
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) if buf.len() + c.len_utf8() <= max => buf.push(c),
                    KeyCode::Enter if buf.trim().is_empty() => {}
                    KeyCode::Enter if *step == StartStep::Folder => *step = StartStep::Prompt,
                    KeyCode::Enter => return self.start_session(d),
                    _ => {}
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
                                set: offer.iter().cloned().collect(),
                                offer,
                                i: 0,
                            });
                        }
                    },
                    _ => {}
                },
                Connect::Pick { code: c, offer, set, i } => {
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
