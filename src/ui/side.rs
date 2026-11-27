// SPDX-License-Identifier: GPL-3.0-or-later
//! The side pane: children of the current directory, the largest files below
//! it, and a breakdown by file type.

use ratatui::style::Modifier;

use super::{Painter, bold, truncate_left, width};
use crate::app::{App, Hit, Pane, Sort, Tab};
use crate::ext::{self, Cat};
use crate::fmt;
use crate::fsinfo;
use crate::theme::Lab;
use crate::tree::{NodeId, Tree, flag};
use crate::treemap::Rect;

pub fn draw(p: &mut Painter, app: &App, pane: Rect, hit: &mut Hit, scroll: &mut [usize; 3]) {
    let Some(tree) = &app.tree else {
        return;
    };
    let th = p.th;
    p.fill(pane, th.panel);
    tabs(p, app, pane, hit);
    if pane.h < 3 {
        return;
    }
    let head = Rect::new(pane.x, pane.y + 1, pane.w, 1);
    let list = Rect::new(pane.x, pane.y + 2, pane.w, pane.h - 2);
    hit.list = list;

    let (len, sel) = match app.tab {
        Tab::Files => (app.rows.len(), app.cursor_row().unwrap_or(0)),
        Tab::Largest => (app.largest.len(), app.largest_sel),
        Tab::Types => (app.types.len(), app.types_sel),
    };
    // Keep the selection on screen, with a little context around it.
    let h = list.h as usize;
    let top = &mut scroll[app.tab as usize];
    let margin = 2.min(h.saturating_sub(1) / 2);
    if sel < *top + margin {
        *top = sel.saturating_sub(margin);
    }
    if sel + margin + 1 > *top + h {
        *top = sel + margin + 1 - h;
    }
    *top = (*top).min(len.saturating_sub(h));
    let top = *top;

    let cats = ext::categories();
    let view = Columns::for_width(list.w, app.sort);
    let cx = ListCtx {
        app,
        tree,
        cats: &cats,
        list,
    };
    match app.tab {
        Tab::Files => {
            files_header(p, app, head, &view);
            let total = app.rows_total.max(1);
            let largest = app.rows_largest.max(1);
            let now = fmt::now();
            for (i, &id) in app.rows.iter().enumerate().skip(top).take(h) {
                let y = list.y + (i - top) as u16;
                let row = FileRow {
                    id,
                    selected: i == sel,
                    total,
                    largest,
                    now,
                };
                file_row(p, &cx, &view, y, &row);
            }
            if app.rows.is_empty() {
                let unreadable = tree.node(app.view).has(flag::ERR);
                let msg = match (unreadable, app.is_root) {
                    (true, false) => "could not be read; try running with sudo",
                    (true, true) => "could not be read",
                    _ => "empty directory",
                };
                empty(p, list, msg);
            }
        }
        Tab::Largest => {
            let style = p.style(th.faint, th.panel);
            p.text(
                head.x + 3,
                head.y,
                head.w.saturating_sub(4),
                "      SIZE  FILE",
                style,
            );
            for (i, &id) in app.largest.iter().enumerate().skip(top).take(h) {
                let y = list.y + (i - top) as u16;
                largest_row(p, &cx, y, id, i == sel);
            }
            if app.largest.is_empty() {
                empty(p, list, "no files below this directory");
            }
        }
        Tab::Types => {
            let style = p.style(th.faint, th.panel);
            p.text(head.x + 3, head.y, head.w.saturating_sub(4), "TYPE", style);
            let total = app.types.iter().map(|t| t.size).sum::<u64>().max(1);
            let largest = app.types.first().map_or(1, |t| t.size).max(1);
            for (i, t) in app.types.iter().enumerate().skip(top).take(h) {
                let y = list.y + (i - top) as u16;
                type_row(p, &cx, y, t, (total, largest), i == sel);
            }
            if app.types.is_empty() {
                empty(p, list, "no files below this directory");
            }
        }
    }
    scrollbar(p, list, len, top);
}

fn empty(p: &mut Painter, list: Rect, msg: &str) {
    let style = p.style(p.th.faint, p.th.panel);
    p.text(list.x + 3, list.y + 1, list.w.saturating_sub(4), msg, style);
}

fn tabs(p: &mut Painter, app: &App, pane: Rect, hit: &mut Hit) {
    let th = p.th;
    let focused = app.focus == Pane::Side;
    p.put(
        pane.x,
        pane.y,
        '▍',
        if focused { th.accent } else { th.line },
        th.panel,
    );
    let mut x = pane.x + 2;
    for (i, tab) in Tab::ALL.into_iter().enumerate() {
        let label = format!(" {} {} ", i + 1, tab.label());
        let w = width(&label) as u16;
        if x + w > pane.right() {
            break;
        }
        let style = if tab == app.tab {
            let bg = if focused { th.accent } else { th.dim };
            bold(p.style(th.ink, bg))
        } else {
            p.style(th.dim, th.panel)
        };
        p.text(x, pane.y, w, &label, style);
        if tab != app.tab {
            // Dim the digit so the label carries the weight.
            let digit = p.style(th.faint, th.panel);
            p.text(x + 1, pane.y, 1, &(i + 1).to_string(), digit);
        }
        hit.tabs.push((Rect::new(x, pane.y, w, 1), tab));
        x += w + 1;
    }
}

/// Column positions of the Files tab for a given pane width.
struct Columns {
    pct_right: u16,
    bar_x: u16,
    bar_w: u16,
    size_right: u16,
    name_x: u16,
    /// Right edge of the name column.
    name_right: u16,
    /// Right edges of the optional columns, if they fit.
    items_right: Option<u16>,
    age_right: Option<u16>,
}

impl Columns {
    /// `sort` decides which optional column gets the space when only one
    /// fits: the one the list is sorted by.
    fn for_width(w: u16, sort: Sort) -> Columns {
        // Mark gutter (3) | percent (6) | bar | size (10) | name | items | age
        let bar_w: u16 = if w >= 70 {
            12
        } else if w >= 52 {
            8
        } else {
            5
        };
        let pct_right = 3 + 6;
        let bar_x = pct_right + 1;
        let size_right = bar_x + bar_w + 11;
        let name_x = size_right + 2;
        let mut right = w.saturating_sub(2);
        let (show_items, show_age) = match (w >= 74, w >= 62, sort) {
            (true, _, _) => (true, true),
            (false, true, Sort::Items) => (true, false),
            (false, true, _) => (false, true),
            (false, false, _) => (false, false),
        };
        let age_right = show_age.then(|| {
            let r = right;
            right = right.saturating_sub(8);
            r
        });
        let items_right = show_items.then(|| {
            let r = right;
            right = right.saturating_sub(9);
            r
        });
        Columns {
            pct_right,
            bar_x,
            bar_w,
            size_right,
            name_x,
            name_right: right,
            items_right,
            age_right,
        }
    }
}

fn files_header(p: &mut Painter, app: &App, head: Rect, c: &Columns) {
    let th = p.th;
    let plain = p.style(th.faint, th.panel);
    let active = p.style(th.dim, th.panel).add_modifier(Modifier::BOLD);
    let pick = |s: Sort| if app.sort == s { active } else { plain };
    let label = |s: Sort, text: &str| {
        if app.sort == s {
            format!("{text} ↓")
        } else {
            text.to_string()
        }
    };
    p.text_right(
        head.x + c.size_right,
        head.y,
        10,
        &label(Sort::Size, "SIZE"),
        pick(Sort::Size),
    );
    // If the column the list is sorted by has no room, say so by the name.
    let hidden = match app.sort {
        Sort::Items if c.items_right.is_none() => Some("NAME · by items ↓"),
        Sort::Modified if c.age_right.is_none() => Some("NAME · by age ↓"),
        _ => None,
    };
    p.text(
        head.x + c.name_x,
        head.y,
        c.name_right.saturating_sub(c.name_x),
        hidden.unwrap_or(&label(Sort::Name, "NAME")),
        if hidden.is_some() {
            active
        } else {
            pick(Sort::Name)
        },
    );
    if let Some(r) = c.items_right {
        p.text_right(
            head.x + r,
            head.y,
            8,
            &label(Sort::Items, "ITEMS"),
            pick(Sort::Items),
        );
    }
    if let Some(r) = c.age_right {
        p.text_right(
            head.x + r,
            head.y,
            7,
            &label(Sort::Modified, "AGE"),
            pick(Sort::Modified),
        );
    }
}

/// Background of a list row, and the selection bar in its first column.
fn row_base(p: &mut Painter, app: &App, list: Rect, y: u16, selected: bool) -> Lab {
    let th = p.th;
    let focused = app.focus == Pane::Side;
    let bg = match (selected, focused) {
        (true, true) => th.raised.lighten(0.035),
        (true, false) => th.raised.lighten(-0.02),
        _ => th.panel,
    };
    if selected {
        p.fill(Rect::new(list.x, y, list.w, 1), bg);
        if focused {
            p.put(list.x, y, '▌', th.accent, bg);
        }
    }
    bg
}

/// Colours of a bar: the filled part, the empty track, and what is behind.
struct BarLook {
    fg: Lab,
    track: Lab,
    bg: Lab,
}

/// A slim horizontal bar across `at`, filled to `frac` (0..=1), in half-cell
/// steps.
fn bar(p: &mut Painter, at: Rect, frac: f64, look: BarLook) {
    let Rect { x, y, w, .. } = at;
    let BarLook { fg, track, bg } = look;
    let halves = (frac.clamp(0.0, 1.0) * w as f64 * 2.0).round() as u32;
    // Anything non-zero gets at least a sliver.
    let halves = if frac > 0.0 { halves.max(1) } else { 0 };
    for i in 0..w {
        match halves.saturating_sub(i as u32 * 2) {
            0 => p.put(x + i, y, '━', track, bg),
            1 => p.put(x + i, y, '╸', fg, bg),
            _ => p.put(x + i, y, '━', fg, bg),
        }
    }
}

struct FileRow {
    id: NodeId,
    selected: bool,
    /// Sum over all rows, for the percentage.
    total: u64,
    /// Largest row, for the bar length.
    largest: u64,
    now: i64,
}

fn node_colour(p: &Painter, n: &crate::tree::Node, cats: &[Cat]) -> Lab {
    if n.is_dir() {
        p.th.dir.lighten(0.12)
    } else if n.is_file() {
        p.th.cat(cats.get(n.ext as usize).copied().unwrap_or(Cat::Other))
    } else {
        p.th.cat(Cat::Other)
    }
}

/// What every row painter needs to know about the list it is in.
struct ListCtx<'a> {
    app: &'a App,
    tree: &'a Tree,
    cats: &'a [Cat],
    list: Rect,
}

fn file_row(p: &mut Painter, cx: &ListCtx, c: &Columns, y: u16, row: &FileRow) {
    let ListCtx {
        app,
        tree,
        cats,
        list,
    } = *cx;
    let th = p.th;
    let n = tree.node(row.id);
    let bg = row_base(p, app, list, y, row.selected);
    let marked = app.is_marked(row.id);
    let x0 = list.x;
    if marked {
        let style = bold(p.style(th.danger, bg));
        p.text(x0 + 1, y, 1, "✗", style);
    }

    let value = n.get(app.metric);
    let skipped = n.has(flag::SKIPPED);
    if !skipped {
        let pct = value as f64 / row.total as f64 * 100.0;
        let pct_text = if pct >= 99.95 {
            "100%".to_string()
        } else {
            format!("{pct:.1}%")
        };
        let style = p.style(if row.selected { th.text } else { th.dim }, bg);
        p.text_right(x0 + c.pct_right, y, 6, &pct_text, style);
        let colour = if marked {
            th.danger
        } else {
            node_colour(p, n, cats)
        };
        let track = if row.selected {
            bg.lighten(0.07)
        } else {
            th.raised
        };
        let frac = value as f64 / row.largest as f64;
        let at = Rect::new(x0 + c.bar_x, y, c.bar_w, 1);
        let look = BarLook {
            fg: colour,
            track,
            bg,
        };
        bar(p, at, frac, look);
        let partial = if n.has(flag::PARTIAL) { "≥ " } else { "" };
        let size = format!("{partial}{}", fmt::size(value));
        let style = p.style(th.text, bg);
        p.text_right(x0 + c.size_right, y, 11, &size, style);
    }

    // Name, with a short note for anything unusual.
    let mut name = fmt::display_name(tree.name(row.id));
    if n.is_dir() {
        name.push('/');
    }
    let note = if skipped {
        Some(format!("{} mount, not scanned", fsinfo::fs_name(n.ext)))
    } else if n.is_dir() && n.has(flag::ERR) {
        Some("unreadable".to_string())
    } else if n.has(flag::MOUNT) {
        Some("mount".to_string())
    } else if n.has(flag::HARDLINK) {
        Some("hard link, counted elsewhere".to_string())
    } else if n.kind() == flag::SYMLINK {
        Some("symlink".to_string())
    } else {
        None
    };
    // Things that take a typed confirmation to delete carry a quiet label, so
    // what is essential can be told apart before anything is marked. Mount
    // points cannot be deleted at all and need no such hint.
    let protected = !n.has(flag::MOUNT) && app.is_protected(row.id);
    let note = match (note, protected) {
        (Some(note), true) => Some(format!("{note}, protected")),
        (None, true) => Some("protected".to_string()),
        (note, false) => note,
    };
    let name_fg = if marked {
        th.danger.lighten(0.08)
    } else if skipped || n.has(flag::HARDLINK) {
        th.faint
    } else if n.is_dir() {
        th.text
    } else {
        th.dim.lighten(0.1)
    };
    let mut style = p.style(name_fg, bg);
    if n.is_dir() && !skipped {
        style = bold(style);
    }
    let room = (x0 + c.name_right).saturating_sub(x0 + c.name_x);
    let used = p.text(x0 + c.name_x, y, room, &name, style);
    if let Some(note) = note {
        let note_fg = if n.has(flag::ERR) { th.warn } else { th.faint };
        let style = p.style(note_fg, bg);
        if used + 2 < room {
            p.text(x0 + c.name_x + used + 2, y, room - used - 2, &note, style);
        }
    }

    let aux = p.style(th.faint.lighten(0.06), bg);
    if let Some(r) = c.items_right
        && n.is_dir()
        && !skipped
    {
        // Seven digits with separators no longer fit the column.
        let full = fmt::count(n.desc as u64);
        let items = if full.len() > 8 {
            fmt::count_short(n.desc as u64)
        } else {
            full
        };
        p.text_right(x0 + r, y, 8, &items, aux);
    }
    if let Some(r) = c.age_right
        && !skipped
        && !n.has(flag::ERR)
    {
        p.text_right(x0 + r, y, 7, &fmt::age(n.mtime, row.now), aux);
    }
}

fn largest_row(p: &mut Painter, cx: &ListCtx, y: u16, id: NodeId, selected: bool) {
    let ListCtx {
        app,
        tree,
        cats,
        list,
    } = *cx;
    let th = p.th;
    let n = tree.node(id);
    let bg = row_base(p, app, list, y, selected);
    let marked = app.is_marked(id);
    if marked {
        let style = bold(p.style(th.danger, bg));
        p.text(list.x + 1, y, 1, "✗", style);
    }
    let colour = if marked {
        th.danger
    } else {
        node_colour(p, n, cats)
    };
    p.put(list.x + 3, y, '▌', colour, bg);
    let style = p.style(th.text, bg);
    p.text_right(list.x + 14, y, 10, &fmt::size(n.get(app.metric)), style);

    // Path relative to the view, cut from the left so the file name stays.
    let x = list.x + 16;
    let room = (list.right().saturating_sub(2)).saturating_sub(x) as usize;
    let rel = tree.rel_path(id, app.view);
    let dir = rel
        .parent()
        .map(|d| fmt::display_name(d.as_os_str().as_encoded_bytes()))
        .filter(|d| !d.is_empty())
        .map(|d| format!("{d}/"))
        .unwrap_or_default();
    let name = fmt::display_name(tree.name(id));
    let name_w = width(&name);
    let name_fg = if marked {
        th.danger.lighten(0.08)
    } else {
        th.text
    };
    if name_w >= room {
        let style = p.style(name_fg, bg);
        p.text(x, y, room as u16, &name, style);
    } else {
        let dir = truncate_left(&dir, room - name_w);
        let dir_w = width(&dir) as u16;
        let style = p.style(th.faint.lighten(0.05), bg);
        p.text(x, y, dir_w, &dir, style);
        let style = p.style(name_fg, bg);
        p.text(x + dir_w, y, name_w as u16, &name, style);
    }
}

/// `scale` is (sum of all types, largest type), for the percentage and bar.
fn type_row(
    p: &mut Painter,
    cx: &ListCtx,
    y: u16,
    t: &crate::app::TypeRow,
    scale: (u64, u64),
    selected: bool,
) {
    let ListCtx {
        app, cats, list, ..
    } = *cx;
    let (total, largest) = scale;
    let th = p.th;
    let bg = row_base(p, app, list, y, selected);
    let cat = cats.get(t.ext as usize).copied().unwrap_or(Cat::Other);
    let colour = th.cat(cat);
    p.put(list.x + 3, y, '■', colour, bg);
    let name = match t.ext {
        ext::EXT_NONE => "(no extension)".to_string(),
        ext::EXT_EXEC => "(executable)".to_string(),
        id => format!(".{}", ext::name(id)),
    };
    let name_style = p.style(th.text, bg);
    let used = p.text(list.x + 5, y, 15, &name, name_style);
    if list.w >= 60 && cat != Cat::Other && used < 12 {
        let style = p.style(th.faint, bg);
        p.text(list.x + 5 + used + 1, y, 15 - used - 1, cat.label(), style);
    }

    let mut x = list.x + 21;
    let style = p.style(th.text, bg);
    p.text_right(x + 10, y, 10, &fmt::size(t.size), style);
    x += 11;
    let pct = t.size as f64 / total as f64 * 100.0;
    let style = p.style(if selected { th.text } else { th.dim }, bg);
    p.text_right(x + 6, y, 6, &format!("{pct:.1}%"), style);
    x += 7;
    let right = list.right().saturating_sub(2);
    let count = format!("{} files", fmt::count(t.count));
    let count_w = 16u16;
    let bar_w = right.saturating_sub(x).saturating_sub(count_w + 1).min(14);
    if bar_w >= 4 {
        let track = if selected {
            bg.lighten(0.07)
        } else {
            th.raised
        };
        let look = BarLook {
            fg: colour,
            track,
            bg,
        };
        let frac = t.size as f64 / largest as f64;
        bar(p, Rect::new(x, y, bar_w, 1), frac, look);
    }
    if right > x + count_w {
        let style = p.style(th.faint.lighten(0.06), bg);
        p.text_right(right, y, count_w, &count, style);
    }
}

/// A slim scrollbar on the right edge, shown only when the list overflows.
fn scrollbar(p: &mut Painter, list: Rect, len: usize, top: usize) {
    let h = list.h as usize;
    if len <= h || h == 0 {
        return;
    }
    let th = p.th;
    let thumb = (h * h / len).max(1);
    let start = (top * (h - thumb) + (len - h) / 2) / (len - h);
    let x = list.right() - 1;
    for i in 0..h {
        let on = i >= start && i < start + thumb;
        p.put(
            x,
            list.y + i as u16,
            '▐',
            if on { th.faint } else { th.panel.lighten(0.04) },
            th.panel,
        );
    }
}
