// SPDX-License-Identifier: GPL-3.0-or-later
//! Everything around the two panes: header, status bar, the scanning screen
//! and overlays.

use std::sync::atomic::Ordering::Relaxed;

use ratatui::style::Color;

use std::path::Path;

use super::{Painter, bold, truncate_left, width};
use crate::app::{App, Choice, Confirm, Deleting, Hit, Modal, Report, ToastKind};
use crate::ext::Cat;
use crate::fmt;
use crate::guard::Level;
use crate::ops::Method;
use crate::theme::Lab;
use crate::tree::{Metric, NodeId, Tree, flag};
use crate::treemap::Rect;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// A filled label, such as the wordmark or a warning badge.
fn pill(p: &mut Painter, x: u16, y: u16, text: &str, fg: Lab, bg: Lab) -> u16 {
    let label = format!(" {text} ");
    let style = bold(p.style(fg, bg));
    p.text(x, y, width(&label) as u16, &label, style)
}

pub fn header(p: &mut Painter, app: &App, area: Rect, hit: &mut Hit) {
    let Some(tree) = &app.tree else {
        return;
    };
    let th = p.th;
    p.fill(area, th.panel);
    let y = area.y;
    let mut x = area.x + 1;
    x += pill(p, x, y, "plat", th.ink, th.accent) + 2;

    // Right side first, so the breadcrumb knows how much room is left.
    let mut right = area.right().saturating_sub(1);
    if let Some((total, avail)) = app.capacity {
        let used = total.saturating_sub(avail);
        let frac = used as f64 / total as f64;
        let text = format!("{} free of {}", fmt::size(avail), fmt::size(total));
        let w = width(&text) as u16;
        if right > x + w + 40 {
            let style = p.style(th.dim, th.panel);
            p.text_right(right, y, w, &text, style);
            right -= w + 2;
            let bar_w = 12u16;
            let filled = (frac * bar_w as f64).round() as u16;
            let colour = if frac > 0.9 { th.danger } else { th.dim };
            for i in 0..bar_w {
                let on = i < filled;
                p.put(
                    right - bar_w + i,
                    y,
                    '━',
                    if on { colour } else { th.line },
                    th.panel,
                );
            }
            right -= bar_w + 2;
        }
    }
    if app.scan_errors > 0 && !app.is_root {
        let text = format!("{} unreadable, try sudo", fmt::count(app.scan_errors));
        let w = width(&text) as u16;
        if right > x + w + 30 {
            let style = p.style(th.warn, th.panel);
            p.text_right(right, y, w, &text, style);
            right -= w + 2;
        }
    }
    if app.read_only {
        let w = width(" READ-ONLY ") as u16;
        if right > x + w + 20 {
            pill(p, right - w, y, "READ-ONLY", th.ink, th.warn);
            right -= w + 2;
        }
    }
    if app.is_root {
        let w = width(" ROOT ") as u16;
        if right > x + w + 20 {
            pill(p, right - w, y, "ROOT", th.ink, th.danger);
            right -= w + 2;
        }
    }

    // Breadcrumb: the scan root, then every directory down to the view.
    let mut chain: Vec<NodeId> = Vec::new();
    let mut cur = app.view;
    while cur != Tree::ROOT {
        chain.push(cur);
        cur = tree.node(cur).parent;
    }
    chain.push(Tree::ROOT);
    chain.reverse();
    let mut parts: Vec<(String, Option<NodeId>)> = chain
        .iter()
        .map(|&id| {
            let label = if id == Tree::ROOT {
                tree.root_path().display().to_string()
            } else {
                fmt::display_name(tree.name(id))
            };
            (label, Some(id))
        })
        .collect();
    let room = right.saturating_sub(x) as usize;
    let total_w = |parts: &[(String, Option<NodeId>)]| {
        parts.iter().map(|(s, _)| width(s)).sum::<usize>() + 3 * parts.len().saturating_sub(1)
    };
    // Too long: fold the middle of the path away.
    while total_w(&parts) > room && parts.len() > 2 {
        if parts[1].1.is_none() {
            parts.remove(2.min(parts.len() - 1));
            if parts.len() == 2 {
                break;
            }
        } else {
            parts[1] = ("…".to_string(), None);
        }
    }
    let last = parts.len() - 1;
    for (i, (label, node)) in parts.iter().enumerate() {
        if x >= right {
            break;
        }
        let style = if i == last {
            bold(p.style(th.text, th.panel))
        } else {
            p.style(th.dim, th.panel)
        };
        let w = p.text(x, y, right - x, label, style);
        if let Some(node) = node {
            hit.crumbs.push((Rect::new(x, y, w, 1), *node));
        }
        x += w;
        if i != last && x + 3 < right {
            let sep = p.style(th.faint, th.panel);
            p.text(x, y, 3, " › ", sep);
            x += 3;
        }
    }
}

pub fn status(p: &mut Painter, app: &App, area: Rect) {
    let Some(tree) = &app.tree else {
        return;
    };
    let th = p.th;
    p.fill(area, th.panel);
    let y = area.y;
    let mut x = area.x + 1;

    // Right: key hints, or what is running.
    let mut right = area.right().saturating_sub(1);
    if let Some(job) = &app.scan {
        let spin = SPINNER[(app.tick as usize / 2) % SPINNER.len()];
        let text = format!(
            "{spin} rescanning  {} files",
            fmt::count(job.progress.files.load(Relaxed))
        );
        let w = width(&text) as u16;
        let style = p.style(th.accent, th.panel);
        p.text_right(right, y, w, &text, style);
        right = right.saturating_sub(w + 2);
    } else {
        let hints: &[(&str, &str)] = if app.marks.is_empty() {
            &[
                ("space", "mark"),
                ("d", "delete"),
                ("?", "help"),
                ("q", "quit"),
            ]
        } else {
            &[
                ("d", "delete marked"),
                ("u", "clear"),
                ("?", "help"),
                ("q", "quit"),
            ]
        };
        let total: u16 = hints
            .iter()
            .map(|(k, v)| (width(k) + width(v) + 4) as u16)
            .sum();
        if area.w > total + 30 {
            let mut hx = right - total + 2;
            for (k, v) in hints {
                let ks = bold(p.style(th.dim.lighten(0.08), th.panel));
                hx += p.text(hx, y, 8, k, ks) + 1;
                let vs = p.style(th.faint, th.panel);
                hx += p.text(hx, y, 20, v, vs) + 3;
            }
            right -= total;
        }
    }

    if !app.marks.is_empty() {
        let text = format!(
            "{} marked · {}",
            fmt::count(app.marks.len() as u64),
            fmt::size(app.marked_size)
        );
        x += pill(p, x, y, &text, th.ink, th.danger) + 2;
    }
    if x >= right {
        return;
    }
    let room = right - x;

    if let Some(toast) = &app.toast {
        let colour = match toast.kind {
            ToastKind::Info => th.dim.lighten(0.1),
            ToastKind::Ok => th.ok,
            ToastKind::Error => th.danger.lighten(0.06),
        };
        let style = p.style(colour, th.panel);
        p.text(x, y, room, &toast.text, style);
        return;
    }

    // Details of whatever actions would apply to.
    let Some(id) = app.active() else {
        if app.scan_errors > 0 && !app.is_root {
            let text = format!(
                "{} {} could not be read; run with sudo to include {}",
                fmt::count(app.scan_errors),
                if app.scan_errors == 1 {
                    "directory"
                } else {
                    "directories"
                },
                if app.scan_errors == 1 { "it" } else { "them" },
            );
            let style = p.style(th.warn, th.panel);
            p.text(x, y, room, &text, style);
        }
        return;
    };
    let n = tree.node(id);
    let mut name = fmt::display_name(tree.name(id));
    if n.is_dir() {
        name.push('/');
    }
    let style = bold(p.style(th.text, th.panel));
    x += p.text(x, y, room.min(40), &name, style) + 2;
    let mut parts: Vec<String> = Vec::new();
    match app.metric {
        Metric::Disk => {
            parts.push(format!("{} on disk", fmt::size(n.disk)));
            if n.size != n.disk {
                parts.push(format!("{} apparent", fmt::size(n.size)));
            }
        }
        Metric::Apparent => {
            parts.push(format!("{} apparent", fmt::size(n.size)));
            parts.push(format!("{} on disk", fmt::size(n.disk)));
        }
    }
    if n.is_dir() {
        parts.push(format!("{} items", fmt::count(n.desc as u64)));
    }
    if !n.has(flag::ERR | flag::SKIPPED) {
        parts.push(format!("modified {}", fmt::date(n.mtime)));
    }
    if n.has(flag::PARTIAL) {
        parts.push("partly unreadable".to_string());
    }
    let text = parts.join("  ·  ");
    if x < right {
        let style = p.style(th.dim, th.panel);
        p.text(x, y, right - x, &text, style);
    }
}

/// A miniature treemap, used as the logo.
fn logo(p: &mut Painter, x: u16, y: u16) {
    let th = p.th;
    let tiles: [(u16, u16, u16, u16, Cat); 5] = [
        (0, 0, 6, 3, Cat::Data),
        (6, 0, 5, 2, Cat::Video),
        (11, 0, 3, 2, Cat::Image),
        (6, 2, 4, 1, Cat::Code),
        (10, 2, 4, 1, Cat::Archive),
    ];
    for (tx, ty, tw, rows, cat) in tiles {
        let colour = th.cat(cat);
        for row in 0..rows {
            for col in 0..tw - 1 {
                let (cx, cy) = (x + tx + col, y + ty + row);
                if row == rows - 1 {
                    // Bottom edge: half a row, like the real tiles.
                    p.put(cx, cy, '▀', colour, th.bg);
                } else {
                    p.put(cx, cy, ' ', colour, colour);
                }
            }
        }
    }
}

pub fn scanning(p: &mut Painter, app: &App, area: Rect) {
    let th = p.th;
    let cx = area.x + area.w / 2;
    let cy = area.y + area.h / 2;
    let centre = |p: &mut Painter, y: u16, text: &str, style| {
        let w = (width(text) as u16).min(area.w.saturating_sub(2));
        p.text(cx.saturating_sub(w / 2), y, w, text, style);
    };

    if area.h >= 14 {
        logo(p, cx.saturating_sub(7), cy.saturating_sub(6));
    }
    let title = bold(p.style(th.accent, th.bg));
    centre(p, cy.saturating_sub(2), "p l a t", title);

    let Some(job) = &app.scan else {
        // No scan and no tree: the scan failed.
        if let Some(t) = &app.toast {
            let style = p.style(th.danger, th.bg);
            centre(p, cy, &t.text, style);
        }
        return;
    };
    let style = p.style(th.dim, th.bg);
    centre(p, cy, &format!("scanning {}", job.path.display()), style);

    // A highlight sliding along a track.
    let bar_w = 44.min(area.w.saturating_sub(8));
    let bx = cx.saturating_sub(bar_w / 2);
    let span = 9u16.min(bar_w);
    let period = (bar_w + span) as u64;
    let head = (app.tick % period.max(1)) as i32 - span as i32;
    for i in 0..bar_w {
        let d = i as i32 - head;
        let colour = if (0..span as i32).contains(&d) {
            th.line.mix(th.accent, (d as f32 + 1.0) / span as f32)
        } else {
            th.line.lighten(-0.06)
        };
        p.put(bx + i, cy + 2, '━', colour, th.bg);
    }

    let pr = &job.progress;
    let stats = format!(
        "{} files    {} directories    {}    {}",
        fmt::count(pr.files.load(Relaxed)),
        fmt::count(pr.dirs.load(Relaxed)),
        fmt::size(pr.bytes.load(Relaxed)),
        fmt::duration(job.started.elapsed().as_secs_f64()),
    );
    let style = p.style(th.text, th.bg);
    centre(p, cy + 4, &stats, style);
    let errors = pr.errors.load(Relaxed);
    if errors > 0 {
        let style = p.style(th.warn, th.bg);
        centre(
            p,
            cy + 5,
            &format!("{} unreadable directories", fmt::count(errors)),
            style,
        );
    }
    let style = p.style(th.faint, th.bg);
    centre(p, area.bottom().saturating_sub(2), "q to cancel", style);
}

/// Darken everything already drawn, so an overlay stands out.
fn scrim(p: &mut Painter, area: Rect) {
    let dim = |c: Color| match c {
        Color::Rgb(r, g, b) => {
            let f = |v: u8| (v as u16 * 2 / 5) as u8;
            Color::Rgb(f(r), f(g), f(b))
        }
        other => other,
    };
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = p.buf.cell_mut((x, y)) {
                cell.fg = dim(cell.fg);
                cell.bg = dim(cell.bg);
            }
        }
    }
}

/// Draw an overlay card of the given size, centred. Returns its inner area.
///
/// The title sits on a band across the top. `alarm` fills that band with a
/// solid colour, for cards that must not be mistaken for routine ones.
fn card(
    p: &mut Painter,
    area: Rect,
    w: u16,
    h: u16,
    title: &str,
    title_fg: Lab,
    alarm: Option<Lab>,
) -> Rect {
    let th = p.th;
    scrim(p, area);
    let w = w.min(area.w.saturating_sub(2));
    let h = h.min(area.h.saturating_sub(2));
    let r = Rect::new(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2, w, h);
    p.fill(r, th.raised);
    let (band, title_fg) = match alarm {
        Some(colour) => (colour, th.ink),
        None => (th.raised.lighten(0.05), title_fg),
    };
    p.fill(Rect::new(r.x, r.y, r.w, 1), band);
    let style = bold(p.style(title_fg, band));
    p.text(r.x + 2, r.y, r.w.saturating_sub(4), title, style);
    Rect::new(
        r.x + 2,
        r.y + 2,
        r.w.saturating_sub(4),
        r.h.saturating_sub(3),
    )
}

pub fn modal(p: &mut Painter, app: &App, modal: &Modal, area: Rect) {
    match modal {
        Modal::Help => help(p, app, area),
        Modal::Confirm(c) => confirm(p, app, c, area),
        Modal::Deleting(d) => deleting(p, app, d, area),
        Modal::Report(r) => report(p, r, area),
    }
}

/// A path with its directory part dimmed, cut from the left if too long.
fn path_line(p: &mut Painter, x: u16, y: u16, room: u16, path: &Path, fg: Lab, bg: Lab) {
    let th = p.th;
    let name = path
        .file_name()
        .map(|n| fmt::display_name(n.as_encoded_bytes()))
        .unwrap_or_default();
    let dir = path
        .parent()
        .map(|d| fmt::display_name(d.as_os_str().as_encoded_bytes()))
        .map(|d| if d.ends_with('/') { d } else { format!("{d}/") })
        .unwrap_or_default();
    let name_w = width(&name) as u16;
    if name_w >= room {
        let style = p.style(fg, bg);
        p.text(x, y, room, &name, style);
        return;
    }
    let dir = truncate_left(&dir, (room - name_w) as usize);
    let dir_w = width(&dir) as u16;
    let style = p.style(th.faint.lighten(0.08), bg);
    p.text(x, y, dir_w, &dir, style);
    let style = bold(p.style(fg, bg));
    p.text(x + dir_w, y, name_w, &name, style);
}

fn button(p: &mut Painter, x: u16, y: u16, label: &str, active: bool, colour: Lab) -> u16 {
    let th = p.th;
    let text = format!("  {label}  ");
    let style = if active {
        bold(p.style(th.ink, colour))
    } else {
        p.style(th.dim, th.raised.lighten(0.07))
    };
    p.text(x, y, width(&text) as u16, &text, style)
}

fn confirm(p: &mut Painter, app: &App, c: &Confirm, area: Rect) {
    let th = p.th;
    let level = c.level();
    let locked = c.locked();

    // How much fits. Everything below the item list is fixed in height, so
    // on a short window the item list gives way first, then the warnings.
    // The first warnings shown are always the most serious ones.
    let per_warning = 2u16;
    let fixed = 3 + app.is_root as u16 + if c.word.is_some() { 2 } else { 0 };
    let warn_rows = |n: u16, total: usize| {
        if total == 0 {
            0
        } else {
            2 + n * per_warning + (total > n as usize) as u16
        }
    };
    let room = area.h.saturating_sub(5);
    let mut shown = c
        .items
        .len()
        .min(if c.warnings.is_empty() { 10 } else { 5 }) as u16;
    let mut warned = c.warnings.len().min(4) as u16;
    while shown > 1 && shown + 1 + warn_rows(warned, c.warnings.len()) + fixed > room {
        shown -= 1;
    }
    while warned > 1 && shown + 1 + warn_rows(warned, c.warnings.len()) + fixed > room {
        warned -= 1;
    }
    let height = shown + 1 + warn_rows(warned, c.warnings.len()) + fixed + 3;

    let title = if c.items.len() == 1 {
        "Delete 1 item".to_string()
    } else {
        format!("Delete {} items", fmt::count(c.items.len() as u64))
    };
    // The band across the top says how serious this is before a word is read.
    let band = match level {
        Some(Level::Critical) => Some(th.danger),
        Some(Level::Caution) => Some(th.warn),
        None => None,
    };
    let inner = card(p, area, 100, height, &title, th.danger.lighten(0.08), band);
    let bg = th.raised;

    // The list never scrolls past its end.
    let top = c.scroll.min(c.items.len().saturating_sub(shown as usize));
    for (i, item) in c.items.iter().enumerate().skip(top).take(shown as usize) {
        let y = inner.y + (i - top) as u16;
        let style = p.style(th.text, bg);
        p.text_right(inner.x + 10, y, 10, &fmt::size(item.size), style);
        let room = inner.w.saturating_sub(12);
        path_line(p, inner.x + 12, y, room, &item.path, th.text, bg);
    }
    if c.items.len() > shown as usize {
        let more = format!(
            "{}-{} of {}   ↑↓ to scroll",
            top + 1,
            top + shown as usize,
            c.items.len()
        );
        let style = p.style(th.faint, bg);
        p.text_right(inner.right(), inner.y + shown, inner.w, &more, style);
    }
    let mut y = inner.y + shown + 1;

    if let Some(level) = level {
        let (label, colour) = match level {
            Level::Critical => ("PROTECTED", th.danger),
            Level::Caution => ("CHECK FIRST", th.warn),
        };
        let used = pill(p, inner.x, y, label, th.ink, colour);
        let lead = match level {
            Level::Critical => "Losing this breaks the system or cannot be undone.",
            Level::Caution => "Make sure this is what you mean to remove.",
        };
        let style = p.style(colour.lighten(0.06), bg);
        p.text(
            inner.x + used + 2,
            y,
            inner.w.saturating_sub(used + 2),
            lead,
            style,
        );
        y += 1;
        for w in c.warnings.iter().take(warned as usize) {
            let colour = match w.level {
                Level::Critical => th.danger.lighten(0.06),
                Level::Caution => th.warn,
            };
            p.put(inner.x, y, '▲', colour, bg);
            let subject = truncate_left(&w.subject, inner.w.saturating_sub(2) as usize);
            let style = bold(p.style(th.text, bg));
            p.text(inner.x + 2, y, inner.w.saturating_sub(2), &subject, style);
            let style = p.style(colour, bg);
            p.text(
                inner.x + 2,
                y + 1,
                inner.w.saturating_sub(2),
                &w.reason,
                style,
            );
            y += per_warning;
        }
        let hidden = c.warnings.len() - warned as usize;
        if hidden > 0 {
            let critical = c.warnings[warned as usize..]
                .iter()
                .filter(|w| w.level == Level::Critical)
                .count();
            let text = if critical > 0 {
                format!("+{hidden} more warnings, {critical} of them protected")
            } else {
                format!("+{hidden} more warnings")
            };
            let style = bold(p.style(th.warn, bg));
            p.text(inner.x + 2, y, inner.w.saturating_sub(2), &text, style);
            y += 1;
        }
        y += 1;
    }

    let frees = format!("Frees {}", fmt::size(c.size));
    let style = bold(p.style(th.text, bg));
    let used = p.text(inner.x, y, inner.w, &frees, style);
    let detail = format!("  ·  {} files and directories", fmt::count(c.entries));
    let style = p.style(th.dim, bg);
    let room = inner.w.saturating_sub(used);
    p.text(inner.x + used, y, room, &detail, style);
    if app.is_root {
        y += 1;
        let style = p.style(th.warn, bg);
        let text = "Running as root: file permissions will not stop this.";
        p.text(inner.x, y, inner.w, text, style);
    }

    if let Some(word) = c.word {
        y += 2;
        let style = p.style(th.text, bg);
        let mut x = inner.x + p.text(inner.x, y, inner.w, "Type", style) + 1;
        x += pill(p, x, y, word, th.ink, th.accent) + 1;
        let style = p.style(th.text, bg);
        x += p.text(x, y, 12, "to unlock:", style) + 1;
        // The input field.
        let field = Rect::new(x, y, 20.min(inner.right().saturating_sub(x)), 1);
        let field_bg = th.raised.lighten(-0.06);
        p.fill(field, field_bg);
        let style = bold(p.style(th.text, field_bg));
        let typed_w = p.text(field.x + 1, y, field.w.saturating_sub(2), &c.typed, style);
        if locked {
            p.put(field.x + 1 + typed_w, y, '▏', th.accent, field_bg);
        } else {
            let style = p.style(th.ok, bg);
            p.text(field.right() + 2, y, 12, "✓ unlocked", style);
        }
    }

    y += 2;
    let mut x = inner.x;
    for choice in c.choices() {
        let (label, colour) = match choice {
            Choice::Cancel => ("Cancel", th.accent),
            Choice::Trash => ("Move to trash", th.warn),
            Choice::Delete => ("Delete permanently", th.danger),
        };
        if locked && choice != Choice::Cancel {
            // Out of reach until the word is typed.
            let text = format!("  {label}  ");
            let style = p.style(th.faint.lighten(-0.08), th.raised.lighten(0.02));
            x += p.text(x, y, width(&text) as u16, &text, style) + 2;
        } else {
            x += button(p, x, y, label, c.choice == choice, colour) + 2;
        }
    }
    let hint = if locked {
        "type the word to unlock   esc cancel"
    } else {
        "←→ choose   enter confirm   esc cancel"
    };
    let style = p.style(th.faint, bg);
    let room = inner.right().saturating_sub(x);
    if room as usize >= width(hint) {
        p.text_right(inner.right(), y, room, hint, style);
    }
}

fn deleting(p: &mut Painter, app: &App, d: &Deleting, area: Rect) {
    let th = p.th;
    let (title, colour) = match d.method {
        Method::Delete => ("Deleting", th.danger.lighten(0.08)),
        Method::Trash(_) => ("Moving to the trash", th.warn),
    };
    let inner = card(p, area, 88, 9, title, colour, None);
    let bg = th.raised;
    let current = d
        .job
        .current
        .load(Relaxed)
        .min(d.items.len().saturating_sub(1));
    let spin = SPINNER[(app.tick as usize / 2) % SPINNER.len()];
    let head = format!("{spin} item {} of {}", current + 1, d.items.len());
    let style = p.style(th.text, bg);
    p.text(inner.x, inner.y, inner.w, &head, style);
    if let Some(item) = d.items.get(current) {
        path_line(p, inner.x, inner.y + 1, inner.w, &item.path, th.text, bg);
    }

    let removed = d.job.counters.removed.load(Relaxed);
    let frac = if d.entries == 0 {
        0.0
    } else {
        (removed as f64 / d.entries as f64).min(1.0)
    };
    let filled = (frac * inner.w as f64 * 2.0).round() as u32;
    for i in 0..inner.w {
        let (ch, fg) = match filled.saturating_sub(i as u32 * 2) {
            0 => ('━', th.line),
            1 => ('╸', colour),
            _ => ('━', colour),
        };
        p.put(inner.x + i, inner.y + 3, ch, fg, bg);
    }
    let counts = format!(
        "{} of {} files and directories",
        fmt::count(removed),
        fmt::count(d.entries)
    );
    let style = p.style(th.dim, bg);
    p.text(inner.x, inner.y + 4, inner.w, &counts, style);
    let stopping = d.job.counters.cancel.load(Relaxed);
    let hint = if stopping {
        "stopping…"
    } else {
        "esc to stop"
    };
    let style = p.style(th.faint, bg);
    p.text_right(inner.right(), inner.y + 4, 20, hint, style);
}

fn report(p: &mut Painter, r: &Report, area: Rect) {
    let th = p.th;
    let shown = r.failures.len().min(8) as u16;
    let inner = card(p, area, 96, shown * 2 + 7, r.title, th.warn, None);
    let bg = th.raised;
    let style = p.style(th.text, bg);
    p.text(inner.x, inner.y, inner.w, &r.summary, style);

    let top = r
        .scroll
        .min(r.failures.len().saturating_sub(shown as usize));
    for (i, (path, reason)) in r.failures.iter().enumerate().skip(top).take(shown as usize) {
        let y = inner.y + 2 + (i - top) as u16 * 2;
        path_line(p, inner.x, y, inner.w, Path::new(path), th.text, bg);
        let style = p.style(th.warn, bg);
        p.text(inner.x + 2, y + 1, inner.w.saturating_sub(2), reason, style);
    }
    let y = inner.y + 2 + shown * 2 + 1;
    let hint = if r.failures.len() > shown as usize {
        "↑↓ to scroll   enter to close"
    } else {
        "enter to close"
    };
    let style = p.style(th.faint, bg);
    p.text_right(
        inner.right(),
        y.min(inner.bottom().saturating_sub(1)),
        inner.w,
        hint,
        style,
    );
}

fn help(p: &mut Painter, _app: &App, area: Rect) {
    let th = p.th;
    const LEFT: &[(&str, &str)] = &[
        ("MOVE", ""),
        ("j  k", "next / previous"),
        ("l  enter", "open directory"),
        ("h  backspace", "parent directory"),
        ("← ↓ ↑ →  H J K L", "map: neighbouring tile"),
        ("tab", "switch map / list"),
        ("g  G", "first / last"),
        ("pgup  pgdn", "page through the list"),
        ("", ""),
        ("VIEW", ""),
        ("1  2  3", "files / largest / types"),
        ("a", "disk / apparent size"),
        ("s", "sort order"),
        ("+  -", "treemap depth"),
        ("", ""),
        ("", ""),
    ];
    const RIGHT: &[(&str, &str)] = &[
        ("ACT", ""),
        ("space", "mark or unmark"),
        ("d  del", "delete marked items"),
        ("u", "clear all marks"),
        ("r  f5", "rescan this directory"),
        ("y", "copy path"),
        ("o", "open with default app"),
        ("!", "shell in this directory"),
        ("?", "this help"),
        ("q", "quit"),
        ("", ""),
        ("MOUSE", ""),
        ("click", "select"),
        ("double click", "open"),
        ("right click", "parent directory"),
        ("wheel on map", "zoom in / out"),
    ];
    let inner = card(p, area, 92, LEFT.len() as u16 + 4, "Keys", th.accent, None);
    let col_w = inner.w / 2;
    for (col, rows) in [LEFT, RIGHT].into_iter().enumerate() {
        let x = inner.x + col as u16 * col_w;
        for (i, (key, what)) in rows.iter().enumerate() {
            let y = inner.y + i as u16;
            if y >= inner.bottom() {
                break;
            }
            if what.is_empty() {
                let style = bold(p.style(th.faint.lighten(0.05), th.raised));
                p.text(x, y, col_w, key, style);
                continue;
            }
            let ks = bold(p.style(th.accent, th.raised));
            p.text(x, y, 18.min(col_w), key, ks);
            let vs = p.style(th.text, th.raised);
            if col_w > 20 {
                p.text(x + 19, y, col_w - 20, what, vs);
            }
        }
    }
}
