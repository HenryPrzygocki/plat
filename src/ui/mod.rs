// SPDX-License-Identifier: GPL-3.0-or-later
//! Drawing. Everything is painted straight into the frame buffer.

mod chrome;
mod map;
mod side;

use std::borrow::Cow;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Pane};
use crate::theme::{Lab, Theme};
use crate::treemap::Rect;

/// Buffer plus theme, with the handful of primitives the views share.
pub struct Painter<'a> {
    pub buf: &'a mut Buffer,
    pub th: &'a Theme,
}

impl Painter<'_> {
    pub fn style(&self, fg: Lab, bg: Lab) -> Style {
        Style::new().fg(self.th.c(fg)).bg(self.th.c(bg))
    }

    /// Fill a rectangle with a background colour, clearing its contents.
    pub fn fill(&mut self, r: Rect, bg: Lab) {
        let bg = self.th.c(bg);
        for y in r.y..r.bottom() {
            for x in r.x..r.right() {
                if let Some(cell) = self.buf.cell_mut((x, y)) {
                    cell.reset();
                    cell.set_bg(bg);
                }
            }
        }
    }

    /// Set one cell.
    pub fn put(&mut self, x: u16, y: u16, ch: char, fg: Lab, bg: Lab) {
        let style = self.style(fg, bg);
        if let Some(cell) = self.buf.cell_mut((x, y)) {
            cell.reset();
            cell.set_char(ch).set_style(style);
        }
    }

    /// Draw text clipped to `max_w` cells, with an ellipsis if it does not
    /// fit. Returns the width drawn.
    pub fn text(&mut self, x: u16, y: u16, max_w: u16, s: &str, style: Style) -> u16 {
        // Never draw outside the buffer, whatever the caller computed.
        let area = *self.buf.area();
        if y < area.y || y >= area.bottom() || x < area.x || x >= area.right() {
            return 0;
        }
        let max_w = max_w.min(area.right() - x);
        if max_w == 0 {
            return 0;
        }
        let s = truncate(s, max_w as usize);
        self.buf.set_stringn(x, y, &s, max_w as usize, style);
        width(&s) as u16
    }

    /// Draw text so that it ends at column `right` (exclusive).
    pub fn text_right(&mut self, right: u16, y: u16, max_w: u16, s: &str, style: Style) -> u16 {
        let s = truncate(s, max_w as usize);
        let w = width(&s) as u16;
        self.text(right.saturating_sub(w), y, w, &s, style)
    }
}

pub fn bold(style: Style) -> Style {
    style.add_modifier(Modifier::BOLD)
}

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Shorten `s` to at most `max` cells, ending in an ellipsis if cut.
pub fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if width(s) <= max {
        return Cow::Borrowed(s);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    Cow::Owned(out)
}

/// Shorten `s` from the left, keeping its end visible.
pub fn truncate_left(s: &str, max: usize) -> Cow<'_, str> {
    if width(s) <= max {
        return Cow::Borrowed(s);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        tail.push(c);
        used += w;
    }
    let mut out = String::from("…");
    out.extend(tail.iter().rev());
    Cow::Owned(out)
}

/// Where each part of the main screen goes.
struct Regions {
    header: Rect,
    map: Option<Rect>,
    side: Option<Rect>,
    status: Rect,
}

fn regions(area: Rect, focus: Pane) -> Regions {
    let header = Rect::new(area.x, area.y, area.w, 1.min(area.h));
    let status = Rect::new(
        area.x,
        area.bottom().saturating_sub(1),
        area.w,
        1.min(area.h),
    );
    let body = Rect::new(area.x, area.y + 1, area.w, area.h.saturating_sub(2));
    let (map, side) = if body.w >= 96 {
        // Side by side.
        let side_w = (body.w * 2 / 5).clamp(40, 76);
        (
            Some(Rect::new(body.x, body.y, body.w - side_w, body.h)),
            Some(Rect::new(body.right() - side_w, body.y, side_w, body.h)),
        )
    } else if body.h >= 26 {
        // Stacked.
        let side_h = (body.h * 2 / 5).max(9);
        (
            Some(Rect::new(body.x, body.y, body.w, body.h - side_h)),
            Some(Rect::new(body.x, body.bottom() - side_h, body.w, side_h)),
        )
    } else if focus == Pane::Map {
        (Some(body), None)
    } else {
        (None, Some(body))
    };
    Regions {
        header,
        map,
        side,
        status,
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let fa = frame.area();
    let area = Rect::new(fa.x, fa.y, fa.width, fa.height);
    app.tick = app.tick.wrapping_add(1);
    app.hit = Default::default();
    app.prepare();

    let regions = regions(area, app.focus);
    // The layout cache needs the map area before anything borrows the app.
    let map_area = regions.map.map(map::content_area);
    if let Some(a) = map_area {
        app.ensure_layout(a);
        app.hit.map = a;
    }

    let mut hit = std::mem::take(&mut app.hit);
    let mut scroll = app.scroll;
    {
        let app = &*app;
        let mut p = Painter {
            buf: frame.buffer_mut(),
            th: &app.theme,
        };
        p.fill(area, app.theme.bg);
        if area.w < 20 || area.h < 6 {
            let style = p.style(app.theme.dim, app.theme.bg);
            p.text(area.x, area.y, area.w, "window too small", style);
        } else if app.tree.is_none() {
            chrome::scanning(&mut p, app, area);
        } else {
            chrome::header(&mut p, app, regions.header, &mut hit);
            if let Some(r) = regions.map {
                map::draw(&mut p, app, r);
            }
            if let Some(r) = regions.side {
                side::draw(&mut p, app, r, &mut hit, &mut scroll);
            }
            chrome::status(&mut p, app, regions.status);
            if let Some(modal) = &app.modal {
                chrome::modal(&mut p, app, modal, area);
            }
        }
    }
    app.hit = hit;
    app.scroll = scroll;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Choice, Confirm, DelItem, Deleting, Modal, Report, Tab, Warning};
    use crate::guard::Level;
    use crate::ops::{DeleteJob, Method};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn truncation_respects_cell_width() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a longer name", 6), "a lon…");
        assert_eq!(truncate("x", 0), "");
        assert_eq!(truncate_left("dir/sub/file.txt", 9), "…file.txt");
        // Wide characters take two cells each.
        assert_eq!(width(&truncate("日本語のファイル", 7)), 7);
        assert!(width(&truncate("日本語のファイル", 6)) <= 6);
    }

    fn fixture(tag: &str) -> PathBuf {
        let root = crate::testutil::scratch(tag);
        let mut size = 3usize;
        for a in 0..5 {
            for b in 0..4 {
                let dir = root.join(format!("dir-{a}/nested-{b}/deeper"));
                fs::create_dir_all(&dir).unwrap();
                for (c, ext) in ["mkv", "h5", "rs", "zip", "log", ""].iter().enumerate() {
                    size = (size * 41 + 17) % 60_000;
                    fs::write(dir.join(format!("file-{c}.{ext}")), vec![0u8; size + 1]).unwrap();
                }
            }
        }
        fs::write(root.join("日本語のファイル名.txt"), vec![0u8; 40_000]).unwrap();
        fs::write(root.join("ctl\x1b[31m\tname"), vec![0u8; 20_000]).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        root.canonicalize().unwrap()
    }

    /// Draw every screen at a sweep of window sizes. Tests are built with
    /// overflow checks, so any size arithmetic that underflows panics here.
    #[test]
    fn every_screen_draws_at_every_size() {
        let root = fixture("sweep");
        let mut app = crate::app::App::new(
            root.clone(),
            crate::scan::Options::default(),
            crate::theme::Theme::new(true),
        );
        app.scan_blocking().unwrap();
        let press = |app: &mut App, code| {
            app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        };
        let item = |name: &str| DelItem {
            id: 1,
            path: root.join(name),
            size: 123_456,
            entries: 42,
            is_dir: true,
        };

        let sizes: Vec<(u16, u16)> = (1..=230u16)
            .step_by(7)
            .flat_map(|w| (1..=70u16).step_by(5).map(move |h| (w, h)))
            .chain([
                (20, 6),
                (21, 7),
                (95, 25),
                (96, 26),
                (97, 27),
                (40, 28),
                (300, 100),
            ])
            .collect();
        let mut frames = 0;
        for &(w, h) in &sizes {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut draw = |app: &mut App| {
                terminal.draw(|f| draw(f, app)).unwrap();
            };
            for tab in Tab::ALL {
                for focus in [Pane::Map, Pane::Side] {
                    app.tab = tab;
                    app.focus = focus;
                    draw(&mut app);
                    press(&mut app, KeyCode::Char(' '));
                    press(&mut app, KeyCode::Down);
                    draw(&mut app);
                    frames += 2;
                }
            }
            app.modal = Some(Modal::Help);
            draw(&mut app);
            for count in [1, 25] {
                // Plain, with cautions, and locked behind a typed word.
                for (warnings, word, typed) in [
                    (0, None, ""),
                    (count, None, ""),
                    (count, Some("walnut"), "wal"),
                    (2, Some("walnut"), "walnut"),
                ] {
                    let confirm = Confirm {
                        items: (0..count).map(|i| item(&format!("victim-{i}"))).collect(),
                        size: 1 << 30,
                        entries: 9000,
                        choice: Choice::Trash,
                        scroll: 3,
                        trash: Some(crate::ops::TrashTool::Gio),
                        warnings: (0..warnings)
                            .map(|i| Warning {
                                subject: format!("/a/rather/long/path/to/victim-{i}"),
                                level: if i == 0 && word.is_some() {
                                    Level::Critical
                                } else {
                                    Level::Caution
                                },
                                reason:
                                    "A reason that explains at some length what is at stake here."
                                        .into(),
                            })
                            .collect(),
                        word,
                        typed: typed.into(),
                    };
                    app.modal = Some(Modal::Confirm(confirm));
                    draw(&mut app);
                    frames += 1;
                }
                app.modal = Some(Modal::Report(Report {
                    title: "Not everything was deleted",
                    summary: "0 of 3 items deleted".into(),
                    failures: (0..count)
                        .map(|i| (format!("/some/long/path/{i}"), "permission denied".into()))
                        .collect(),
                    scroll: 2,
                }));
                draw(&mut app);
                frames += 2;
            }
            app.modal = Some(Modal::Deleting(Deleting {
                items: vec![item("going")],
                entries: 500,
                method: Method::Delete,
                job: DeleteJob::start(Vec::new(), Method::Delete).unwrap(),
            }));
            draw(&mut app);
            app.modal = None;
            // Walk into a directory and back so deeper views get drawn too.
            app.focus = Pane::Side;
            app.tab = Tab::Files;
            press(&mut app, KeyCode::Home);
            press(&mut app, KeyCode::Enter);
            draw(&mut app);
            press(&mut app, KeyCode::Backspace);
            frames += 3;
        }
        assert!(frames > 3000, "only {frames} frames drawn");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn control_characters_in_names_never_reach_the_screen() {
        let root = fixture("names");
        let mut app = crate::app::App::new(
            root.clone(),
            crate::scan::Options::default(),
            crate::theme::Theme::new(true),
        );
        app.scan_blocking().unwrap();
        app.depth = 0;
        let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut seen_name = false;
        for cell in buffer.content() {
            assert!(
                !cell.symbol().chars().any(char::is_control),
                "control character on screen: {:?}",
                cell.symbol()
            );
            seen_name |= cell.symbol() == "\u{fffd}";
        }
        assert!(seen_name, "the awkward file name was not drawn at all");
        fs::remove_dir_all(&root).unwrap();
    }
}
