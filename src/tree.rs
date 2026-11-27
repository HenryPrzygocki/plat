// SPDX-License-Identifier: GPL-3.0-or-later
//! The scanned file tree, stored as one flat arena.
//!
//! Nodes are laid out in depth-first pre-order, and the children of every
//! directory are emitted largest first. Two things follow from that layout:
//!
//! * the subtree of node `i` is exactly the range `i ..= i + desc`, so a whole
//!   subtree can be scanned, summed or cut out as a contiguous slice;
//! * the first child of a directory is `i + 1`, and the next sibling of a node
//!   `c` is `c + 1 + desc(c)`, so no child or sibling links are stored.
//!
//! Names live in one shared byte pool. A node is 40 bytes.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::scan::{NO_SUB, RawDir, RawEntry, RootMeta};

pub type NodeId = u32;

/// Length byte of a name too long to fit in it. Such a name is stored with
/// its length in two bytes in front. Linux limits names to 255 bytes on most
/// filesystems, but vfat, exFAT and NTFS allow 255 UTF-16 units, which can
/// come to over 700 bytes of UTF-8.
const LONG_NAME: u8 = u8::MAX;

/// Append `name` to a name pool. Returns the length byte to store with it.
pub fn push_name(pool: &mut Vec<u8>, name: &[u8]) -> u8 {
    if name.len() < LONG_NAME as usize {
        pool.extend_from_slice(name);
        return name.len() as u8;
    }
    // Far beyond anything a filesystem hands out, but never silently wrong.
    let len = name.len().min(u16::MAX as usize);
    pool.extend_from_slice(&(len as u16).to_le_bytes());
    pool.extend_from_slice(&name[..len]);
    LONG_NAME
}

/// The bytes a name takes up in its pool, length prefix included.
#[inline]
fn stored(pool: &[u8], off: u32, len: u8) -> std::ops::Range<usize> {
    let off = off as usize;
    if len < LONG_NAME {
        off..off + len as usize
    } else {
        off..off + 2 + u16::from_le_bytes([pool[off], pool[off + 1]]) as usize
    }
}

/// The name stored at `off` with length byte `len`.
#[inline]
pub fn name_at(pool: &[u8], off: u32, len: u8) -> &[u8] {
    let range = stored(pool, off, len);
    let skip = if len < LONG_NAME { 0 } else { 2 };
    &pool[range.start + skip..range.end]
}

/// Node flag bits. The low two bits hold the kind.
pub mod flag {
    pub const FILE: u8 = 0;
    pub const DIR: u8 = 1;
    pub const SYMLINK: u8 = 2;
    pub const SPECIAL: u8 = 3;
    pub const KIND_MASK: u8 = 3;
    /// Could not be read (directory) or stat'ed (any entry).
    pub const ERR: u8 = 0x04;
    /// Extra link to an inode that is already counted elsewhere.
    pub const HARDLINK: u8 = 0x08;
    /// Directory on a different filesystem than its parent.
    pub const MOUNT: u8 = 0x10;
    /// Mount point the scan did not descend into.
    pub const SKIPPED: u8 = 0x20;
    /// Something below this directory could not be read, so sizes are lower bounds.
    pub const PARTIAL: u8 = 0x40;
}

/// Which size a view works with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    /// Bytes allocated on disk.
    Disk,
    /// Apparent size: what the file claims to contain.
    Apparent,
}

#[derive(Clone, Copy, Debug)]
pub struct Node {
    /// Apparent bytes, including everything below a directory.
    pub size: u64,
    /// Allocated bytes, including everything below a directory.
    pub disk: u64,
    /// Modification time. For a directory, the newest time found below it.
    pub mtime: i64,
    name_off: u32,
    pub parent: NodeId,
    /// Number of nodes below this one. Zero for anything but a directory.
    pub desc: u32,
    /// Extension id for files, filesystem type id for skipped mounts.
    pub ext: u16,
    name_len: u8,
    pub flags: u8,
}

const _: () = assert!(std::mem::size_of::<Node>() == 40);

impl Node {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & flag::KIND_MASK == flag::DIR
    }
    #[inline]
    pub fn is_file(&self) -> bool {
        self.flags & flag::KIND_MASK == flag::FILE
    }
    #[inline]
    pub fn kind(&self) -> u8 {
        self.flags & flag::KIND_MASK
    }
    #[inline]
    pub fn has(&self, f: u8) -> bool {
        self.flags & f != 0
    }
    #[inline]
    pub fn get(&self, metric: Metric) -> u64 {
        match metric {
            Metric::Disk => self.disk,
            Metric::Apparent => self.size,
        }
    }
}

/// How node ids moved after the arena was edited.
#[derive(Debug, Default)]
pub struct Remap {
    /// (first id of a cut, one past its last id, shift applied to ids after it).
    cuts: Vec<(u32, u32, i64)>,
}

impl Remap {
    /// New id of a node, or `None` if the node no longer exists.
    pub fn map(&self, old: NodeId) -> Option<NodeId> {
        let k = self.cuts.partition_point(|&(start, _, _)| start <= old);
        if k == 0 {
            return Some(old);
        }
        let (_, end, shift) = self.cuts[k - 1];
        (old >= end).then(|| (old as i64 + shift) as u32)
    }
}

#[derive(Debug)]
pub enum BuildError {
    /// More nodes or name bytes than 32-bit indices can address.
    TooLarge,
}

pub struct Tree {
    nodes: Vec<Node>,
    names: Vec<u8>,
    /// Bytes of `names` that no node refers to any more.
    dead_names: usize,
    root: PathBuf,
}

impl Tree {
    pub const ROOT: NodeId = 0;

    /// Flatten a scan result into an arena.
    pub fn build(root: PathBuf, meta: RootMeta, raw: RawDir) -> Result<Tree, BuildError> {
        if raw.count >= u32::MAX as u64 || raw.name_bytes >= u32::MAX as u64 {
            return Err(BuildError::TooLarge);
        }
        let mut nodes = Vec::with_capacity(raw.count as usize + 1);
        let mut names = Vec::with_capacity(raw.name_bytes as usize);
        let mut flags = flag::DIR;
        if raw.err {
            flags |= flag::ERR;
        }
        if raw.err || raw.partial {
            flags |= flag::PARTIAL;
        }
        nodes.push(Node {
            size: meta.size + raw.size,
            disk: meta.disk + raw.disk,
            mtime: meta.mtime.max(raw.mtime),
            name_off: 0,
            parent: NodeId::MAX,
            desc: raw.count as u32,
            ext: 0,
            name_len: 0,
            flags,
        });
        emit(raw, Tree::ROOT, &mut nodes, &mut names);
        Ok(Tree {
            nodes,
            names,
            dead_names: 0,
            root,
        })
    }

    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// All nodes, in depth-first order.
    #[inline]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    /// Raw name bytes of a node. The root has an empty name; use
    /// [`Tree::root_path`] for it.
    #[inline]
    pub fn name(&self, id: NodeId) -> &[u8] {
        let n = &self.nodes[id as usize];
        name_at(&self.names, n.name_off, n.name_len)
    }

    /// Children of a directory, largest first.
    pub fn children(&self, id: NodeId) -> Children<'_> {
        let end = id + 1 + self.nodes[id as usize].desc;
        Children {
            nodes: &self.nodes,
            next: id + 1,
            end,
        }
    }

    /// True if `node` lies strictly below `ancestor`.
    #[inline]
    pub fn is_below(&self, node: NodeId, ancestor: NodeId) -> bool {
        node > ancestor && node <= ancestor + self.nodes[ancestor as usize].desc
    }

    /// Exact filesystem path of a node, built from raw name bytes.
    pub fn path(&self, id: NodeId) -> PathBuf {
        self.root.join(self.rel_path(id, Tree::ROOT))
    }

    /// Path of a node relative to `base`, which must be the node or one of its
    /// ancestors.
    pub fn rel_path(&self, id: NodeId, base: NodeId) -> PathBuf {
        let mut chain = Vec::new();
        let mut cur = id;
        while cur != base && cur != Tree::ROOT {
            chain.push(cur);
            cur = self.nodes[cur as usize].parent;
        }
        let mut path = PathBuf::new();
        for &c in chain.iter().rev() {
            path.push(OsStr::from_bytes(self.name(c)));
        }
        path
    }

    /// Find a node by absolute path.
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let mut cur = Tree::ROOT;
        for comp in rel.components() {
            let want = comp.as_os_str().as_bytes();
            cur = self.children(cur).find(|&c| self.name(c) == want)?;
        }
        Some(cur)
    }

    /// Remove whole subtrees and compact the arena. Sizes and counts of all
    /// ancestors are adjusted. Ids that are nested inside another id in the
    /// list are ignored. The root cannot be removed.
    pub fn remove(&mut self, ids: &[NodeId]) -> Remap {
        let mut roots: Vec<NodeId> = ids.iter().copied().filter(|&i| i != Tree::ROOT).collect();
        roots.sort_unstable();
        roots.dedup();
        let mut ranges: Vec<(u32, u32)> = Vec::with_capacity(roots.len());
        for r in roots {
            let end = r + 1 + self.nodes[r as usize].desc;
            match ranges.last() {
                Some(&(_, last_end)) if r < last_end => {}
                _ => ranges.push((r, end)),
            }
        }
        if ranges.is_empty() {
            return Remap::default();
        }

        for &(start, end) in &ranges {
            self.dead_names += self.name_bytes(start..end);
            let gone = self.nodes[start as usize];
            let count = end - start;
            let mut cur = gone.parent;
            while cur != NodeId::MAX {
                let n = &mut self.nodes[cur as usize];
                n.size = n.size.saturating_sub(gone.size);
                n.disk = n.disk.saturating_sub(gone.disk);
                n.desc -= count;
                cur = n.parent;
            }
        }

        let mut remap = Remap::default();
        let mut removed = 0i64;
        for &(start, end) in &ranges {
            removed += (end - start) as i64;
            remap.cuts.push((start, end, -removed));
        }

        // Their parents survive, and may no longer hold anything unreadable.
        let mut parents: Vec<NodeId> = ranges
            .iter()
            .filter_map(|&(start, _)| remap.map(self.nodes[start as usize].parent))
            .collect();
        parents.dedup();

        let mut out = Vec::with_capacity(self.nodes.len() - removed as usize);
        let mut pos = 0u32;
        for &(start, end) in &ranges {
            out.extend_from_slice(&self.nodes[pos as usize..start as usize]);
            pos = end;
        }
        out.extend_from_slice(&self.nodes[pos as usize..]);
        let first_cut = ranges[0].0;
        for n in &mut out[first_cut as usize..] {
            if n.parent != NodeId::MAX && n.parent > first_cut {
                // A surviving node never has a removed parent.
                n.parent = remap.map(n.parent).unwrap_or(n.parent);
            }
        }
        self.nodes = out;
        for p in parents {
            self.refresh_partial(p);
        }
        self.reclaim_names();
        remap
    }

    /// Replace everything below directory `id` with a fresh scan of it.
    pub fn replace(
        &mut self,
        id: NodeId,
        meta: RootMeta,
        raw: RawDir,
    ) -> Result<Remap, BuildError> {
        let old = self.nodes[id as usize];
        let old_end = id + 1 + old.desc;
        let new_total = self.nodes.len() as u64 - old.desc as u64 + raw.count;
        if new_total >= u32::MAX as u64
            || self.names.len() as u64 + raw.name_bytes >= u32::MAX as u64
        {
            return Err(BuildError::TooLarge);
        }
        let new_desc = raw.count as u32;
        let shift = new_desc as i64 - old.desc as i64;
        let new_size = meta.size + raw.size;
        let new_disk = meta.disk + raw.disk;

        self.dead_names += self.name_bytes(id + 1..old_end);
        let mut out = Vec::with_capacity(new_total as usize);
        out.extend_from_slice(&self.nodes[..=id as usize]);
        {
            let n = &mut out[id as usize];
            n.flags &= !(flag::ERR | flag::PARTIAL);
            if raw.err {
                n.flags |= flag::ERR;
            }
            if raw.err || raw.partial {
                n.flags |= flag::PARTIAL;
            }
            n.mtime = meta.mtime.max(raw.mtime);
        }
        emit(raw, id, &mut out, &mut self.names);
        let tail_start = out.len();
        out.extend_from_slice(&self.nodes[old_end as usize..]);
        for n in &mut out[tail_start..] {
            if n.parent != NodeId::MAX && n.parent >= old_end {
                n.parent = (n.parent as i64 + shift) as u32;
            }
        }

        // The node itself and all of its ancestors change by the same amounts.
        let mut cur = id;
        while cur != NodeId::MAX {
            let n = &mut out[cur as usize];
            n.size = (n.size + new_size).saturating_sub(old.size);
            n.disk = (n.disk + new_disk).saturating_sub(old.disk);
            n.desc = (n.desc as i64 + shift) as u32;
            cur = n.parent;
        }
        self.nodes = out;
        self.refresh_partial(self.nodes[id as usize].parent);
        self.reclaim_names();
        Ok(Remap {
            cuts: vec![(id + 1, old_end, shift)],
        })
    }

    /// Pool bytes taken by the names of the nodes in `ids`.
    fn name_bytes(&self, ids: std::ops::Range<NodeId>) -> usize {
        self.nodes[ids.start as usize..ids.end as usize]
            .iter()
            .map(|n| stored(&self.names, n.name_off, n.name_len).len())
            .sum()
    }

    /// Recompute whether `dir` and its ancestors hold anything unreadable,
    /// after something below `dir` changed. Stops at the first directory
    /// whose answer stays the same, since nothing above it can change then.
    fn refresh_partial(&mut self, dir: NodeId) {
        let mut cur = dir;
        while cur != NodeId::MAX {
            let n = self.nodes[cur as usize];
            let partial = n.has(flag::ERR)
                || self
                    .children(cur)
                    .any(|c| self.nodes[c as usize].has(flag::ERR | flag::PARTIAL));
            if partial == n.has(flag::PARTIAL) {
                return;
            }
            self.nodes[cur as usize].flags ^= flag::PARTIAL;
            cur = n.parent;
        }
    }

    /// Rescans and deletions leave the old names behind in the pool. Once
    /// they outweigh the live ones, copy the live names into a fresh pool:
    /// linear in the tree, and amortised over at least as many dead bytes.
    fn reclaim_names(&mut self) {
        if self.dead_names * 2 <= self.names.len() {
            return;
        }
        let mut names = Vec::with_capacity(self.names.len() - self.dead_names);
        for n in &mut self.nodes {
            let range = stored(&self.names, n.name_off, n.name_len);
            n.name_off = names.len() as u32;
            names.extend_from_slice(&self.names[range]);
        }
        self.names = names;
        self.dead_names = 0;
    }
}

/// Iterator over the direct children of a directory.
pub struct Children<'a> {
    nodes: &'a [Node],
    next: NodeId,
    end: NodeId,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        if self.next >= self.end {
            return None;
        }
        let id = self.next;
        self.next = id + 1 + self.nodes[id as usize].desc;
        Some(id)
    }
}

/// Append the contents of `dir` below `parent`, depth first, largest first.
///
/// `nodes.len()` must equal `parent + 1` on entry so that ids come out right.
/// Iterative, because directory depth is not bounded by anything sensible.
fn emit(dir: RawDir, parent: NodeId, nodes: &mut Vec<Node>, names: &mut Vec<u8>) {
    struct Frame {
        entries: Vec<RawEntry>,
        subdirs: Vec<RawDir>,
        order: Vec<u32>,
        pos: usize,
        name_base: u32,
        parent: NodeId,
    }

    fn open(dir: RawDir, parent: NodeId, names: &mut Vec<u8>) -> Frame {
        let name_base = names.len() as u32;
        names.extend_from_slice(&dir.names);
        let entries = dir.entries;
        let dir_names = &dir.names;
        let name_of = |e: &RawEntry| name_at(dir_names, e.name_off, e.name_len);
        let mut order: Vec<u32> = (0..entries.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| {
            let (ea, eb) = (&entries[a as usize], &entries[b as usize]);
            eb.disk
                .cmp(&ea.disk)
                .then(eb.size.cmp(&ea.size))
                .then_with(|| name_of(ea).cmp(name_of(eb)))
        });
        Frame {
            entries,
            subdirs: dir.subdirs,
            order,
            pos: 0,
            name_base,
            parent,
        }
    }

    let mut stack = vec![open(dir, parent, names)];
    while let Some(frame) = stack.last_mut() {
        if frame.pos == frame.order.len() {
            stack.pop();
            continue;
        }
        let e = &frame.entries[frame.order[frame.pos] as usize];
        frame.pos += 1;
        let id = nodes.len() as NodeId;
        let sub = (e.sub != NO_SUB).then(|| std::mem::take(&mut frame.subdirs[e.sub as usize]));
        nodes.push(Node {
            size: e.size,
            disk: e.disk,
            mtime: e.mtime,
            name_off: frame.name_base + e.name_off,
            parent: frame.parent,
            desc: sub.as_ref().map_or(0, |s| s.count as u32),
            ext: e.ext,
            name_len: e.name_len,
            flags: e.flags,
        });
        if let Some(sub) = sub {
            stack.push(open(sub, id, names));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Options, Progress, scan};
    use crate::testutil::{scratch, with_files};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn scan_of(path: &Path) -> (RootMeta, RawDir) {
        scan(path, Options::default(), &Progress::default()).unwrap()
    }

    #[test]
    fn names_of_every_length_survive() {
        // Longer than any name Linux filesystems create natively, so build
        // the scan result by hand.
        let mut raw = RawDir::default();
        let lengths = [0usize, 1, 254, 255, 256, 765, 70_000];
        let names: Vec<Vec<u8>> = lengths
            .iter()
            .enumerate()
            .map(|(i, &len)| (0..len).map(|k| b'a' + ((i + k) % 26) as u8).collect())
            .collect();
        for (i, name) in names.iter().enumerate() {
            let name_off = raw.names.len() as u32;
            let name_len = push_name(&mut raw.names, name);
            raw.entries.push(RawEntry {
                size: 1000 - i as u64,
                disk: 1000 - i as u64,
                mtime: 0,
                name_off,
                sub: NO_SUB,
                ext: 0,
                name_len,
                flags: flag::FILE,
            });
        }
        raw.count = names.len() as u64;
        raw.name_bytes = raw.names.len() as u64;
        let meta = RootMeta {
            size: 0,
            disk: 0,
            mtime: 0,
        };
        let tree = Tree::build(PathBuf::from("/r"), meta, raw).unwrap();
        for (i, name) in names.iter().enumerate() {
            let id = i as NodeId + 1;
            let want = &name[..name.len().min(u16::MAX as usize)];
            assert_eq!(tree.name(id), want, "name of {} bytes", name.len());
        }
        let long = Path::new("/r").join(OsStr::from_bytes(&names[5]));
        assert_eq!(tree.find(&long), Some(6));
        assert_eq!(tree.path(6), long);
    }

    #[test]
    fn rescans_do_not_grow_the_name_pool() {
        let files: Vec<(String, usize)> = (0..200).map(|i| (format!("d/f{i:03}"), 10)).collect();
        let files: Vec<(&str, usize)> = files.iter().map(|(n, s)| (n.as_str(), *s)).collect();
        let root = with_files("pool", &files);
        let (meta, raw) = scan_of(&root);
        let mut tree = Tree::build(root.clone(), meta, raw).unwrap();
        let live = tree.names.len();
        for _ in 0..20 {
            let d = tree.find(&root.join("d")).unwrap();
            let (meta, raw) = scan_of(&root.join("d"));
            tree.replace(d, meta, raw).unwrap();
            assert!(
                tree.names.len() <= 2 * live + 16,
                "{} bytes",
                tree.names.len()
            );
        }
        // Every name still reads back.
        let d = tree.find(&root.join("d")).unwrap();
        assert_eq!(tree.children(d).count(), 200);
        assert!(tree.find(&root.join("d/f123")).is_some());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unreadable_flags_follow_rescans_up_the_tree() {
        if crate::fsinfo::is_root() {
            return; // root reads everything, so nothing is ever partial
        }
        let root = scratch("partial");
        fs::create_dir_all(root.join("a/b/locked")).unwrap();
        fs::write(root.join("a/b/locked/f"), b"x").unwrap();
        fs::write(root.join("a/b/fine"), b"x").unwrap();
        let lock = |mode| {
            fs::set_permissions(root.join("a/b/locked"), fs::Permissions::from_mode(mode)).unwrap()
        };
        let partial = |tree: &Tree, rel: &str| {
            let id = tree.find(&root.join(rel)).unwrap();
            tree.node(id).has(flag::PARTIAL)
        };
        let rescan = |tree: &mut Tree, rel: &str| {
            let id = tree.find(&root.join(rel)).unwrap();
            let (meta, raw) = scan_of(&root.join(rel));
            tree.replace(id, meta, raw).unwrap();
        };
        let (meta, raw) = scan_of(&root);
        let mut tree = Tree::build(root.clone(), meta, raw).unwrap();
        assert!(!partial(&tree, "a"));

        // Something becomes unreadable: everything above it says so.
        lock(0o000);
        rescan(&mut tree, "a/b");
        assert!(partial(&tree, "a/b") && partial(&tree, "a") && partial(&tree, ""));

        // And stops saying so once it can be read again.
        lock(0o755);
        rescan(&mut tree, "a/b");
        assert!(!partial(&tree, "a/b") && !partial(&tree, "a") && !partial(&tree, ""));

        // Deleting the unreadable part has the same effect.
        lock(0o000);
        rescan(&mut tree, "a/b");
        let locked = tree.find(&root.join("a/b/locked")).unwrap();
        tree.remove(&[locked]);
        assert!(!partial(&tree, "a/b") && !partial(&tree, "a") && !partial(&tree, ""));
        lock(0o755);
        fs::remove_dir_all(&root).unwrap();
    }
}
