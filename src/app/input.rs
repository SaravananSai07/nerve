use crossterm::event::{KeyCode, KeyModifiers};

use super::{App, Overlay};

impl App {
    pub(super) fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        // Ctrl+C always quits, regardless of overlay. This is the one
        // hard exit; `q` from inside an overlay closes the overlay.
        // Match both `c` and `C` so the comment ("always quits") holds
        // when the user has Caps Lock on or Shift down.
        if matches!(code, KeyCode::Char('c') | KeyCode::Char('C'))
            && modifiers.contains(KeyModifiers::CONTROL)
        {
            self.should_quit = true;
            return;
        }
        match &self.overlay {
            Overlay::Help => {
                match code {
                    KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc => {
                        self.overlay = Overlay::None;
                    }
                    _ => {}
                }
                return;
            }
            Overlay::Search => {
                self.handle_search_key(code);
                return;
            }
            Overlay::Rename(_) => {
                self.handle_rename_key(code);
                return;
            }
            Overlay::Preview { .. } => {
                self.handle_preview_key(code);
                return;
            }
            Overlay::ConfirmKill { .. } => {
                match code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        if let Overlay::ConfirmKill { name, id, kind, root } =
                            std::mem::replace(&mut self.overlay, Overlay::None)
                        {
                            self.execute_kill(&name, id.as_str(), kind, root);
                        }
                    }
                    // Enter / n / Esc / q all cancel — kill is
                    // destructive, so the "punch through with Enter"
                    // reflex resolves to the safe choice.
                    KeyCode::Char('n')
                    | KeyCode::Char('N')
                    | KeyCode::Char('q')
                    | KeyCode::Enter
                    | KeyCode::Esc => self.overlay = Overlay::None,
                    _ => {}
                }
                return;
            }
            Overlay::ConfirmPreview => {
                self.handle_confirm_preview_key(code);
                return;
            }
            Overlay::Details { .. } => {
                match code {
                    KeyCode::Enter => {
                        self.overlay = Overlay::None;
                        self.go_to_selected_tab();
                    }
                    KeyCode::Char('c') => self.copy_selected_details_target(),
                    KeyCode::Char('i') | KeyCode::Char('q') | KeyCode::Esc => {
                        self.overlay = Overlay::None;
                    }
                    _ => {}
                }
                return;
            }
            Overlay::None => {}
        }

        self.status_message = None;

        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => {
                // In 3-col mode the last row may be partial. A user
                // pressing `j` from the rightmost cell of the last
                // full row would otherwise pin the cursor; snap to
                // the last selectable session instead.
                let count = self.num_filtered();
                if count == 0 {
                    // No-op when there's nothing to select.
                } else {
                    let next = self.selected + self.cols;
                    let last_row = self.selected / self.cols;
                    let next_row = next / self.cols;
                    if next < count {
                        self.selected = next;
                    } else if next_row > last_row {
                        self.selected = count - 1;
                    }
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.selected >= self.cols {
                    self.selected -= self.cols;
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let col = self.selected % self.cols;
                if col > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let count = self.num_filtered();
                let col = self.selected % self.cols;
                if col + 1 < self.cols && self.selected + 1 < count {
                    self.selected += 1;
                }
            }
            // Enter is the only "switch to that session's tab" key.
            // Plain `g` previously aliased here, but vim users hitting
            // `g` (expecting `gg` = top, or to-of-list navigation)
            // would teleport into a session with no return path.
            KeyCode::Enter => {
                self.go_to_selected_tab();
            }
            KeyCode::Char('s') => {
                self.registry.cycle_sort();
                self.apply_filter();
            }
            KeyCode::Char('t') => {
                self.cycle_theme();
            }
            KeyCode::Char('n') => {
                self.start_rename();
            }
            KeyCode::Char('p') => {
                self.open_log_preview();
            }
            KeyCode::Char('P') => {
                self.open_preview();
            }
            KeyCode::Char('x') => {
                self.start_kill();
            }
            KeyCode::Char('i') => {
                if let Some(session) = self.nth_filtered(self.selected) {
                    self.overlay = Overlay::Details { id: session.id.clone() };
                }
            }
            KeyCode::Char('u') => {
                // Dismiss the update banner for *this version only*.
                // The next upstream release flips `pending_update`'s
                // value and the banner returns. The empty-banner
                // feedback distinguishes "nothing pending" from
                // "already dismissed earlier this version" — the
                // second case used to say "nothing to dismiss",
                // which felt like the dismissal hadn't taken.
                if let Some(version) = self.update_banner.take() {
                    self.prefs.dismissed_update_version = Some(version);
                    self.prefs.save(&self.paths);
                } else if let Some(v) = self.prefs.dismissed_update_version.as_deref() {
                    self.set_status_info(format!("update banner dismissed for v{v}"));
                } else {
                    self.set_status_info("no update banner to dismiss");
                }
            }
            KeyCode::Char('m') => {
                self.prefs.notifications_muted = !self.prefs.notifications_muted;
                self.prefs.save(&self.paths);
                let msg = if self.prefs.notifications_muted {
                    "notifications muted"
                } else {
                    "notifications unmuted"
                };
                self.set_status_info(msg);
            }
            KeyCode::Char('/') => {
                self.search_query.get_or_insert_with(String::new);
                self.overlay = Overlay::Search;
            }
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
            }
            KeyCode::Esc => {
                if self.search_query.is_some() {
                    self.search_query = None;
                    self.apply_filter();
                    self.status_message = None;
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as usize) - ('1' as usize);
                let count = self.num_filtered();
                if idx < count {
                    self.selected = idx;
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_preview_key(&mut self, code: KeyCode) {
        let Overlay::Preview { scroll, source } = &mut self.overlay else {
            return;
        };
        // Clamp the cursor at the handler so holding `j` past the
        // bottom can't grow `scroll` to absurd values that take an
        // equal number of `k` presses to recover.
        let max = source.line_count().saturating_sub(1);
        // Half-screen-ish for PageUp/PageDown — matches the reading
        // distance most TUI viewers use.
        const PAGE: usize = 10;
        match code {
            KeyCode::Char('p')
            | KeyCode::Char('P')
            | KeyCode::Char('q')
            | KeyCode::Esc => {
                self.overlay = Overlay::None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                *scroll = scroll.saturating_add(1).min(max);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                *scroll = scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                *scroll = scroll.saturating_add(PAGE).min(max);
            }
            KeyCode::PageUp => {
                *scroll = scroll.saturating_sub(PAGE);
            }
            KeyCode::Char('g') => *scroll = 0,
            KeyCode::Char('G') => *scroll = max,
            _ => {}
        }
    }

    pub(super) fn handle_confirm_preview_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') => {
                self.overlay = Overlay::None;
                self.execute_preview_capture();
            }
            KeyCode::Char('d') => {
                self.prefs.preview_flicker_accepted = true;
                self.prefs.save(&self.paths);
                self.overlay = Overlay::None;
                self.execute_preview_capture();
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.overlay = Overlay::None;
                self.open_log_preview();
            }
            _ => {}
        }
    }

    pub(super) fn handle_rename_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => {
                if let Overlay::Rename(buf) = std::mem::replace(&mut self.overlay, Overlay::None) {
                    let trimmed = buf.trim().to_string();
                    if !trimmed.is_empty() {
                        self.commit_rename(trimmed);
                    }
                }
            }
            KeyCode::Esc => {
                self.overlay = Overlay::None;
            }
            KeyCode::Backspace => {
                if let Overlay::Rename(ref mut buf) = self.overlay {
                    buf.pop();
                }
            }
            KeyCode::Char(c) => {
                // Cap by char count, not byte length — a CJK glyph is
                // 3 UTF-8 bytes, so the previous `buf.len() < 48` cut
                // CJK names off at roughly 16 characters. The cap
                // matches the rename overlay's drawn width.
                const MAX_CHARS: usize = 48;
                if let Overlay::Rename(ref mut buf) = self.overlay {
                    if buf.chars().count() < MAX_CHARS {
                        buf.push(c);
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_search_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => {
                self.overlay = Overlay::None;
                match self.search_query.as_deref() {
                    Some(q) if !q.is_empty() => {
                        let count = self.num_filtered();
                        self.set_status_info(format!("search: /{q}  ({count})"));
                    }
                    _ => self.search_query = None,
                }
            }
            KeyCode::Esc => {
                self.overlay = Overlay::None;
                self.search_query = None;
                self.apply_filter();
            }
            KeyCode::Backspace => {
                if let Some(buf) = self.search_query.as_mut() {
                    buf.pop();
                }
                self.apply_filter();
            }
            KeyCode::Char(c) => {
                self.search_query.get_or_insert_with(String::new).push(c);
                self.apply_filter();
            }
            _ => {}
        }
    }
}
