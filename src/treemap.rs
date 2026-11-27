// SPDX-License-Identifier: GPL-3.0-or-later
//! Nested squarified treemap layout on a terminal cell grid.
//!
//! The layout works in cells. A tile owns a rectangle of cells, but it only
//! paints part of it: the rightmost column and the lower half of the bottom
//! row are left as a gap, unless the tile touches the edge of its region.
//! With a cell roughly twice as tall as it is wide, that makes every gap the
//! same visual thickness.
//!
//! A directory that is large enough becomes a frame: one header row, then its
//! own children laid out inside it. Everything too small to draw is folded
//! into a single "rest" tile per directory.

use crate::ext::Cat;
use crate::tree::{Metric, NodeId, Tree, flag};

/// Height of a cell divided by its width, times a bias toward wide tiles,
/// which hold labels far better than square ones.
const ASPECT: f64 = 3.0;
/// Smallest area, in cells, a child needs to get its own tile.
const MIN_TILE_CELLS: f64 = 3.0;
/// Most tiles a single directory is split into.
const MAX_TILES: usize = 256;
/// Smallest painted width of a directory that is shown as a frame.
const MIN_FRAME_W: u16 = 9;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> u16 {
        self.x + self.w
    }
    pub fn bottom(&self) -> u16 {
        self.y + self.h
    }
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TileKind {
    /// A file, or a directory drawn as a single block.
    Leaf,
    /// A directory drawn as a header with its children inside.
    Frame,
    /// Everything in a directory that was too small to draw.
    Rest,
}

#[derive(Clone, Copy, Debug)]
pub struct Tile {
    pub rect: Rect,
    /// The node shown. For a rest tile, the largest node folded into it.
    pub node: NodeId,
    /// The child of the laid-out directory this tile belongs to. This is what
    /// a click on the tile selects.
    pub top: NodeId,
    pub kind: TileKind,
    /// Nesting depth. Direct children of the laid-out directory are depth 0.
    pub depth: u8,
    /// Whether the tile leaves a gap column on its right.
    pub gap_r: bool,
    /// Whether the tile leaves a half-row gap at its bottom.
    pub gap_b: bool,
    /// Category to colour the tile by: a file's own, or for a directory drawn
    /// as one block, the category that makes up most of its contents.
    pub cat: Option<Cat>,
    /// For a rest tile: how many nodes it stands for, and their total size.
    pub rest_count: u32,
    pub rest_size: u64,
}

impl Tile {
    /// Width of the painted part of the tile.
    pub fn body_w(&self) -> u16 {
        self.rect.w - self.gap_r as u16
    }
    /// Number of rows that are painted in full.
    pub fn full_rows(&self) -> u16 {
        self.rect.h - self.gap_b as u16
    }
}

/// Lay out the children of `dir` inside `area`. Tiles come out parents first.
///
/// `cats` maps extension ids to categories, as returned by
/// [`crate::ext::categories`].
pub fn layout(
    tree: &Tree,
    dir: NodeId,
    area: Rect,
    metric: Metric,
    max_depth: u8,
    cats: &[Cat],
) -> Vec<Tile> {
    let mut tiles = Vec::new();
    if area.w == 0 || area.h == 0 {
        return tiles;
    }
    let ctx = Ctx {
        tree,
        metric,
        max_depth,
        cats,
    };
    ctx.layout_dir(dir, area, 0, None, false, &mut tiles);
    tiles
}

struct Ctx<'a> {
    tree: &'a Tree,
    metric: Metric,
    max_depth: u8,
    cats: &'a [Cat],
}

impl Ctx<'_> {
    fn cat_of(&self, ext: u16) -> Cat {
        self.cats.get(ext as usize).copied().unwrap_or(Cat::Other)
    }

    /// The category holding more than half of the bytes below `dir`, if any.
    /// A subtree is one contiguous slice of the arena, so this is a linear
    /// pass, and each node is visited at most once per layout.
    fn dominant(&self, dir: NodeId) -> Option<Cat> {
        let start = dir as usize + 1;
        let end = start + self.tree.node(dir).desc as usize;
        let mut sums = [0u64; Cat::COUNT];
        for n in &self.tree.nodes()[start..end] {
            if n.is_file() {
                sums[self.cat_of(n.ext) as usize] += n.get(self.metric);
            }
        }
        let total: u64 = sums.iter().sum();
        let (best, &most) = sums.iter().enumerate().max_by_key(|&(_, &s)| s)?;
        let cat = Cat::ALL[best];
        (cat != Cat::Other && most > total / 2).then_some(cat)
    }

    /// `keep_bottom_gap`: tiles on the bottom edge of the region still leave
    /// their half-row gap. Used inside frames that have no gap of their own.
    fn layout_dir(
        &self,
        dir: NodeId,
        region: Rect,
        depth: u8,
        top: Option<NodeId>,
        keep_bottom_gap: bool,
        out: &mut Vec<Tile>,
    ) {
        let tree = self.tree;
        let mut items: Vec<(u64, NodeId)> = tree
            .children(dir)
            .map(|c| (tree.node(c).get(self.metric), c))
            .filter(|&(s, _)| s > 0)
            .collect();
        if items.is_empty() {
            return;
        }
        let total: u64 = items.iter().map(|&(s, _)| s).sum();
        let by_size_desc = |a: &(u64, NodeId), b: &(u64, NodeId)| b.0.cmp(&a.0).then(a.1.cmp(&b.1));
        if items.len() > MAX_TILES {
            items.select_nth_unstable_by(MAX_TILES, by_size_desc);
            items[..MAX_TILES].sort_unstable_by(by_size_desc);
        } else {
            items.sort_unstable_by(by_size_desc);
        }

        // Children that are big enough to draw; the remainder becomes "rest".
        let cells = region.w as f64 * region.h as f64;
        let per_byte = cells / total as f64;
        let shown = items
            .iter()
            .take(MAX_TILES)
            .take_while(|&&(s, _)| s as f64 * per_byte >= MIN_TILE_CELLS)
            .count();
        let shown_size: u64 = items[..shown].iter().map(|&(s, _)| s).sum();
        let rest_size = total - shown_size;
        let rest_count = (items.len() - shown) as u32;
        let rest_node = if rest_count > 0 {
            // Largest node that did not get a tile. After the partial sort
            // above, the unsorted tail needs a scan to find it.
            items[shown..]
                .iter()
                .copied()
                .min_by(by_size_desc)
                .map(|(_, id)| id)
        } else {
            None
        };
        let show_rest = rest_count > 0 && (shown == 0 || rest_size as f64 * per_byte >= 2.0);

        // Areas in layout order: descending, with the rest tile slotted in by size.
        let mut entries: Vec<(u64, Option<NodeId>)> = items[..shown]
            .iter()
            .map(|&(s, id)| (s, Some(id)))
            .collect();
        if show_rest {
            let at = entries.partition_point(|&(s, _)| s >= rest_size);
            entries.insert(at, (rest_size, None));
        }
        let laid: u64 = entries.iter().map(|&(s, _)| s).sum();
        let fw = region.w as f64;
        let fh = region.h as f64 * ASPECT;
        let scale = fw * fh / laid as f64;
        let areas: Vec<f64> = entries.iter().map(|&(s, _)| s as f64 * scale).collect();
        let rects = squarify(&areas, fw, fh);

        for (&(_, node), &(fx0, fy0, fx1, fy1)) in entries.iter().zip(&rects) {
            let x0 = (fx0.round() as u16).min(region.w);
            let x1 = (fx1.round() as u16).min(region.w);
            let y0 = ((fy0 / ASPECT).round() as u16).min(region.h);
            let y1 = ((fy1 / ASPECT).round() as u16).min(region.h);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let rect = Rect::new(region.x + x0, region.y + y0, x1 - x0, y1 - y0);
            let gap_r = x1 < region.w;
            let gap_b = y1 < region.h || keep_bottom_gap;
            if rect.w - gap_r as u16 == 0 {
                continue;
            }
            let mut tile = Tile {
                rect,
                node: node.or(rest_node).unwrap_or(dir),
                top: top.or(node).or(rest_node).unwrap_or(dir),
                kind: TileKind::Leaf,
                depth,
                gap_r,
                gap_b,
                cat: None,
                rest_count: 0,
                rest_size: 0,
            };
            let Some(node) = node else {
                tile.kind = TileKind::Rest;
                tile.rest_count = rest_count;
                tile.rest_size = rest_size;
                out.push(tile);
                continue;
            };
            let n = tree.node(node);
            let body_w = tile.body_w();
            // Header row, then room for children that can carry a label. A
            // frame without its own bottom gap lends its last row to the
            // children's gaps instead.
            let content_h = rect.h.saturating_sub(1 + gap_b as u16);
            let frame = n.is_dir()
                && n.desc > 0
                && !n.has(flag::SKIPPED)
                && depth < self.max_depth
                && body_w >= MIN_FRAME_W
                && content_h >= 2;
            let at = out.len();
            if frame {
                tile.kind = TileKind::Frame;
                out.push(tile);
                let inner = Rect::new(rect.x + 1, rect.y + 1, body_w - 2, content_h);
                self.layout_dir(node, inner, depth + 1, Some(tile.top), !gap_b, out);
                if out[at + 1..].iter().any(|t| t.kind != TileKind::Rest) {
                    continue;
                }
                // Nothing inside was big enough to draw, so a frame would be
                // an empty box. Show the directory as one block instead.
                out.truncate(at);
                tile.kind = TileKind::Leaf;
            }
            tile.cat = if n.is_dir() {
                self.dominant(node)
            } else if n.is_file() {
                Some(self.cat_of(n.ext))
            } else {
                None
            };
            out.push(tile);
        }
    }
}

/// Squarified treemap (Bruls, Huizing, van Wijk) of `areas` in a `w` by `h`
/// rectangle at the origin. `areas` must be in descending order, or close to
/// it, and sum to `w * h`. Returns `(x0, y0, x1, y1)` per area.
fn squarify(areas: &[f64], w: f64, h: f64) -> Vec<(f64, f64, f64, f64)> {
    let mut out = Vec::with_capacity(areas.len());
    let (mut x, mut y, mut w, mut h) = (0.0f64, 0.0f64, w, h);
    let mut i = 0;
    while i < areas.len() {
        let short = w.min(h).max(1e-9);
        // Grow the strip while the worst aspect ratio in it keeps improving.
        let mut j = i;
        let mut sum = 0.0;
        let mut lo = f64::INFINITY;
        let mut hi = 0.0f64;
        let mut best = f64::INFINITY;
        while j < areas.len() {
            let a = areas[j].max(1e-12);
            let (nsum, nlo, nhi) = (sum + a, lo.min(a), hi.max(a));
            let s2 = short * short;
            let worst = (s2 * nhi / (nsum * nsum)).max(nsum * nsum / (s2 * nlo));
            if j > i && worst > best {
                break;
            }
            (sum, lo, hi, best) = (nsum, nlo, nhi, worst);
            j += 1;
        }
        // The last strip takes whatever is left, so rounding errors cannot
        // leave a sliver uncovered.
        let last_strip = j == areas.len();
        let thick = match (w >= h, last_strip) {
            (true, true) => w,
            (true, false) => (sum / h).min(w),
            (false, true) => h,
            (false, false) => (sum / w).min(h),
        };
        if w >= h {
            // Strip down the left edge, items stacked top to bottom.
            let mut cy = y;
            for (k, &a) in areas[i..j].iter().enumerate() {
                let end = if k == j - i - 1 {
                    y + h
                } else {
                    cy + a / sum * h
                };
                out.push((x, cy, x + thick, end));
                cy = end;
            }
            x += thick;
            w -= thick;
        } else {
            // Strip along the top edge, items side by side.
            let mut cx = x;
            for (k, &a) in areas[i..j].iter().enumerate() {
                let end = if k == j - i - 1 {
                    x + w
                } else {
                    cx + a / sum * w
                };
                out.push((cx, y, end, y + thick));
                cx = end;
            }
            y += thick;
            h -= thick;
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarify_fills_the_rectangle() {
        let raw = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let (w, h) = (6.0, 4.0);
        let total: f64 = raw.iter().sum();
        let areas: Vec<f64> = raw.iter().map(|a| a * w * h / total).collect();
        let rects = squarify(&areas, w, h);
        assert_eq!(rects.len(), areas.len());
        let mut covered = 0.0;
        for (r, a) in rects.iter().zip(&areas) {
            let (x0, y0, x1, y1) = *r;
            assert!(x0 >= -1e-9 && y0 >= -1e-9 && x1 <= w + 1e-9 && y1 <= h + 1e-9);
            let area = (x1 - x0) * (y1 - y0);
            assert!((area - a).abs() < 1e-6, "area {area} != {a}");
            covered += area;
        }
        assert!((covered - w * h).abs() < 1e-6);
    }

    /// Scan a generated directory tree with sizes spread over a few orders
    /// of magnitude, several levels deep.
    fn sample_tree(tag: &str) -> (Tree, std::path::PathBuf) {
        use std::fs;
        let root = crate::testutil::scratch(tag);
        let mut size = 7usize;
        for a in 0..6 {
            for b in 0..5 {
                let dir = root.join(format!("d{a}/sub{b}/leaf"));
                fs::create_dir_all(&dir).unwrap();
                for c in 0..(1 + (a * 7 + b * 3) % 9) {
                    size = (size * 37 + 11) % 90_000;
                    fs::write(dir.join(format!("f{c}.bin")), vec![0u8; size + 1]).unwrap();
                }
            }
            fs::write(
                root.join(format!("d{a}/top.dat")),
                vec![0u8; 3000 * (a + 1)],
            )
            .unwrap();
        }
        fs::create_dir_all(root.join("empty")).unwrap();
        let root = root.canonicalize().unwrap();
        let progress = crate::scan::Progress::default();
        let (meta, raw) = crate::scan::scan(&root, Default::default(), &progress).unwrap();
        (Tree::build(root.clone(), meta, raw).unwrap(), root)
    }

    fn inside(inner: Rect, outer: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.right() <= outer.right()
            && inner.bottom() <= outer.bottom()
    }

    fn overlap(a: Rect, b: Rect) -> bool {
        a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
    }

    #[test]
    fn tiles_stay_inside_their_parents_at_every_size() {
        let (tree, root) = sample_tree("bounds");
        let cats = crate::ext::categories();
        let mut checked = 0usize;
        for metric in [Metric::Disk, Metric::Apparent] {
            for depth in [0u8, 1, 3, 8] {
                for w in (1..=140u16).step_by(3).chain([1, 2, 9, 10, 11]) {
                    for h in (1..=45u16).step_by(2).chain([2, 3, 4]) {
                        let area = Rect::new(3, 5, w, h);
                        let tiles = layout(&tree, Tree::ROOT, area, metric, depth, &cats);
                        // Enclosing frame per depth, as tiles come parents first.
                        let mut frames: Vec<Rect> = Vec::new();
                        let mut siblings: Vec<Vec<Rect>> = vec![Vec::new()];
                        for t in &tiles {
                            let d = t.depth as usize;
                            assert!(d <= depth as usize, "depth {d} beyond the limit {depth}");
                            frames.truncate(d);
                            siblings.truncate(d + 1);
                            let parent = frames.last().copied().unwrap_or(area);
                            let ctx = format!("{w}x{h} depth {depth}: {t:?} in {parent:?}");
                            assert!(t.rect.w > 0 && t.rect.h > 0, "empty tile, {ctx}");
                            assert!(t.body_w() > 0, "nothing to paint, {ctx}");
                            assert!(inside(t.rect, parent), "outside its parent, {ctx}");
                            for other in &siblings[d] {
                                assert!(!overlap(t.rect, *other), "overlaps {other:?}, {ctx}");
                            }
                            siblings[d].push(t.rect);
                            if t.kind == TileKind::Frame {
                                frames.push(t.rect);
                                siblings.push(Vec::new());
                            }
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert!(checked > 10_000, "only {checked} tiles checked");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn top_level_tiles_account_for_every_child() {
        let (tree, root) = sample_tree("account");
        let cats = crate::ext::categories();
        let tiles = layout(
            &tree,
            Tree::ROOT,
            Rect::new(0, 0, 120, 40),
            Metric::Disk,
            4,
            &cats,
        );
        let top: Vec<&Tile> = tiles.iter().filter(|t| t.depth == 0).collect();
        let shown = top.iter().filter(|t| t.kind != TileKind::Rest).count();
        let rest: u32 = top.iter().map(|t| t.rest_count).sum();
        let with_size = tree
            .children(Tree::ROOT)
            .filter(|&c| tree.node(c).disk > 0)
            .count();
        assert_eq!(shown + rest as usize, with_size);
        // Every tile points back at a real child of the directory.
        for t in &tiles {
            assert_eq!(tree.node(t.top).parent, Tree::ROOT, "{t:?}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn squarify_handles_degenerate_input() {
        assert!(squarify(&[], 10.0, 10.0).is_empty());
        let one = squarify(&[100.0], 10.0, 10.0);
        assert_eq!(one, vec![(0.0, 0.0, 10.0, 10.0)]);
    }
}
