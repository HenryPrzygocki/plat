// SPDX-License-Identifier: GPL-3.0-or-later
//! Deleting: the confirmation dialog, the job, and bringing the tree in line
//! with what the job did.

use super::*;

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
pub(super) const UNLOCK_WORDS: &[&str] = &[
    "anchor", "badger", "bramble", "canyon", "cedar", "cinder", "cobalt", "comet", "copper",
    "ember", "falcon", "fennel", "garnet", "ginger", "harbor", "hazel", "juniper", "kettle",
    "lantern", "linden", "mantle", "marble", "meadow", "nettle", "orchid", "osprey", "pebble",
    "pillar", "quartz", "quiver", "russet", "saddle", "sorrel", "thistle", "timber", "tundra",
    "umber", "velvet", "vessel", "walnut", "wicker", "willow", "yarrow", "zephyr",
];

pub(super) fn random_word() -> &'static str {
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

    pub(super) fn step(&mut self, forward: bool) {
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

impl App {
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
    pub(super) fn open_delete(&mut self) {
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

    pub(super) fn start_delete(&mut self, confirm: Confirm) {
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
    pub(super) fn finish_delete(
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
}

/// The user's trash folder, per the freedesktop trash specification.
pub(super) fn trash_dir() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/share")))?;
    Some(data.join("Trash"))
}
