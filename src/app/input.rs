// SPDX-License-Identifier: GPL-3.0-or-later
//! Keys and mouse.

use super::*;

impl App {
    // ------------------------------------------------------------------- input

    /// Bring the lists and the map layout in line with the tree and the view.
    /// Drawing does this as well, but several events can be handled between
    /// two frames, and each has to act on what is current rather than on
    /// what was drawn last: after a deletion, old ids point at other nodes.
    fn sync(&mut self) {
        self.prepare();
        self.ensure_layout(self.hit.map);
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Effect {
        if key.kind == KeyEventKind::Release {
            return Effect::None;
        }
        self.sync();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Effect::Quit;
        }
        if self.modal.is_some() {
            return self.on_modal_key(key);
        }
        if self.tree.is_none() {
            // Scanning screen.
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                if let Some(job) = &self.scan {
                    job.progress.cancel.store(true, Relaxed);
                }
                return Effect::Quit;
            }
            return Effect::None;
        }

        let page = (self.hit.list.h.max(2) - 1) as isize;
        match key.code {
            KeyCode::Char('u' | 'b') if ctrl => self.move_rows(-page),
            KeyCode::Char('d' | 'f') if ctrl => self.move_rows(page),
            KeyCode::Char('q') => return Effect::Quit,
            KeyCode::Char('?') | KeyCode::F(1) => self.modal = Some(Modal::Help),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Pane::Map => Pane::Side,
                    Pane::Side => Pane::Map,
                };
            }
            KeyCode::Char('1') => self.set_tab(Tab::Files),
            KeyCode::Char('2') => self.set_tab(Tab::Largest),
            KeyCode::Char('3') => self.set_tab(Tab::Types),
            KeyCode::Char(' ') => {
                if let Some(id) = self.active() {
                    self.toggle_mark(id);
                    if self.focus == Pane::Side {
                        self.move_rows(1);
                    }
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => self.open_delete(),
            KeyCode::Char('u') => {
                if !self.marks.is_empty() {
                    self.marks.clear();
                    self.recount_marks();
                    self.toast("marks cleared", ToastKind::Info);
                }
            }
            KeyCode::Char('a') => {
                self.metric = match self.metric {
                    Metric::Disk => Metric::Apparent,
                    Metric::Apparent => Metric::Disk,
                };
                self.changed();
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.tab = Tab::Files;
                self.rebuild_rows();
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.depth = (self.depth + 1).min(MAX_DEPTH);
            }
            KeyCode::Char('-') | KeyCode::Char('_') => self.depth = self.depth.saturating_sub(1),
            KeyCode::Char('r') | KeyCode::F(5) => self.rescan(),
            KeyCode::Char('y') => return self.copy_path(),
            KeyCode::Char('o') => self.open_external(),
            KeyCode::Char('!') => {
                if let Some(dir) = self.shell_dir() {
                    return Effect::Shell(dir);
                }
            }
            KeyCode::Enter => self.activate(),
            KeyCode::Backspace | KeyCode::Esc => self.leave(),
            KeyCode::Home | KeyCode::Char('g') => self.move_rows(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_rows(isize::MAX / 2),
            KeyCode::PageUp => self.move_rows(-page),
            KeyCode::PageDown => self.move_rows(page),
            code => self.on_move_key(code),
        }
        Effect::None
    }

    fn on_move_key(&mut self, code: KeyCode) {
        // In the map, the arrow keys go to the neighbouring tile. So do the
        // shifted letters, for hands that stay on the home row.
        let across = match code {
            KeyCode::Left | KeyCode::Char('H') => Some((-1, 0)),
            KeyCode::Right | KeyCode::Char('L') => Some((1, 0)),
            KeyCode::Up | KeyCode::Char('K') => Some((0, -1)),
            KeyCode::Down | KeyCode::Char('J') => Some((0, 1)),
            _ => None,
        };
        if self.focus == Pane::Map {
            // Whatever moves the map's selection is about the files here.
            if matches!(code, KeyCode::Char('h' | 'j' | 'k' | 'l')) || across.is_some() {
                self.tab = Tab::Files;
            }
            if let Some((dx, dy)) = across {
                self.move_map(dx, dy);
                return;
            }
        }
        // h j k l walk the tree the same way in both panes, as in ncdu or
        // ranger: parent, next, previous, open.
        match code {
            KeyCode::Down | KeyCode::Char('j' | 'J') => self.move_rows(1),
            KeyCode::Up | KeyCode::Char('k' | 'K') => self.move_rows(-1),
            KeyCode::Right | KeyCode::Char('l' | 'L') => self.activate(),
            KeyCode::Left | KeyCode::Char('h' | 'H') => self.leave(),
            _ => {}
        }
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.focus = Pane::Side;
    }

    /// Enter: open the directory under the cursor, or jump to a listed file.
    fn activate(&mut self) {
        if self.focus == Pane::Side && self.tab == Tab::Largest {
            if let Some(&id) = self.largest.get(self.largest_sel) {
                self.reveal(id);
            }
        } else if (self.tab != Tab::Types || self.focus == Pane::Map) && self.cursor != NONE {
            self.enter(self.cursor);
        }
    }

    pub(super) fn rescan(&mut self) {
        let Some(tree) = &self.tree else {
            return;
        };
        if self.scan.is_some() {
            return;
        }
        let path = tree.path(self.view);
        self.start_scan(Some(path));
    }

    fn on_modal_key(&mut self, key: KeyEvent) -> Effect {
        let close = matches!(key.code, KeyCode::Esc | KeyCode::Char('q'));
        match &mut self.modal {
            Some(Modal::Help) => {
                if close || matches!(key.code, KeyCode::Char('?') | KeyCode::Enter) {
                    self.modal = None;
                }
            }
            Some(Modal::Confirm(c)) => {
                // While a word has to be typed, letters are input, so the
                // letter shortcuts are off. Esc always closes.
                let typing = c.word.is_some();
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => self.modal = None,
                    KeyCode::Right | KeyCode::Tab => c.step(true),
                    KeyCode::Left | KeyCode::BackTab => c.step(false),
                    KeyCode::Up => c.scroll = c.scroll.saturating_sub(1),
                    KeyCode::Down => {
                        c.scroll = (c.scroll + 1).min(c.items.len().saturating_sub(1));
                    }
                    KeyCode::Enter => {
                        if c.choice == Choice::Cancel {
                            self.modal = None;
                        } else if !c.locked() {
                            let Some(Modal::Confirm(c)) = self.modal.take() else {
                                unreachable!()
                            };
                            self.start_delete(c);
                        }
                    }
                    KeyCode::Backspace if typing => {
                        c.typed.pop();
                        if c.locked() {
                            c.choice = Choice::Cancel;
                        }
                    }
                    KeyCode::Char(ch) if typing && !ctrl => {
                        if c.typed.chars().count() < 32 {
                            c.typed.push(ch);
                        }
                        if c.locked() {
                            c.choice = Choice::Cancel;
                        }
                    }
                    KeyCode::Char('q' | 'n') => self.modal = None,
                    KeyCode::Char('l') => c.step(true),
                    KeyCode::Char('h') => c.step(false),
                    KeyCode::Char('k') => c.scroll = c.scroll.saturating_sub(1),
                    KeyCode::Char('j') => {
                        c.scroll = (c.scroll + 1).min(c.items.len().saturating_sub(1));
                    }
                    _ => {}
                }
            }
            Some(Modal::Deleting(d)) => {
                if close {
                    d.job.counters.cancel.store(true, Relaxed);
                }
            }
            Some(Modal::Report(r)) => match key.code {
                _ if close => self.modal = None,
                KeyCode::Enter => self.modal = None,
                KeyCode::Up | KeyCode::Char('k') => r.scroll = r.scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    r.scroll = (r.scroll + 1).min(r.failures.len().saturating_sub(1));
                }
                _ => {}
            },
            None => {}
        }
        Effect::None
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) -> Effect {
        if self.modal.is_some() || self.tree.is_none() {
            return Effect::None;
        }
        self.sync();
        let (x, y) = (ev.column, ev.row);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let now = Instant::now();
                let double = self
                    .last_click
                    .is_some_and(|(t, cx, cy)| now - t < DOUBLE_CLICK && cx == x && cy == y);
                // A double click is spent: a third press starts over.
                self.last_click = (!double).then_some((now, x, y));

                if let Some(&(_, tab)) = self.hit.tabs.iter().find(|(r, _)| r.contains(x, y)) {
                    self.set_tab(tab);
                } else if let Some(&(_, dir)) =
                    self.hit.crumbs.iter().find(|(r, _)| r.contains(x, y))
                {
                    self.goto(dir);
                } else if self.hit.map.contains(x, y) {
                    self.focus = Pane::Map;
                    self.tab = Tab::Files;
                    if let Some(t) = self.tile_at(x, y) {
                        self.cursor = if t.kind == TileKind::Rest {
                            t.node
                        } else {
                            t.top
                        };
                        self.anchor = Some((x, y));
                        if double {
                            self.enter(self.cursor);
                        }
                    }
                } else if self.hit.list.contains(x, y) {
                    self.focus = Pane::Side;
                    let row = self.scroll[self.tab as usize] + (y - self.hit.list.y) as usize;
                    match self.tab {
                        Tab::Files => {
                            if let Some(&id) = self.rows.get(row) {
                                self.cursor = id;
                                if double {
                                    self.enter(id);
                                }
                            }
                        }
                        Tab::Largest => {
                            if row < self.largest.len() {
                                self.largest_sel = row;
                                if double {
                                    self.activate();
                                }
                            }
                        }
                        Tab::Types => {
                            if row < self.types.len() {
                                self.types_sel = row;
                            }
                        }
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => self.leave(),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let down = ev.kind == MouseEventKind::ScrollDown;
                if self.hit.list.contains(x, y) {
                    self.move_rows(if down { 3 } else { -3 });
                } else if self.hit.map.contains(x, y) {
                    // Wheel on the map zooms: in on the tile under the
                    // pointer, or back out.
                    if down {
                        self.leave();
                    } else if let Some(top) = self
                        .tile_at(x, y)
                        .filter(|t| t.kind != TileKind::Rest)
                        .map(|t| t.top)
                    {
                        self.enter(top);
                    }
                }
            }
            _ => {}
        }
        Effect::None
    }
}
