// SPDX-License-Identifier: GPL-3.0-or-later
//! The treemap pane.

use ratatui::style::Modifier;

use super::{Painter, bold};
use crate::app::{App, NONE, Pane, Tab};
use crate::ext::Cat;
use crate::fmt;
use crate::theme::Lab;
use crate::tree::{Metric, NodeId, Tree, flag};
use crate::treemap::{Rect, Tile, TileKind};

/// The part of the map pane that holds tiles: below the title row, with a
/// one-column margin on either side.
pub fn content_area(pane: Rect) -> Rect {
    Rect::new(
        pane.x + 1,
        pane.y + 1,
        pane.w.saturating_sub(2),
        pane.h.saturating_sub(1),
    )
}

pub fn draw(p: &mut Painter, app: &App, pane: Rect) {
    let Some(tree) = &app.tree else {
        return;
    };
    title(p, app, tree, pane);
    let area = content_area(pane);
    if area.w == 0 || area.h == 0 {
        return;
    }
    if app.tiles.is_empty() {
        let view = tree.node(app.view);
        let msg = if view.has(flag::ERR) {
            "this directory could not be read"
        } else if view.desc == 0 {
            "empty directory"
        } else {
            "nothing in here takes up space"
        };
        let style = p.style(p.th.faint, p.th.bg);
        let w = super::width(msg) as u16;
        p.text(
            area.x + area.w.saturating_sub(w) / 2,
            area.y + area.h / 2,
            area.w,
            msg,
            style,
        );
        return;
    }

    let spotlight = app.spotlight();
    let hl = Highlight::of(app, tree);
    for (i, tile) in app.tiles.iter().enumerate() {
        let selected = hl.selects(i, tile);
        let marked = tile.kind != TileKind::Rest && app.is_marked(tile.node);
        match tile.kind {
            TileKind::Frame => frame(p, tree, tile, app.metric, selected, marked),
            TileKind::Leaf => {
                let look = Look {
                    metric: app.metric,
                    spotlight,
                };
                leaf(p, tree, tile, &look, selected, marked)
            }
            TileKind::Rest => rest(p, tile, selected, spotlight.is_some()),
        }
    }
}

/// Which tile, if any, is drawn as selected.
enum Highlight {
    None,
    /// The top-level tile for this child of the view.
    Top(NodeId),
    /// The cursor has no tile of its own; light up the rest tile.
    TopRest,
    /// One specific tile, by index.
    Tile(usize),
}

impl Highlight {
    fn of(app: &App, tree: &Tree) -> Highlight {
        if app.focus == Pane::Side && app.tab == Tab::Types {
            return Highlight::None;
        }
        if app.focus == Pane::Side && app.tab == Tab::Largest {
            // Deepest tile that is the file or contains it.
            let Some(&id) = app.largest.get(app.largest_sel) else {
                return Highlight::None;
            };
            return app
                .tiles
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    t.kind != TileKind::Rest && (t.node == id || tree.is_below(id, t.node))
                })
                .max_by_key(|(_, t)| t.depth)
                .map_or(Highlight::None, |(i, _)| Highlight::Tile(i));
        }
        if app.cursor == NONE {
            return Highlight::None;
        }
        let has_tile = app
            .tiles
            .iter()
            .any(|t| t.depth == 0 && t.kind != TileKind::Rest && t.top == app.cursor);
        if has_tile {
            Highlight::Top(app.cursor)
        } else {
            Highlight::TopRest
        }
    }

    fn selects(&self, index: usize, tile: &Tile) -> bool {
        match *self {
            Highlight::None => false,
            Highlight::Top(id) => tile.depth == 0 && tile.kind != TileKind::Rest && tile.top == id,
            Highlight::TopRest => tile.depth == 0 && tile.kind == TileKind::Rest,
            Highlight::Tile(i) => i == index,
        }
    }
}

fn title(p: &mut Painter, app: &App, tree: &Tree, pane: Rect) {
    let th = p.th;
    let y = pane.y;
    let focused = app.focus == Pane::Map;
    p.put(
        pane.x,
        y,
        '▍',
        if focused { th.accent } else { th.line },
        th.bg,
    );
    let n = tree.node(app.view);
    let name = if app.view == Tree::ROOT {
        tree.root_path().display().to_string()
    } else {
        format!("{}/", fmt::display_name(tree.name(app.view)))
    };
    let mut x = pane.x + 2;
    let right = pane.right().saturating_sub(1);
    let name_style = bold(p.style(if focused { th.accent } else { th.text }, th.bg));
    x += p.text(x, y, right.saturating_sub(x).min(40), &name, name_style) + 2;

    let partial = if n.has(flag::PARTIAL) { "≥ " } else { "" };
    let detail = format!(
        "{partial}{}  ·  {} items",
        fmt::size(n.get(app.metric)),
        fmt::count(n.desc as u64)
    );
    let detail_style = p.style(th.dim, th.bg);
    if x < right {
        x += p.text(x, y, right - x, &detail, detail_style) + 2;
    }

    let settings = format!(
        "{}  ·  depth {}",
        match app.metric {
            Metric::Disk => "disk usage",
            Metric::Apparent => "apparent size",
        },
        app.depth
    );
    let w = super::width(&settings) as u16;
    if x + w < right {
        let style = p.style(th.faint, th.bg);
        p.text_right(right, y, w, &settings, style);
    }
}

/// Vertical shading: rows get slightly darker toward the bottom of a tile.
/// Kept faint, so it softens large tiles without banding small ones.
fn shade(base: Lab, row: u16, rows: u16) -> Lab {
    if rows < 2 {
        return base;
    }
    let t = row as f32 / (rows - 1) as f32;
    base.lighten(0.012 - 0.024 * t)
}

/// Paint the body of a tile in `base`, including its half-row bottom edge.
/// `hatch` overlays diagonal strokes in the given colour.
fn body(p: &mut Painter, tile: &Tile, base: Lab, hatch: Option<Lab>, shaded: bool) {
    let r = tile.rect;
    let behind = p.th.surface(tile.depth);
    for row in 0..r.h {
        let colour = if shaded { shade(base, row, r.h) } else { base };
        let half = tile.gap_b && row == r.h - 1;
        for x in r.x..r.x + tile.body_w() {
            if half {
                p.put(x, r.y + row, '▀', colour, behind);
            } else if let Some(stroke) = hatch {
                p.put(x, r.y + row, '╱', stroke, colour);
            } else {
                p.put(x, r.y + row, ' ', colour, colour);
            }
        }
    }
}

/// Settings that decide how a leaf tile is coloured and labelled.
struct Look {
    metric: Metric,
    spotlight: Option<u16>,
}

fn leaf(p: &mut Painter, tree: &Tree, tile: &Tile, look: &Look, selected: bool, marked: bool) {
    let th = p.th;
    let n = tree.node(tile.node);
    let is_dir = n.is_dir();
    let mut base = match (is_dir, tile.cat) {
        // A directory takes a muted tint of whatever fills most of it.
        (true, Some(cat)) => th.dir.mix(th.cat(cat), 0.4),
        (true, None) => th.dir,
        (false, cat) => th.cat(cat.unwrap_or(Cat::Other)),
    };
    let mut ink = if is_dir { th.text } else { th.ink };
    let mut hatch = None;
    if let Some(ext) = look.spotlight
        && !(n.is_file() && n.ext == ext)
    {
        base = base.mix(th.surface(tile.depth), 0.78);
        ink = th.faint;
    }
    if marked {
        base = th.danger;
        ink = th.ink;
        hatch = Some(th.danger_dark);
    }
    if selected {
        base = if marked {
            th.danger.lighten(0.14)
        } else {
            th.accent
        };
        ink = th.ink;
    }
    body(p, tile, base, hatch, true);

    // Label: name on the first row, size below it or beside it.
    let r = tile.rect;
    let room = tile.body_w().saturating_sub(2);
    if tile.full_rows() == 0 || room < 3 {
        return;
    }
    let mut name = fmt::display_name(tree.name(tile.node));
    if is_dir {
        name.push('/');
    }
    let size = fmt::size_short(n.get(look.metric));
    let row_bg = |row: u16| shade(base, row, r.h);
    let mut name_style = p.style(ink, row_bg(0));
    if selected || is_dir {
        name_style = name_style.add_modifier(Modifier::BOLD);
    }
    let used = p.text(r.x + 1, r.y, room, &name, name_style);
    let size_ink = ink.mix(base, 0.38);
    let size_w = super::width(&size) as u16;
    if tile.full_rows() >= 2 {
        let style = p.style(size_ink, row_bg(1));
        p.text(r.x + 1, r.y + 1, room, &size, style);
    } else if used + 1 + size_w <= room {
        let style = p.style(size_ink, row_bg(0));
        p.text_right(r.x + 1 + room, r.y, size_w, &size, style);
    }
}

fn frame(p: &mut Painter, tree: &Tree, tile: &Tile, metric: Metric, selected: bool, marked: bool) {
    let th = p.th;
    let n = tree.node(tile.node);
    let r = tile.rect;
    let mut surface = th.surface(tile.depth + 1);
    if marked {
        surface = surface.mix(th.danger, 0.22);
    }
    if selected {
        surface = surface.lighten(0.035);
    }
    body(p, tile, surface, None, false);

    // Header strip.
    let (head_bg, head_fg, detail_fg) = match (selected, marked) {
        (true, false) => (th.accent, th.ink, th.ink.mix(th.accent, 0.35)),
        (true, true) => (th.danger.lighten(0.14), th.ink, th.ink.mix(th.danger, 0.3)),
        (false, true) => (th.danger, th.ink, th.ink.mix(th.danger, 0.3)),
        (false, false) => (
            surface,
            if tile.depth == 0 {
                th.text
            } else {
                th.dim.lighten(0.1)
            },
            th.faint.lighten(0.06),
        ),
    };
    let body_w = tile.body_w();
    if selected || marked {
        p.fill(Rect::new(r.x, r.y, body_w, 1), head_bg);
    }
    let room = body_w.saturating_sub(2);
    let name = format!("{}/", fmt::display_name(tree.name(tile.node)));
    let mut style = p.style(head_fg, head_bg);
    if tile.depth == 0 || selected {
        style = bold(style);
    }
    let used = p.text(r.x + 1, r.y, room, &name, style);
    let partial = if n.has(flag::PARTIAL) { "≥" } else { "" };
    let size = format!("{partial}{}", fmt::size_short(n.get(metric)));
    let size_w = super::width(&size) as u16;
    if used + 2 + size_w <= room {
        let style = p.style(detail_fg, head_bg);
        p.text_right(r.x + 1 + room, r.y, size_w, &size, style);
    }
}

fn rest(p: &mut Painter, tile: &Tile, selected: bool, dimmed: bool) {
    let th = p.th;
    let r = tile.rect;
    let behind = th.surface(tile.depth);
    let bg = behind.lighten(if dimmed { 0.012 } else { 0.03 });
    let dot = if selected {
        th.accent
    } else {
        th.line.lighten(0.04)
    };
    for row in 0..r.h {
        let half = tile.gap_b && row == r.h - 1;
        for x in r.x..r.x + tile.body_w() {
            let y = r.y + row;
            if half {
                p.put(x, y, '▀', bg, behind);
            } else if (x + y).is_multiple_of(2) {
                p.put(x, y, '·', dot, bg);
            } else {
                p.put(x, y, ' ', bg, bg);
            }
        }
    }
    let room = tile.body_w().saturating_sub(2);
    if tile.full_rows() == 0 || room < 5 {
        return;
    }
    let long = format!(
        " +{} more  {} ",
        fmt::count(tile.rest_count as u64),
        fmt::size_short(tile.rest_size)
    );
    let short = format!(" +{} ", fmt::count_short(tile.rest_count as u64));
    let label = if super::width(&long) as u16 <= room {
        long
    } else {
        short
    };
    if super::width(&label) as u16 > room {
        return;
    }
    let mut style = p.style(if selected { th.accent } else { th.dim }, bg);
    if selected {
        style = bold(style);
    }
    p.text(r.x + 1, r.y, room, &label, style);
}
