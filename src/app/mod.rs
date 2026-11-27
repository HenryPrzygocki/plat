// SPDX-License-Identifier: GPL-3.0-or-later
//! Application state and input handling. Nothing in here draws.

mod delete;
mod input;
#[cfg(test)]
mod tests;

pub use delete::{Choice, Confirm, Deleting, Report};
// Named outside this module only by the interface tests.
#[cfg(test)]
pub use delete::{DelItem, Warning};

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use rustc_hash::FxHashSet;

use crate::ext;
use crate::fmt;
use crate::fsinfo;
use crate::guard::{Guard, Level};
use crate::ops::{DeleteJob, Method, Outcome, TrashTool};
use crate::scan::{self, Progress, RawDir, RootMeta};
use crate::theme::Theme;
use crate::tree::{Metric, NodeId, Tree, flag};
use crate::treemap::{self, Rect, Tile, TileKind};

/// "No node": an empty directory has no cursor.
pub const NONE: NodeId = NodeId::MAX;
/// How many files the "Largest" tab lists.
const LARGEST_COUNT: usize = 500;
const MAX_DEPTH: u8 = 8;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const TOAST_TIME: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pane {
    Map,
    Side,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Files,
    Largest,
    Types,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Files, Tab::Largest, Tab::Types];
    pub fn label(self) -> &'static str {
        match self {
            Tab::Files => "Files",
            Tab::Largest => "Largest",
            Tab::Types => "Types",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    Size,
    Name,
    Modified,
    Items,
}

impl Sort {
    fn next(self) -> Sort {
        match self {
            Sort::Size => Sort::Name,
            Sort::Name => Sort::Modified,
            Sort::Modified => Sort::Items,
            Sort::Items => Sort::Size,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToastKind {
    Info,
    Ok,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub until: Instant,
}

/// One row of the "Types" tab.
#[derive(Clone, Copy, Debug)]
pub struct TypeRow {
    pub ext: u16,
    pub size: u64,
    pub count: u64,
}

pub enum Modal {
    Help,
    Confirm(Confirm),
    Deleting(Deleting),
    Report(Report),
}

/// Something the event loop has to do on the app's behalf, because it needs
/// the terminal.
pub enum Effect {
    None,
    Quit,
    /// Suspend the interface and run a shell in this directory.
    Shell(PathBuf),
    /// Put this text on the clipboard.
    Copy(String),
}

/// A scan running on a background thread.
pub struct ScanJob {
    pub progress: Arc<Progress>,
    pub started: Instant,
    /// Directory being scanned.
    pub path: PathBuf,
    /// `None` for the initial scan, otherwise the directory being refreshed.
    refresh: Option<PathBuf>,
    rx: mpsc::Receiver<io::Result<(RootMeta, RawDir)>>,
}

/// Screen regions from the last draw, for mouse hit testing.
#[derive(Default)]
pub struct Hit {
    pub map: Rect,
    pub list: Rect,
    pub tabs: Vec<(Rect, Tab)>,
    pub crumbs: Vec<(Rect, NodeId)>,
}

/// What the treemap layout was computed for.
#[derive(Clone, Copy, PartialEq, Eq)]
struct LayoutKey {
    view: NodeId,
    area: Rect,
    metric: Metric,
    depth: u8,
    generation: u64,
}

pub struct App {
    pub theme: Theme,
    pub root: PathBuf,
    pub opts: scan::Options,
    pub is_root: bool,
    /// Decides which deletions deserve a warning or a typed confirmation.
    pub guard: Guard,
    pub read_only: bool,
    /// False in the headless mode unless the root is a throwaway directory,
    /// so a test script can never delete real data.
    pub allow_delete: bool,
    /// Directories to rescan once nothing else is running.
    pending_rescans: Vec<PathBuf>,
    /// Filesystem capacity: (total, available).
    pub capacity: Option<(u64, u64)>,

    pub tree: Option<Tree>,
    pub scan: Option<ScanJob>,
    /// How long the last full scan took.
    pub scan_time: Duration,
    /// Unreadable directories in the last full scan.
    pub scan_errors: u64,
    /// Why there is nothing to show, once the initial scan has failed.
    fatal: Option<String>,
    /// Bumped whenever the tree or anything derived from it changes.
    generation: u64,

    /// Directory on display.
    pub view: NodeId,
    /// Selected child of `view`, or `NONE`.
    pub cursor: NodeId,
    pub focus: Pane,
    pub tab: Tab,
    pub sort: Sort,
    pub metric: Metric,
    /// How many levels the treemap nests.
    pub depth: u8,

    /// Children of `view` in display order.
    pub rows: Vec<NodeId>,
    /// Sum and maximum of the row sizes, under the current metric.
    pub rows_total: u64,
    pub rows_largest: u64,
    /// Largest files below `view`.
    pub largest: Vec<NodeId>,
    pub largest_sel: usize,
    largest_for: Option<(NodeId, u64)>,
    /// Extension breakdown below `view`.
    pub types: Vec<TypeRow>,
    pub types_sel: usize,
    types_for: Option<(NodeId, u64)>,
    /// First visible row per tab.
    pub scroll: [usize; 3],

    pub marks: FxHashSet<NodeId>,
    pub marked_size: u64,

    pub tiles: Vec<Tile>,
    layout_key: Option<LayoutKey>,
    /// Remembered position for moving through the map with the keyboard.
    anchor: Option<(u16, u16)>,

    pub modal: Option<Modal>,
    pub toast: Option<Toast>,
    pub hit: Hit,
    last_click: Option<(Instant, u16, u16)>,
    /// Frame counter for animations.
    pub tick: u64,
}

impl App {
    pub fn new(root: PathBuf, opts: scan::Options, theme: Theme) -> App {
        App {
            theme,
            capacity: fsinfo::capacity(&root),
            root,
            opts,
            is_root: fsinfo::is_root(),
            guard: Guard::system(),
            read_only: false,
            allow_delete: true,
            pending_rescans: Vec::new(),
            tree: None,
            scan: None,
            scan_time: Duration::ZERO,
            scan_errors: 0,
            fatal: None,
            generation: 0,
            view: Tree::ROOT,
            cursor: NONE,
            focus: Pane::Map,
            tab: Tab::Files,
            sort: Sort::Size,
            metric: Metric::Disk,
            depth: 5,
            rows: Vec::new(),
            rows_total: 0,
            rows_largest: 0,
            largest: Vec::new(),
            largest_sel: 0,
            largest_for: None,
            types: Vec::new(),
            types_sel: 0,
            types_for: None,
            scroll: [0; 3],
            marks: FxHashSet::default(),
            marked_size: 0,
            tiles: Vec::new(),
            layout_key: None,
            anchor: None,
            modal: None,
            toast: None,
            hit: Hit::default(),
            last_click: None,
            tick: 0,
        }
    }

    // ---------------------------------------------------------------- scanning

    /// Start scanning in the background. `refresh` names a directory inside
    /// the current tree to rescan; `None` scans everything from the root.
    pub fn start_scan(&mut self, refresh: Option<PathBuf>) {
        if self.scan.is_some() {
            return;
        }
        let path = refresh.clone().unwrap_or_else(|| self.root.clone());
        let progress = Arc::new(Progress::default());
        let (tx, rx) = mpsc::channel();
        let (p, o, pr) = (path.clone(), self.opts, progress.clone());
        let spawned = std::thread::Builder::new()
            .name("plat-scan".into())
            .stack_size(64 << 20)
            .spawn(move || {
                let _ = tx.send(scan::scan(&p, o, &pr));
            });
        if spawned.is_err() {
            self.error("could not start a scanner thread");
            return;
        }
        self.scan = Some(ScanJob {
            progress,
            started: Instant::now(),
            path,
            refresh,
            rx,
        });
    }

    /// Scan on the calling thread. Used by the non-interactive modes.
    pub fn scan_blocking(&mut self) -> io::Result<()> {
        let progress = Progress::default();
        let started = Instant::now();
        let result = scan::scan(&self.root, self.opts, &progress)?;
        self.scan_time = started.elapsed();
        self.scan_errors = progress.errors.load(Relaxed);
        self.install(result);
        Ok(())
    }

    /// Pick up results from background work. Returns true if anything changed.
    pub fn poll_jobs(&mut self) -> bool {
        let mut changed = false;
        if let Some(Modal::Deleting(d)) = &self.modal
            && let Some(outcomes) = d.job.try_finish()
        {
            let Some(Modal::Deleting(d)) = self.modal.take() else {
                unreachable!()
            };
            let removed = d.job.counters.removed.load(Relaxed);
            self.finish_delete(d.items, outcomes, d.method, removed);
            changed = true;
        }
        if self.scan.is_none()
            && !matches!(self.modal, Some(Modal::Deleting(_)))
            && let Some(path) = self.pending_rescans.pop()
        {
            self.start_scan(Some(path));
            changed = true;
        }
        changed | self.poll_scan()
    }

    fn poll_scan(&mut self) -> bool {
        let Some(job) = &self.scan else {
            return false;
        };
        let result = match job.rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(io::Error::other("the scanner thread died"))
            }
        };
        let job = self.scan.take().unwrap();
        match (result, job.refresh) {
            (Ok(r), None) => {
                self.scan_time = job.started.elapsed();
                self.scan_errors = job.progress.errors.load(Relaxed);
                self.install(r);
            }
            (Ok((meta, raw)), Some(path)) => self.install_refresh(&path, meta, raw),
            (Err(e), None) => self.error(format!("{}: {e}", job.path.display())),
            (Err(e), Some(path)) => {
                self.toast(format!("{}: {e}", path.display()), ToastKind::Error);
            }
        }
        true
    }

    /// True while background work is running and the screen should animate.
    pub fn busy(&self) -> bool {
        self.scan.is_some()
            || !self.pending_rescans.is_empty()
            || matches!(self.modal, Some(Modal::Deleting(_)))
    }

    /// Block until background work has finished. For non-interactive use.
    pub fn settle(&mut self) {
        while self.busy() {
            if !self.poll_jobs() {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    /// The initial scan failed and there is nothing to show.
    pub fn failed(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    /// Report an error: on the status line, or, while there is no tree to
    /// show at all, as the reason the session ends.
    fn error(&mut self, text: impl Into<String>) {
        if self.tree.is_none() {
            self.fatal = Some(text.into());
        } else {
            self.toast(text, ToastKind::Error);
        }
    }

    fn install(&mut self, (meta, raw): (RootMeta, RawDir)) {
        match Tree::build(self.root.clone(), meta, raw) {
            Ok(tree) => {
                self.tree = Some(tree);
                self.marks.clear();
                self.view = Tree::ROOT;
                self.cursor = NONE;
                self.changed();
            }
            Err(_) => self.error("too many files for one scan"),
        }
        scan::release_memory();
    }

    fn install_refresh(&mut self, path: &std::path::Path, meta: RootMeta, raw: RawDir) {
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        let Some(id) = tree.find(path) else {
            self.toast("that directory is no longer in the tree", ToastKind::Error);
            return;
        };
        // Ids below the refreshed directory are about to change, so carry
        // everything that points there across by path.
        let view_path = tree.path(self.view);
        let cursor_path = (self.cursor != NONE).then(|| tree.path(self.cursor));
        let inside: Vec<PathBuf> = self
            .marks
            .iter()
            .filter(|&&m| tree.is_below(m, id))
            .map(|&m| tree.path(m))
            .collect();
        let Ok(remap) = tree.replace(id, meta, raw) else {
            self.toast("too many files for one scan", ToastKind::Error);
            return;
        };
        self.marks = self.marks.iter().filter_map(|&m| remap.map(m)).collect();
        for p in inside {
            if let Some(m) = tree.find(&p) {
                self.marks.insert(m);
            }
        }
        // The view may itself have vanished; fall back to the nearest
        // ancestor that still exists.
        let mut vp = view_path.as_path();
        self.view = loop {
            if let Some(v) = tree.find(vp).filter(|&v| tree.node(v).is_dir()) {
                break v;
            }
            match vp.parent() {
                Some(p) => vp = p,
                None => break Tree::ROOT,
            }
        };
        self.cursor = cursor_path.and_then(|p| tree.find(&p)).unwrap_or(NONE);
        self.changed();
        scan::release_memory();
        self.toast("rescanned", ToastKind::Ok);
    }

    /// Recompute everything derived from the tree, the view or the settings.
    fn changed(&mut self) {
        self.generation += 1;
        self.rebuild_rows();
        self.recount_marks();
    }

    // ------------------------------------------------------------ derived data

    fn rebuild_rows(&mut self) {
        let Some(tree) = &self.tree else {
            self.rows.clear();
            (self.rows_total, self.rows_largest) = (0, 0);
            return;
        };
        let metric = self.metric;
        let mut rows: Vec<NodeId> = tree.children(self.view).collect();
        match self.sort {
            Sort::Size => rows.sort_by_key(|&c| (Reverse(tree.node(c).get(metric)), c)),
            Sort::Name => rows.sort_by(|&a, &b| {
                let (na, nb) = (tree.node(a), tree.node(b));
                nb.is_dir()
                    .cmp(&na.is_dir())
                    .then_with(|| cmp_names(tree.name(a), tree.name(b)))
            }),
            Sort::Modified => rows.sort_by_key(|&c| (Reverse(tree.node(c).mtime), c)),
            Sort::Items => rows.sort_by_key(|&c| (Reverse(tree.node(c).desc), c)),
        }
        // The gauge in the header is about the filesystem being looked at,
        // which changes when the view crosses a mount.
        self.capacity = fsinfo::capacity(&tree.path(self.view)).or(self.capacity);
        self.rows_total = rows.iter().map(|&r| tree.node(r).get(metric)).sum();
        self.rows_largest = rows
            .iter()
            .map(|&r| tree.node(r).get(metric))
            .max()
            .unwrap_or(0);
        self.rows = rows;
        if !self.rows.contains(&self.cursor) {
            self.cursor = self.rows.first().copied().unwrap_or(NONE);
        }
        self.anchor = None;
    }

    /// Make sure the data behind the visible tab is current.
    pub fn prepare(&mut self) {
        let Some(tree) = &self.tree else {
            return;
        };
        let key = (self.view, self.generation);
        match self.tab {
            Tab::Files => {}
            Tab::Largest if self.largest_for != Some(key) => {
                let metric = self.metric;
                let start = self.view as usize + 1;
                let end = start + tree.node(self.view).desc as usize;
                let mut heap: BinaryHeap<Reverse<(u64, Reverse<NodeId>)>> =
                    BinaryHeap::with_capacity(LARGEST_COUNT + 1);
                for (i, n) in tree.nodes()[start..end].iter().enumerate() {
                    if !n.is_file() {
                        continue;
                    }
                    let size = n.get(metric);
                    if heap.len() == LARGEST_COUNT {
                        if heap.peek().is_some_and(|m| m.0.0 >= size) {
                            continue;
                        }
                        heap.pop();
                    }
                    heap.push(Reverse((size, Reverse((start + i) as NodeId))));
                }
                let mut top: Vec<(u64, Reverse<NodeId>)> = heap.into_iter().map(|r| r.0).collect();
                top.sort_unstable_by(|a, b| b.cmp(a));
                self.largest = top.into_iter().map(|(_, id)| id.0).collect();
                self.largest_sel = self.largest_sel.min(self.largest.len().saturating_sub(1));
                self.largest_for = Some(key);
            }
            Tab::Types if self.types_for != Some(key) => {
                let metric = self.metric;
                let start = self.view as usize + 1;
                let end = start + tree.node(self.view).desc as usize;
                let mut acc: Vec<(u64, u64)> = Vec::new();
                for n in &tree.nodes()[start..end] {
                    if !n.is_file() {
                        continue;
                    }
                    let e = n.ext as usize;
                    if e >= acc.len() {
                        acc.resize(e + 1, (0, 0));
                    }
                    acc[e].0 += n.get(metric);
                    acc[e].1 += 1;
                }
                let mut types: Vec<TypeRow> = acc
                    .iter()
                    .enumerate()
                    .filter(|&(_, &(_, count))| count > 0)
                    .map(|(ext, &(size, count))| TypeRow {
                        ext: ext as u16,
                        size,
                        count,
                    })
                    .collect();
                types.sort_unstable_by_key(|t| (Reverse(t.size), t.ext));
                self.types = types;
                self.types_sel = self.types_sel.min(self.types.len().saturating_sub(1));
                self.types_for = Some(key);
            }
            _ => {}
        }
    }

    /// Compute the treemap for `area` unless the cached one still fits.
    pub fn ensure_layout(&mut self, area: Rect) {
        let Some(tree) = &self.tree else {
            return;
        };
        let key = LayoutKey {
            view: self.view,
            area,
            metric: self.metric,
            depth: self.depth,
            generation: self.generation,
        };
        if self.layout_key != Some(key) {
            let cats = ext::categories();
            self.tiles = treemap::layout(tree, self.view, area, self.metric, self.depth, &cats);
            self.layout_key = Some(key);
            self.anchor = None;
        }
    }

    fn recount_marks(&mut self) {
        let Some(tree) = &self.tree else {
            self.marked_size = 0;
            return;
        };
        self.marked_size = self.marks.iter().map(|&m| tree.node(m).disk).sum();
    }

    // ----------------------------------------------------------------- queries

    /// Index of the cursor within `rows`.
    pub fn cursor_row(&self) -> Option<usize> {
        self.rows.iter().position(|&r| r == self.cursor)
    }

    /// The node that actions such as mark, delete or open apply to.
    pub fn active(&self) -> Option<NodeId> {
        if self.focus == Pane::Side && self.tab == Tab::Largest {
            return self.largest.get(self.largest_sel).copied();
        }
        if self.focus == Pane::Side && self.tab == Tab::Types {
            return None;
        }
        (self.cursor != NONE).then_some(self.cursor)
    }

    /// Extension to spotlight in the map, when the Types tab is being browsed.
    pub fn spotlight(&self) -> Option<u16> {
        (self.focus == Pane::Side && self.tab == Tab::Types)
            .then(|| self.types.get(self.types_sel).map(|t| t.ext))
            .flatten()
    }

    /// Whether a node is marked, either itself or through a marked ancestor.
    pub fn is_marked(&self, id: NodeId) -> bool {
        if self.marks.is_empty() {
            return false;
        }
        let Some(tree) = &self.tree else {
            return false;
        };
        let mut cur = id;
        while cur != NodeId::MAX {
            if self.marks.contains(&cur) {
                return true;
            }
            cur = tree.node(cur).parent;
        }
        false
    }

    pub fn toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast {
            text: text.into(),
            kind,
            until: Instant::now() + TOAST_TIME,
        });
    }

    /// Drop the toast once it has expired. Returns true if it was dropped.
    pub fn expire_toast(&mut self) -> bool {
        if self
            .toast
            .as_ref()
            .is_some_and(|t| Instant::now() >= t.until)
        {
            self.toast = None;
            return true;
        }
        false
    }

    // -------------------------------------------------------------- navigation

    fn enter(&mut self, id: NodeId) {
        let Some(tree) = &self.tree else {
            return;
        };
        let n = tree.node(id);
        if !n.is_dir() {
            return;
        }
        if n.has(flag::SKIPPED) {
            let fs = fsinfo::fs_name(n.ext);
            let text = if n.ext == fsinfo::DUPLICATE {
                "the same files are already counted at another place in this scan".to_string()
            } else if self.opts.one_fs {
                format!("{fs} mount, not scanned (plat was started with --one-file-system)")
            } else {
                format!("{fs} mount, not scanned (start with --all-mounts to include it)")
            };
            self.toast(text, ToastKind::Info);
            return;
        }
        if n.has(flag::ERR) && n.desc == 0 {
            let hint = if self.is_root {
                ""
            } else {
                " (try running with sudo)"
            };
            self.toast(
                format!("could not read this directory{hint}"),
                ToastKind::Error,
            );
            return;
        }
        self.view = id;
        self.cursor = NONE;
        self.scroll[Tab::Files as usize] = 0;
        self.rebuild_rows();
    }

    fn leave(&mut self) {
        let Some(tree) = &self.tree else {
            return;
        };
        if self.view == Tree::ROOT {
            return;
        }
        let from = self.view;
        self.view = tree.node(from).parent;
        self.cursor = from;
        self.rebuild_rows();
    }

    /// Show `id` in its parent directory with the cursor on it.
    fn reveal(&mut self, id: NodeId) {
        let Some(tree) = &self.tree else {
            return;
        };
        if id == Tree::ROOT {
            return;
        }
        self.view = tree.node(id).parent;
        self.cursor = id;
        self.tab = Tab::Files;
        self.rebuild_rows();
    }

    fn goto(&mut self, dir: NodeId) {
        if dir == self.view {
            return;
        }
        // Keep the cursor on the branch we came from when moving up.
        let mut child = self.view;
        if let Some(tree) = &self.tree {
            while child != Tree::ROOT && tree.node(child).parent != dir {
                child = tree.node(child).parent;
            }
        }
        self.view = dir;
        self.cursor = if child == Tree::ROOT { NONE } else { child };
        self.rebuild_rows();
    }

    fn move_rows(&mut self, delta: isize) {
        let len = match self.tab {
            Tab::Files => self.rows.len(),
            Tab::Largest => self.largest.len(),
            Tab::Types => self.types.len(),
        };
        if len == 0 {
            return;
        }
        let step = |cur: usize| (cur as isize + delta).clamp(0, len as isize - 1) as usize;
        match self.tab {
            Tab::Files => {
                let cur = self.cursor_row().unwrap_or(0);
                self.cursor = self.rows[step(cur)];
                self.anchor = None;
            }
            Tab::Largest => self.largest_sel = step(self.largest_sel),
            Tab::Types => self.types_sel = step(self.types_sel),
        }
    }

    /// The top-level tile the cursor is on.
    fn cursor_tile(&self) -> Option<&Tile> {
        self.tiles
            .iter()
            .find(|t| t.depth == 0 && t.kind != TileKind::Rest && t.top == self.cursor)
            .or_else(|| {
                // No tile of its own: the cursor is one of the nodes folded
                // into the rest tile.
                self.tiles
                    .iter()
                    .find(|t| t.depth == 0 && t.kind == TileKind::Rest)
            })
    }

    fn tile_at(&self, x: u16, y: u16) -> Option<&Tile> {
        self.tiles
            .iter()
            .find(|t| t.depth == 0 && t.rect.contains(x, y))
    }

    /// Move the cursor to the neighbouring tile in the given direction.
    fn move_map(&mut self, dx: i32, dy: i32) {
        let Some(cur) = self.cursor_tile().map(|t| t.rect) else {
            if let Some(&first) = self.rows.first() {
                self.cursor = first;
            }
            return;
        };
        let centre = (cur.x + cur.w / 2, cur.y + cur.h / 2);
        let (ax, ay) = match self.anchor {
            Some((x, y)) if cur.contains(x, y) => (x, y),
            _ => centre,
        };
        // Probe just past the edge we are leaving through. A few extra steps
        // cover slivers where a tile was too thin to draw.
        for step in 0..4 {
            let px = match dx {
                1 => cur.right() as i32 + step,
                -1 => cur.x as i32 - 1 - step,
                _ => ax as i32,
            };
            let py = match dy {
                1 => cur.bottom() as i32 + step,
                -1 => cur.y as i32 - 1 - step,
                _ => ay as i32,
            };
            if px < 0 || py < 0 {
                return;
            }
            if let Some(t) = self.tile_at(px as u16, py as u16) {
                let r = t.rect;
                let target = if t.kind == TileKind::Rest {
                    t.node
                } else {
                    t.top
                };
                self.cursor = target;
                self.anchor = Some((
                    if dx != 0 { r.x + r.w / 2 } else { ax },
                    if dy != 0 { r.y + r.h / 2 } else { ay },
                ));
                return;
            }
        }
    }

    // ------------------------------------------------------------------- marks

    fn toggle_mark(&mut self, id: NodeId) {
        let Some(tree) = &self.tree else {
            return;
        };
        if self.marks.remove(&id) {
            self.recount_marks();
            return;
        }
        if self.is_marked(id) {
            self.toast("already inside a marked directory", ToastKind::Info);
            return;
        }
        if tree.node(id).has(flag::MOUNT) {
            self.toast(
                "mount points cannot be marked for deletion",
                ToastKind::Info,
            );
            return;
        }
        // Marking a directory covers everything below it.
        self.marks.retain(|&m| !tree.is_below(m, id));
        self.marks.insert(id);
        // Say what it is at the moment it is picked, not only at the end.
        let notice = self.risk_of(id).map(|risk| {
            let name = fmt::display_name(tree.name(id));
            match risk.level {
                Level::Critical => format!("{name} is protected: {}", risk.reason),
                Level::Caution => format!("{name}: {}", risk.reason),
            }
        });
        self.recount_marks();
        if let Some(text) = notice {
            self.toast(text, ToastKind::Info);
        }
    }

    // ------------------------------------------------------- other operations

    /// Hand the path of the active node to the clipboard.
    fn copy_path(&mut self) -> Effect {
        let (Some(tree), Some(id)) = (&self.tree, self.active()) else {
            return Effect::None;
        };
        let path = tree.path(id).display().to_string();
        self.toast(format!("copied {path}"), ToastKind::Ok);
        Effect::Copy(path)
    }

    /// Open the active node with the desktop's default application.
    fn open_external(&mut self) {
        let (Some(tree), Some(id)) = (&self.tree, self.active()) else {
            return;
        };
        if self.is_root {
            self.toast("not opening desktop applications as root", ToastKind::Info);
            return;
        }
        let path = tree.path(id);
        let spawned = std::process::Command::new("xdg-open")
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            Ok(_) => self.toast(format!("opened {}", path.display()), ToastKind::Ok),
            Err(e) => self.toast(format!("xdg-open: {e}"), ToastKind::Error),
        }
    }

    /// The directory a shell should start in: the one on view.
    fn shell_dir(&self) -> Option<PathBuf> {
        let tree = self.tree.as_ref()?;
        Some(tree.path(self.view))
    }

    /// Called by the event loop after the shell has exited.
    pub fn after_shell(&mut self) {
        // Whatever was done in there, show the result.
        self.rescan();
    }
}

/// Case-insensitive name order with a case-sensitive tie break. Called
/// n log n times per sort, so it compares in place instead of allocating
/// folded copies.
fn cmp_names(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.iter()
        .map(u8::to_ascii_lowercase)
        .cmp(b.iter().map(u8::to_ascii_lowercase))
        .then_with(|| a.cmp(b))
}
