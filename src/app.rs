// SPDX-License-Identifier: GPL-3.0-or-later
//! Application state and input handling. Nothing in here draws.

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

/// One entry of a pending deletion. The path is fixed when the dialog opens;
/// that exact path is what gets deleted, whatever happens to node ids.
pub struct DelItem {
    pub id: NodeId,
    pub path: PathBuf,
    pub size: u64,
    /// Files and directories this removes, itself included.
    pub entries: u64,
    pub is_dir: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Cancel,
    Trash,
    Delete,
}

/// Something the user should know before confirming a deletion.
pub struct Warning {
    /// What it is about: a path, or the selection as a whole.
    pub subject: String,
    pub level: Level,
    pub reason: String,
}

/// Words for the typed confirmation: short, lowercase, hard to mistype, and
/// picked at random so that typing one never becomes a habit.
const UNLOCK_WORDS: &[&str] = &[
    "anchor", "badger", "bramble", "canyon", "cedar", "cinder", "cobalt", "comet", "copper",
    "ember", "falcon", "fennel", "garnet", "ginger", "harbor", "hazel", "juniper", "kettle",
    "lantern", "linden", "mantle", "marble", "meadow", "nettle", "orchid", "osprey", "pebble",
    "pillar", "quartz", "quiver", "russet", "saddle", "sorrel", "thistle", "timber", "tundra",
    "umber", "velvet", "vessel", "walnut", "wicker", "willow", "yarrow", "zephyr",
];

fn random_word() -> &'static str {
    use std::hash::{BuildHasher, Hasher};
    // A fresh `RandomState` is seeded from the operating system.
    let n = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    UNLOCK_WORDS[(n % UNLOCK_WORDS.len() as u64) as usize]
}

pub struct Confirm {
    pub items: Vec<DelItem>,
    pub size: u64,
    pub entries: u64,
    pub choice: Choice,
    pub scroll: usize,
    /// How to reach the trash, when that is on offer.
    pub trash: Option<TrashTool>,
    /// What is at risk, most serious first.
    pub warnings: Vec<Warning>,
    /// The word that has to be typed before anything critical is touched.
    pub word: Option<&'static str>,
    /// What has been typed so far.
    pub typed: String,
}

impl Confirm {
    /// The buttons on offer, left to right.
    pub fn choices(&self) -> Vec<Choice> {
        let mut v = vec![Choice::Cancel];
        if self.trash.is_some() {
            v.push(Choice::Trash);
        }
        v.push(Choice::Delete);
        v
    }

    /// True while a typed confirmation is required and not yet given. Both
    /// destructive choices are out of reach until it is.
    pub fn locked(&self) -> bool {
        self.word
            .is_some_and(|word| !self.typed.trim().eq_ignore_ascii_case(word))
    }

    /// The most serious warning level present.
    pub fn level(&self) -> Option<Level> {
        self.warnings.iter().map(|w| w.level).max()
    }

    fn step(&mut self, forward: bool) {
        if self.locked() {
            self.choice = Choice::Cancel;
            return;
        }
        let all = self.choices();
        let at = all.iter().position(|&c| c == self.choice).unwrap_or(0);
        let next = if forward { at + 1 } else { at + all.len() - 1 } % all.len();
        self.choice = all[next];
    }
}

/// A deletion in progress, shown as a modal.
pub struct Deleting {
    pub items: Vec<DelItem>,
    pub entries: u64,
    pub method: Method,
    pub job: DeleteJob,
}

pub struct Report {
    pub title: &'static str,
    pub summary: String,
    /// (path, reason) of everything that could not be deleted.
    pub failures: Vec<(String, String)>,
    pub scroll: usize,
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
            self.toast("could not start a scanner thread", ToastKind::Error);
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
            (Err(e), None) => {
                // Nothing to show without a tree; the event loop reports this.
                self.toast(format!("{}: {e}", job.path.display()), ToastKind::Error);
            }
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
        if self.tree.is_none() && self.scan.is_none() {
            self.toast.as_ref().map(|t| t.text.as_str())
        } else {
            None
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
            Err(_) => self.toast("too many files for one scan", ToastKind::Error),
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

    /// What deleting a node would put at risk, if anything.
    pub fn risk_of(&self, id: NodeId) -> Option<crate::guard::Risk> {
        let tree = self.tree.as_ref()?;
        if id == Tree::ROOT {
            return None;
        }
        let n = tree.node(id);
        let has_git = n.is_dir() && tree.children(id).any(|c| tree.name(c) == b".git");
        self.guard.assess(&tree.path(id), n.is_dir(), has_git)
    }

    /// Whether deleting a node would take a typed confirmation. Cheap enough
    /// to ask for every row on every frame: unlike [`App::risk_of`] it does
    /// not look inside the directory, which only matters for cautions.
    pub fn is_protected(&self, id: NodeId) -> bool {
        let Some(tree) = &self.tree else {
            return false;
        };
        if id == Tree::ROOT {
            return false;
        }
        self.guard
            .assess(&tree.path(id), tree.node(id).is_dir(), false)
            .is_some_and(|r| r.level == Level::Critical)
    }

    // ---------------------------------------------------------------- deletion

    /// Open the confirmation dialog for the marked items, or for the item
    /// under the cursor if nothing is marked.
    fn open_delete(&mut self) {
        let Some(tree) = &self.tree else {
            return;
        };
        if self.read_only {
            self.toast("read-only mode: nothing can be deleted", ToastKind::Info);
            return;
        }
        if self.scan.is_some() {
            self.toast("wait for the scan to finish", ToastKind::Info);
            return;
        }
        let mut ids: Vec<NodeId> = if self.marks.is_empty() {
            self.active().into_iter().collect()
        } else {
            self.marks.iter().copied().collect()
        };
        ids.retain(|&id| id != Tree::ROOT);
        if let Some(&id) = ids.iter().find(|&&id| tree.node(id).has(flag::MOUNT)) {
            let name = fmt::display_name(tree.name(id));
            self.toast(
                format!("{name} is a mount point and cannot be deleted"),
                ToastKind::Info,
            );
            return;
        }
        if ids.is_empty() {
            self.toast("nothing selected", ToastKind::Info);
            return;
        }
        let mut items: Vec<DelItem> = ids
            .into_iter()
            .map(|id| {
                let n = tree.node(id);
                DelItem {
                    id,
                    path: tree.path(id),
                    size: n.disk,
                    entries: n.desc as u64 + 1,
                    is_dir: n.is_dir(),
                }
            })
            .collect();
        items.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
        let size: u64 = items.iter().map(|i| i.size).sum();

        // What is at risk. Critical findings come first so they are never
        // the ones that scroll out of sight.
        let mut warnings: Vec<Warning> = items
            .iter()
            .filter_map(|item| {
                let risk = self.risk_of(item.id)?;
                Some(Warning {
                    subject: item.path.display().to_string(),
                    level: risk.level,
                    reason: risk.reason,
                })
            })
            .collect();
        // Measured against the filesystem the largest item is on, which is
        // not necessarily the one on view when marks were gathered elsewhere.
        let home_fs = items
            .first()
            .and_then(|item| item.path.parent())
            .and_then(fsinfo::capacity);
        if let Some((total, avail)) = home_fs {
            let used = total.saturating_sub(avail);
            if size >= 1 << 30 && size.saturating_mul(4) >= used {
                let share = (size as f64 / used.max(1) as f64 * 100.0).min(100.0);
                warnings.push(Warning {
                    subject: "The selection as a whole".to_string(),
                    level: Level::Caution,
                    reason: format!("It is {share:.0}% of everything stored on its filesystem."),
                });
            }
        }
        warnings.sort_by_key(|w| Reverse(w.level));
        let critical = warnings.iter().any(|w| w.level == Level::Critical);

        self.modal = Some(Modal::Confirm(Confirm {
            size,
            entries: items.iter().map(|i| i.entries).sum(),
            items,
            choice: Choice::Cancel,
            scroll: 0,
            warnings,
            word: critical.then(random_word),
            typed: String::new(),
            // Root's trash is not the trash of whoever ran sudo, and a file
            // parked there is easy to forget. As root, only deletion is offered.
            trash: if self.is_root {
                None
            } else {
                TrashTool::find()
            },
        }));
    }

    fn start_delete(&mut self, confirm: Confirm) {
        // Checked here, where the work starts, and not only where the
        // buttons are drawn: no path through the dialog may skip it.
        if confirm.locked() {
            self.toast("type the confirmation word first", ToastKind::Info);
            self.modal = Some(Modal::Confirm(confirm));
            return;
        }
        if !self.allow_delete {
            self.toast(
                "deleting is disabled outside throwaway directories",
                ToastKind::Error,
            );
            return;
        }
        let method = match (confirm.choice, confirm.trash) {
            (Choice::Trash, Some(tool)) => Method::Trash(tool),
            (Choice::Delete, _) => Method::Delete,
            _ => return,
        };
        let work = confirm
            .items
            .iter()
            .map(|i| (i.path.clone(), i.entries))
            .collect();
        match DeleteJob::start(work, method) {
            Ok(job) => {
                self.modal = Some(Modal::Deleting(Deleting {
                    entries: confirm.entries,
                    items: confirm.items,
                    method,
                    job,
                }));
            }
            Err(e) => self.toast(format!("could not start deleting: {e}"), ToastKind::Error),
        }
    }

    /// Bring the tree in line with what the deletion actually did.
    fn finish_delete(
        &mut self,
        items: Vec<DelItem>,
        outcomes: Vec<Outcome>,
        method: Method,
        removed: u64,
    ) {
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        let mut gone: Vec<NodeId> = Vec::new();
        let mut failures: Vec<(String, String)> = Vec::new();
        let mut rescans: Vec<PathBuf> = Vec::new();
        let mut freed = 0u64;
        let mut skipped = 0usize;
        let mut outcomes = outcomes.into_iter();
        for item in &items {
            // The dialog was modal, so ids cannot have moved; check anyway
            // rather than remove the wrong node.
            let id = if (item.id as usize) < tree.len() && tree.path(item.id) == item.path {
                Some(item.id)
            } else {
                tree.find(&item.path)
            };
            // Trust the disk over the reported outcome.
            let exists = std::fs::symlink_metadata(&item.path).is_ok();
            match outcomes.next() {
                Some(Outcome::Skipped) => skipped += 1,
                Some(Outcome::Failed(f)) => {
                    failures.push((f.path.display().to_string(), f.reason.to_string()));
                    if exists && item.is_dir {
                        rescans.push(item.path.clone());
                    }
                }
                Some(Outcome::Deleted) => {}
                None => {
                    failures.push((item.path.display().to_string(), "no result".into()));
                    if exists && item.is_dir {
                        rescans.push(item.path.clone());
                    }
                }
            }
            if !exists && let Some(id) = id {
                freed += tree.node(id).disk;
                gone.push(id);
            }
        }

        // Where to stand afterwards if the current directory went away.
        let mut ancestors = vec![self.view];
        while let Some(&last) = ancestors.last() {
            if last == Tree::ROOT {
                break;
            }
            ancestors.push(tree.node(last).parent);
        }
        let deleted = gone.len();
        let remap = tree.remove(&gone);
        self.marks = self.marks.iter().filter_map(|&m| remap.map(m)).collect();
        self.view = ancestors
            .iter()
            .find_map(|&a| remap.map(a))
            .unwrap_or(Tree::ROOT);
        self.cursor = remap.map(self.cursor).unwrap_or(NONE);
        self.capacity = fsinfo::capacity(&self.root);
        scan::release_memory();
        if matches!(method, Method::Trash(_)) && deleted > 0 {
            // Trashed files reappear in the trash folder. If that folder is
            // part of this scan, refresh it so the totals stay honest.
            if let Some(trash) = trash_dir().filter(|t| tree.find(t).is_some()) {
                rescans.push(trash);
            }
        }
        self.pending_rescans.extend(rescans);
        self.changed();

        let items_word = |n: usize| if n == 1 { "item" } else { "items" };
        if failures.is_empty() && skipped == 0 {
            let text = match method {
                Method::Delete => format!(
                    "deleted {} {}, freed {}",
                    fmt::count(deleted as u64),
                    items_word(deleted),
                    fmt::size(freed)
                ),
                Method::Trash(_) => format!(
                    "moved {} {} ({}) to the trash; empty it to free the space",
                    fmt::count(deleted as u64),
                    items_word(deleted),
                    fmt::size(freed)
                ),
            };
            self.toast(text, ToastKind::Ok);
            return;
        }
        // A directory can be partly gone without counting as deleted, so
        // report entries rather than claim a number of bytes.
        let mut summary = format!(
            "{} of {} {} {}",
            fmt::count(deleted as u64),
            fmt::count(items.len() as u64),
            items_word(items.len()),
            if method == Method::Delete {
                "deleted"
            } else {
                "moved to the trash"
            },
        );
        if method == Method::Delete {
            summary.push_str(&format!(
                "  ·  {} files and directories removed in all",
                fmt::count(removed)
            ));
        }
        if skipped > 0 {
            summary.push_str(&format!("  ·  {skipped} not attempted"));
        }
        self.modal = Some(Modal::Report(Report {
            title: match method {
                Method::Delete => "Not everything was deleted",
                Method::Trash(_) => "Not everything was moved to the trash",
            },
            summary,
            failures,
            scroll: 0,
        }));
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

    /// The directory a shell should start in: the active directory, or the
    /// one holding the active file.
    fn shell_dir(&self) -> Option<PathBuf> {
        let tree = self.tree.as_ref()?;
        Some(tree.path(self.view))
    }

    /// Called by the event loop after the shell has exited.
    pub fn after_shell(&mut self) {
        // Whatever was done in there, show the result.
        self.rescan();
    }

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

    fn rescan(&mut self) {
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

/// The user's trash folder, per the freedesktop trash specification.
fn trash_dir() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/share")))?;
    Some(data.join("Trash"))
}

/// Case-insensitive name order with a case-sensitive tie break.
fn cmp_names(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let fold = |s: &[u8]| {
        s.iter()
            .map(|c| c.to_ascii_lowercase())
            .collect::<Vec<u8>>()
    };
    fold(a).cmp(&fold(b)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::with_files;
    use std::fs;
    use std::path::Path;

    fn app_for(root: &Path) -> App {
        let mut app = App::new(
            root.to_path_buf(),
            scan::Options::default(),
            Theme::new(true),
        );
        app.scan_blocking().unwrap();
        // No home directories unless a test declares one, so results do not
        // depend on who runs the tests or from where.
        app.guard = Guard::new(Vec::new());
        app
    }

    /// The same, with `home` (relative to the root) declared a home directory.
    fn app_with_home(root: &Path, home: &str) -> App {
        let mut app = app_for(root);
        app.guard = Guard::new(vec![(root.join(home), "alice".into())]);
        app
    }

    fn confirm(app: &App) -> &Confirm {
        match &app.modal {
            Some(Modal::Confirm(c)) => c,
            _ => panic!("no confirmation dialog is open"),
        }
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            key(app, KeyCode::Char(ch));
        }
    }

    /// Everything on screen at 120x40, as plain text.
    fn screen(app: &mut App) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for (i, cell) in buffer.content().iter().enumerate() {
            if i > 0 && i % 120 == 0 {
                out.push('\n');
            }
            out.push_str(cell.symbol());
        }
        out
    }

    fn key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        app.settle();
    }

    fn id(app: &App, root: &Path, rel: &str) -> NodeId {
        app.tree
            .as_ref()
            .unwrap()
            .find(&root.join(rel))
            .unwrap_or_else(|| panic!("{rel} is not in the tree"))
    }

    /// Paths of all marked nodes, relative to the root, sorted.
    fn marked(app: &App, root: &Path) -> Vec<String> {
        let tree = app.tree.as_ref().unwrap();
        let mut out: Vec<String> = app
            .marks
            .iter()
            .map(|&m| {
                tree.path(m)
                    .strip_prefix(root)
                    .unwrap()
                    .display()
                    .to_string()
            })
            .collect();
        out.sort();
        out
    }

    /// Structural invariants of the arena, plus a comparison against a fresh
    /// scan of the same directory.
    fn assert_consistent(app: &App, root: &Path) {
        let tree = app.tree.as_ref().unwrap();
        for i in 0..tree.len() as NodeId {
            let n = tree.node(i);
            if !n.is_dir() {
                assert_eq!(n.desc, 0, "non-directory {i} has descendants");
                continue;
            }
            let mut count = 0;
            let mut disk = 0;
            for c in tree.children(i) {
                assert_eq!(tree.node(c).parent, i, "child {c} of {i} points elsewhere");
                count += tree.node(c).desc + 1;
                disk += tree.node(c).disk;
            }
            assert_eq!(n.desc, count, "descendant count of {i}");
            assert!(n.disk >= disk, "directory {i} is smaller than its contents");
        }
        let fresh = app_for(root);
        let fresh_tree = fresh.tree.as_ref().unwrap();
        assert_eq!(
            tree.len(),
            fresh_tree.len(),
            "node count differs from a fresh scan"
        );
        let listing = |t: &Tree| {
            let mut v: Vec<(PathBuf, u64)> = (0..t.len() as NodeId)
                .filter(|&i| t.node(i).is_file())
                .map(|i| (t.path(i), t.node(i).size))
                .collect();
            v.sort();
            v
        };
        assert_eq!(listing(tree), listing(fresh_tree));
    }

    #[test]
    fn marks_follow_their_paths_through_a_rescan() {
        let root = with_files(
            "rescan",
            &[
                ("a/one", 5000),
                ("a/two", 3000),
                ("b/keep", 9000),
                ("b/doomed", 7000),
                ("c/deep/file", 100),
            ],
        );
        let mut app = app_for(&root);
        let doomed = id(&app, &root, "b/doomed");
        let one = id(&app, &root, "a/one");
        app.toggle_mark(doomed);
        app.toggle_mark(one);
        assert_eq!(marked(&app, &root), ["a/one", "b/doomed"]);

        // Change `a` on disk so that rescanning it renumbers everything after.
        for i in 0..40 {
            fs::write(root.join(format!("a/extra-{i}")), vec![b'y'; 20_000]).unwrap();
        }
        fs::remove_file(root.join("a/two")).unwrap();
        app.start_scan(Some(root.join("a")));
        app.settle();

        assert_eq!(marked(&app, &root), ["a/one", "b/doomed"]);
        assert_consistent(&app, &root);

        // What the dialog lists is exactly the marked paths.
        key(&mut app, KeyCode::Char('d'));
        let Some(Modal::Confirm(c)) = &app.modal else {
            panic!("no confirmation dialog");
        };
        let mut listed: Vec<&Path> = c.items.iter().map(|i| i.path.as_path()).collect();
        listed.sort();
        assert_eq!(listed, [root.join("a/one"), root.join("b/doomed")]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn deleting_marked_items_updates_disk_and_tree() {
        let root = with_files(
            "delete",
            &[
                ("big/blob", 50_000),
                ("big/nested/more", 20_000),
                ("mid/thing", 10_000),
                ("small", 1_000),
                ("stays/here", 4_000),
            ],
        );
        let mut app = app_for(&root);
        let before = app.tree.as_ref().unwrap().node(Tree::ROOT).size;
        let (big, small) = (id(&app, &root, "big"), id(&app, &root, "small"));
        app.toggle_mark(big);
        app.toggle_mark(small);

        key(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));
        // Enter on the default button cancels.
        key(&mut app, KeyCode::Enter);
        assert!(app.modal.is_none());
        assert!(root.join("big/blob").exists());

        key(&mut app, KeyCode::Char('d'));
        key(&mut app, KeyCode::BackTab);
        key(&mut app, KeyCode::Enter);
        assert!(app.modal.is_none(), "a clean deletion leaves no dialog");
        assert!(!root.join("big").exists());
        assert!(!root.join("small").exists());
        assert!(root.join("mid/thing").exists());
        assert!(root.join("stays/here").exists());
        assert!(app.marks.is_empty());

        let tree = app.tree.as_ref().unwrap();
        assert!(tree.find(&root.join("big")).is_none());
        let after = tree.node(Tree::ROOT).size;
        assert!(before - after >= 71_000, "freed {} bytes", before - after);
        assert_consistent(&app, &root);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Events that arrive together are all handled before the next frame, so
    /// nothing a key acts on may depend on a frame having been drawn.
    #[test]
    fn keys_in_one_batch_act_on_what_is_current() {
        let root = with_files(
            "batch",
            &[
                ("gone/a", 30_000),
                ("gone/b", 29_000),
                ("gone/c", 28_000),
                ("keep/x", 9_000),
                ("keep/y", 8_000),
            ],
        );
        let mut app = app_for(&root);
        // One frame: the map laid out, the largest files listed.
        let map = Rect::new(0, 0, 80, 30);
        app.hit.map = map;
        app.ensure_layout(map);
        key(&mut app, KeyCode::Char('2'));
        app.prepare();
        app.largest_sel = 2;

        // No frame from here on. Delete `gone`, which renumbers everything.
        key(&mut app, KeyCode::Char('1'));
        app.cursor = id(&app, &root, "gone");
        key(&mut app, KeyCode::Char('d'));
        key(&mut app, KeyCode::BackTab);
        key(&mut app, KeyCode::Enter);
        assert!(!root.join("gone").exists());

        // The largest files are the ones that are left.
        key(&mut app, KeyCode::Char('2'));
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(marked(&app, &root), ["keep/y"]);
        // The map moves among the tiles of what is on view now.
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Down);
        assert!(app.rows.contains(&app.cursor), "cursor {}", app.cursor);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn deleting_the_directory_on_view_moves_up() {
        let root = with_files("view", &[("outer/inner/file", 3000), ("other", 10)]);
        let mut app = app_for(&root);
        let (outer, inner) = (id(&app, &root, "outer"), id(&app, &root, "outer/inner"));
        app.toggle_mark(outer);
        app.enter(outer);
        app.enter(inner);
        assert_eq!(app.view, inner);
        key(&mut app, KeyCode::Char('d'));
        key(&mut app, KeyCode::BackTab);
        key(&mut app, KeyCode::Enter);
        assert!(!root.join("outer").exists());
        assert_eq!(app.view, Tree::ROOT);
        assert_consistent(&app, &root);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_partial_failure_is_reported_and_rescanned() {
        let root = with_files(
            "partial",
            &[("dir/removable", 2000), ("dir/pinned/file", 2000)],
        );
        // Something that deletion cannot get past: a directory we may not read.
        use std::os::unix::fs::PermissionsExt;
        let pinned = root.join("dir/pinned");
        fs::set_permissions(&pinned, fs::Permissions::from_mode(0o000)).unwrap();
        let mut app = app_for(&root);
        if app.is_root {
            // Root reads everything, so there is nothing to fail on.
            fs::set_permissions(&pinned, fs::Permissions::from_mode(0o755)).unwrap();
            fs::remove_dir_all(&root).unwrap();
            return;
        }
        let dir = id(&app, &root, "dir");
        app.toggle_mark(dir);
        key(&mut app, KeyCode::Char('d'));
        key(&mut app, KeyCode::BackTab);
        key(&mut app, KeyCode::Enter);

        let Some(Modal::Report(r)) = &app.modal else {
            panic!("expected a failure report");
        };
        assert_eq!(r.failures.len(), 1);
        assert_eq!(r.failures[0].0, pinned.display().to_string());
        assert_eq!(r.failures[0].1, "permission denied");
        assert!(!root.join("dir/removable").exists());
        assert!(pinned.exists());
        // The tree was refreshed to show what is left.
        let tree = app.tree.as_ref().unwrap();
        assert!(tree.find(&root.join("dir/removable")).is_none());
        assert!(tree.find(&pinned).is_some());

        fs::set_permissions(&pinned, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn nothing_is_deleted_when_deletion_is_disabled() {
        let root = with_files("gate", &[("precious", 100)]);
        for read_only in [true, false] {
            let mut app = app_for(&root);
            app.read_only = read_only;
            app.allow_delete = false;
            let precious = id(&app, &root, "precious");
            app.toggle_mark(precious);
            key(&mut app, KeyCode::Char('d'));
            key(&mut app, KeyCode::BackTab);
            key(&mut app, KeyCode::Enter);
            assert!(root.join("precious").exists());
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn marking_a_directory_absorbs_marks_below_it() {
        let root = with_files("absorb", &[("d/a", 10), ("d/b", 20), ("e", 5)]);
        let mut app = app_for(&root);
        let (a, d) = (id(&app, &root, "d/a"), id(&app, &root, "d"));
        app.toggle_mark(a);
        app.toggle_mark(d);
        assert_eq!(marked(&app, &root), ["d"]);
        assert!(app.is_marked(a));
        // A node inside a marked directory cannot be marked or unmarked alone.
        app.toggle_mark(a);
        assert_eq!(marked(&app, &root), ["d"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn vim_keys_walk_the_tree_in_both_panes() {
        // One directory holding everything, as when looking at /home.
        let root = with_files(
            "vim",
            &[("only/big", 9000), ("only/mid", 5000), ("only/small", 100)],
        );
        let mut app = app_for(&root);
        let only = id(&app, &root, "only");
        for focus in [Pane::Map, Pane::Side] {
            app.focus = focus;
            assert_eq!((app.view, app.cursor), (Tree::ROOT, only), "{focus:?}");
            key(&mut app, KeyCode::Char('l'));
            assert_eq!(app.view, only, "l opens, {focus:?}");
            assert_eq!(app.cursor, id(&app, &root, "only/big"));
            key(&mut app, KeyCode::Char('j'));
            assert_eq!(
                app.cursor,
                id(&app, &root, "only/mid"),
                "j is next, {focus:?}"
            );
            key(&mut app, KeyCode::Char('j'));
            key(&mut app, KeyCode::Char('j'));
            assert_eq!(
                app.cursor,
                id(&app, &root, "only/small"),
                "j stops at the end"
            );
            key(&mut app, KeyCode::Char('k'));
            assert_eq!(
                app.cursor,
                id(&app, &root, "only/mid"),
                "k is previous, {focus:?}"
            );
            // l on a file does nothing.
            key(&mut app, KeyCode::Char('l'));
            assert_eq!(app.view, only);
            key(&mut app, KeyCode::Char('h'));
            assert_eq!(
                (app.view, app.cursor),
                (Tree::ROOT, only),
                "h goes up, {focus:?}"
            );
            // h at the top stays put.
            key(&mut app, KeyCode::Char('h'));
            assert_eq!(app.view, Tree::ROOT);
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_protected_target_stays_locked_until_the_word_is_typed() {
        let root = with_files(
            "locked",
            &[
                ("home/alice/thesis.tex", 4000),
                ("home/alice/.ssh/id_ed25519", 400),
            ],
        );
        let mut app = app_with_home(&root, "home/alice");
        let alice = id(&app, &root, "home/alice");
        app.toggle_mark(alice);
        key(&mut app, KeyCode::Char('d'));

        let c = confirm(&app);
        assert_eq!(c.level(), Some(Level::Critical));
        assert!(c.warnings[0].reason.contains("alice"));
        let word = c.word.expect("a critical target asks for a word");
        assert!(c.locked());

        // The buttons cannot be reached while locked.
        for code in [
            KeyCode::Right,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Left,
        ] {
            key(&mut app, code);
            assert_eq!(confirm(&app).choice, Choice::Cancel);
        }
        // Letters are input now, not shortcuts: none of these close the dialog.
        type_text(&mut app, "qnhl");
        assert_eq!(confirm(&app).typed, "qnhl");
        assert!(confirm(&app).locked(), "a wrong word unlocks nothing");
        for _ in 0..4 {
            key(&mut app, KeyCode::Backspace);
        }

        // Even with the choice forced, Enter starts nothing while locked...
        let Some(Modal::Confirm(c)) = &mut app.modal else {
            unreachable!()
        };
        c.choice = Choice::Delete;
        key(&mut app, KeyCode::Enter);
        assert!(root.join("home/alice/thesis.tex").exists());
        // ...and neither does asking for the deletion directly.
        let Some(Modal::Confirm(c)) = app.modal.take() else {
            panic!("the dialog should still be open");
        };
        app.start_delete(c);
        app.settle();
        assert!(root.join("home/alice/thesis.tex").exists());
        assert!(
            matches!(app.modal, Some(Modal::Confirm(_))),
            "the dialog comes back"
        );

        // Typing the word unlocks; taking a letter back locks again and
        // drops the selection back to Cancel.
        type_text(&mut app, word);
        assert!(!confirm(&app).locked());
        key(&mut app, KeyCode::BackTab);
        assert_eq!(confirm(&app).choice, Choice::Delete);
        key(&mut app, KeyCode::Backspace);
        assert!(confirm(&app).locked());
        assert_eq!(confirm(&app).choice, Choice::Cancel);
        key(&mut app, KeyCode::Enter);
        assert!(app.modal.is_none(), "Enter on Cancel closes");
        assert!(root.join("home/alice/thesis.tex").exists());

        // The whole way through, typed in capitals for good measure.
        key(&mut app, KeyCode::Char('d'));
        let word = confirm(&app).word.unwrap();
        type_text(&mut app, &word.to_uppercase());
        key(&mut app, KeyCode::BackTab);
        key(&mut app, KeyCode::Enter);
        assert!(!root.join("home/alice").exists());
        assert!(root.join("home").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_caution_warns_but_confirms_the_usual_way() {
        let root = with_files(
            "caution",
            &[
                ("home/alice/Documents/notes.txt", 3000),
                ("home/alice/keep", 10),
            ],
        );
        let mut app = app_with_home(&root, "home/alice");
        let docs = id(&app, &root, "home/alice/Documents");
        app.toggle_mark(docs);
        key(&mut app, KeyCode::Char('d'));
        let c = confirm(&app);
        assert_eq!(c.level(), Some(Level::Caution));
        assert!(c.word.is_none() && !c.locked());
        let text = screen(&mut app);
        assert!(text.contains("CHECK FIRST"), "the warning is on screen");
        assert!(text.contains("top-level folder of alice"));
        assert!(!text.contains("PROTECTED"));

        key(&mut app, KeyCode::BackTab);
        key(&mut app, KeyCode::Enter);
        assert!(!root.join("home/alice/Documents").exists());
        assert!(root.join("home/alice/keep").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_ordinary_target_gets_the_plain_dialog() {
        let root = with_files("plain", &[("home/alice/Documents/old/draft", 3000)]);
        let mut app = app_with_home(&root, "home/alice");
        let old = id(&app, &root, "home/alice/Documents/old");
        app.toggle_mark(old);
        key(&mut app, KeyCode::Char('d'));
        let c = confirm(&app);
        assert!(c.warnings.is_empty() && c.word.is_none());
        let text = screen(&mut app);
        assert!(!text.contains("PROTECTED") && !text.contains("CHECK FIRST"));
        // The letter shortcuts still work here.
        key(&mut app, KeyCode::Char('n'));
        assert!(app.modal.is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_protected_item_is_never_lost_in_a_long_list() {
        // Twenty-four ordinary files and, smallest of all, a key directory.
        let mut files: Vec<(String, usize)> = (0..24)
            .map(|i| {
                (
                    format!("home/alice/work/data-{i:02}.bin"),
                    50_000 + i * 1000,
                )
            })
            .collect();
        files.push(("home/alice/.ssh/id_ed25519".into(), 100));
        let borrowed: Vec<(&str, usize)> = files.iter().map(|(p, s)| (p.as_str(), *s)).collect();
        let root = with_files("buried", &borrowed);
        let mut app = app_with_home(&root, "home/alice");
        for (path, _) in &files[..24] {
            let node = id(&app, &root, path);
            app.toggle_mark(node);
        }
        let ssh = id(&app, &root, "home/alice/.ssh");
        app.toggle_mark(ssh);
        key(&mut app, KeyCode::Char('d'));

        let c = confirm(&app);
        assert_eq!(c.items.len(), 25);
        assert!(
            c.items.last().unwrap().path.ends_with(".ssh"),
            "it sorts last by size"
        );
        assert_eq!(
            c.warnings[0].level,
            Level::Critical,
            "but its warning comes first"
        );
        assert!(c.locked());
        let text = screen(&mut app);
        assert!(text.contains("PROTECTED"));
        assert!(text.contains("home/alice/.ssh"), "the path is on screen");
        assert!(text.contains("cannot be recovered"));
        assert!(text.contains(c_word(&app)), "and so is the word to type");
        fs::remove_dir_all(&root).unwrap();
    }

    fn c_word(app: &App) -> &'static str {
        confirm(app).word.unwrap()
    }

    #[test]
    fn unlock_words_are_typeable_and_vary() {
        for word in UNLOCK_WORDS {
            assert!(word.len() >= 4 && word.chars().all(|c| c.is_ascii_lowercase()));
        }
        let seen: std::collections::HashSet<&str> = (0..200).map(|_| random_word()).collect();
        assert!(
            seen.len() > 10,
            "only {} different words in 200 draws",
            seen.len()
        );
    }
}
