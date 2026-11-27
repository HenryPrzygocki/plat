// SPDX-License-Identifier: GPL-3.0-or-later
//! Parallel filesystem scanner.
//!
//! Each directory is one rayon task. A task reads the directory, stats every
//! entry relative to the open directory descriptor (so no path is resolved
//! per file), then fans out over its subdirectories. Results come back as a tree of per-directory chunks, which
//! [`crate::tree::Tree`] flattens into its arena in one pass.

use std::ffi::OsStr;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use rayon::prelude::*;
use rustc_hash::FxHashSet;
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Statx, StatxFlags};

use crate::ext;
use crate::fsinfo::{self, FsClass};
use crate::tree::flag;

/// Marker for "this entry has no scanned subdirectory".
pub const NO_SUB: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Never leave the filesystem the scan started on.
    pub one_fs: bool,
    /// Descend into every mount, including virtual and network filesystems.
    pub all_mounts: bool,
}

/// Live counters shared between the scanner and the UI.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    pub cancel: AtomicBool,
}

/// One scanned directory: its direct entries plus the scanned subdirectories.
#[derive(Default)]
pub struct RawDir {
    /// Entry names, concatenated.
    pub names: Vec<u8>,
    pub entries: Vec<RawEntry>,
    /// Scanned subdirectories, referenced by `RawEntry::sub`.
    pub subdirs: Vec<RawDir>,
    /// Number of nodes below this directory.
    pub count: u64,
    /// Total name bytes below this directory, for preallocation.
    pub name_bytes: u64,
    /// Apparent and allocated bytes below this directory.
    pub size: u64,
    pub disk: u64,
    /// Newest modification time below this directory.
    pub mtime: i64,
    /// The directory itself could not be read.
    pub err: bool,
    /// Something below this directory could not be read.
    pub partial: bool,
}

pub struct RawEntry {
    /// Apparent size. For directories this includes everything below.
    pub size: u64,
    /// Allocated size. For directories this includes everything below.
    pub disk: u64,
    pub mtime: i64,
    pub name_off: u32,
    pub sub: u32,
    pub ext: u16,
    pub name_len: u8,
    pub flags: u8,
}

/// Metadata of the scan root itself.
#[derive(Clone, Copy, Debug)]
pub struct RootMeta {
    pub size: u64,
    pub disk: u64,
    pub mtime: i64,
}

struct Ctx<'a> {
    opts: Options,
    progress: &'a Progress,
    /// (device, inode) of every multiply-linked file seen so far.
    hardlinks: Mutex<FxHashSet<(u64, u64)>>,
}

/// How entries are stat'ed: never through a symlink, and never in a way that
/// makes the kernel mount something. A plain `statx` on an automount point
/// (a systemd `x-systemd.automount` entry, say) mounts it; with this flag it
/// reports the trigger directory itself, which then shows up as an autofs
/// mount and is skipped.
const STAT_FLAGS: AtFlags = AtFlags::SYMLINK_NOFOLLOW.union(AtFlags::NO_AUTOMOUNT);

/// One device number from the two halves `statx` reports.
fn dev_of(stx: &Statx) -> u64 {
    (stx.stx_dev_major as u64) << 32 | stx.stx_dev_minor as u64
}

/// Scan `root` and everything below it. `root` must be a directory.
pub fn scan(root: &Path, opts: Options, progress: &Progress) -> io::Result<(RootMeta, RawDir)> {
    // The root is what was asked for, so it is followed and mounted if need be.
    let stx = rustix::fs::statx(CWD, root, AtFlags::empty(), StatxFlags::BASIC_STATS)?;
    if FileType::from_raw_mode(stx.stx_mode as u32) != FileType::Directory {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "not a directory",
        ));
    }
    let ctx = Ctx {
        opts,
        progress,
        hardlinks: Mutex::new(FxHashSet::default()),
    };
    let dir = scan_dir(root, None, &ctx);
    let meta = RootMeta {
        size: stx.stx_size,
        disk: stx.stx_blocks * 512,
        mtime: stx.stx_mtime.tv_sec,
    };
    Ok((meta, dir))
}

/// Which filesystem a directory entry leads to, found without triggering an
/// automount: an `O_PATH` descriptor that is not asked to be a directory
/// stops at the trigger.
fn classify_entry(dir: BorrowedFd, name: &OsStr) -> (FsClass, u16) {
    let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::openat(dir, name, flags, Mode::empty()) {
        Ok(fd) => fsinfo::classify_fd(&fd),
        // Unknown is treated as a real disk, so nothing is hidden by mistake.
        Err(_) => (FsClass::Disk, 0),
    }
}

/// Scan one directory. `dev` is the device its parent expects it to be on, or
/// `None` right after crossing onto another filesystem, in which case the
/// directory is asked itself.
fn scan_dir(path: &Path, dev: Option<u64>, ctx: &Ctx) -> RawDir {
    let mut out = RawDir::default();
    if ctx.progress.cancel.load(Relaxed) {
        out.partial = true;
        return out;
    }
    let unreadable = |mut out: RawDir| {
        out.err = true;
        ctx.progress.errors.fetch_add(1, Relaxed);
        out
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let Ok(fd) = rustix::fs::open(path, flags, Mode::empty()) else {
        return unreadable(out);
    };
    let Ok(mut reader) = Dir::new(fd) else {
        return unreadable(out);
    };

    // Pass one: names and nothing else, so the directory is read in one go.
    let mut failed = false;
    while let Some(entry) = reader.read() {
        let Ok(entry) = entry else {
            failed = true;
            break;
        };
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        out.entries.push(RawEntry {
            size: 0,
            disk: 0,
            mtime: 0,
            name_off: out.names.len() as u32,
            sub: NO_SUB,
            ext: ext::EXT_NONE,
            // NAME_MAX is 255 on Linux, so this never truncates in practice.
            name_len: name.len().min(255) as u8,
            flags: flag::FILE,
        });
        out.names.extend_from_slice(&name[..name.len().min(255)]);
    }
    if failed {
        if out.entries.is_empty() {
            return unreadable(out);
        }
        out.partial = true;
    }
    let Ok(dirfd) = reader.fd() else {
        return unreadable(out);
    };
    let dev = match dev {
        Some(dev) => dev,
        None => {
            let own = rustix::fs::statx(dirfd, "", AtFlags::EMPTY_PATH, StatxFlags::BASIC_STATS);
            match own {
                Ok(stx) => dev_of(&stx),
                Err(_) => return unreadable(out),
            }
        }
    };

    // Pass two: one statx per entry, relative to the open directory, so the
    // kernel resolves a single name each time instead of a whole path.
    // Subdirectories to descend into: (entry index, device if still known).
    let mut pending: Vec<(u32, Option<u64>)> = Vec::new();
    let mut files = 0u64;
    let mut bytes = 0u64;
    for (index, e) in out.entries.iter_mut().enumerate() {
        let start = e.name_off as usize;
        let name = &out.names[start..start + e.name_len as usize];
        let name_os = OsStr::from_bytes(name);
        let stx = match rustix::fs::statx(dirfd, name_os, STAT_FLAGS, StatxFlags::BASIC_STATS) {
            Ok(stx) => stx,
            Err(_) => {
                e.flags |= flag::ERR;
                out.partial = true;
                continue;
            }
        };
        e.size = stx.stx_size;
        e.disk = stx.stx_blocks * 512;
        e.mtime = stx.stx_mtime.tv_sec;
        match FileType::from_raw_mode(stx.stx_mode as u32) {
            FileType::Directory => {
                e.flags = flag::DIR;
                if dev_of(&stx) == dev {
                    pending.push((index as u32, Some(dev)));
                } else {
                    e.flags |= flag::MOUNT;
                    let (class, fs_id) = classify_entry(dirfd, name_os);
                    let skip = ctx.opts.one_fs || (class != FsClass::Disk && !ctx.opts.all_mounts);
                    // The filesystem type is kept either way, for display.
                    e.ext = fs_id;
                    if skip {
                        // Sizes of a mount point we do not enter are
                        // meaningless (procfs reports terabytes).
                        e.flags |= flag::SKIPPED;
                        e.size = 0;
                        e.disk = 0;
                    } else {
                        pending.push((index as u32, None));
                    }
                }
            }
            FileType::RegularFile => {
                if stx.stx_nlink > 1
                    && !ctx
                        .hardlinks
                        .lock()
                        .unwrap()
                        .insert((dev_of(&stx), stx.stx_ino))
                {
                    // Another link to this inode was already counted.
                    e.flags |= flag::HARDLINK;
                    e.size = 0;
                    e.disk = 0;
                }
                e.ext = ext::intern(name, stx.stx_mode & 0o111 != 0);
                files += 1;
            }
            FileType::Symlink => {
                e.flags = flag::SYMLINK;
                files += 1;
            }
            _ => {
                e.flags = flag::SPECIAL;
                files += 1;
            }
        }
        bytes += e.disk;
    }
    // Close the directory before descending: recursion depth should not
    // translate into open descriptors.
    drop(reader);

    ctx.progress.files.fetch_add(files, Relaxed);
    ctx.progress.dirs.fetch_add(1, Relaxed);
    ctx.progress.bytes.fetch_add(bytes, Relaxed);

    if !pending.is_empty() {
        let child_path = |idx: u32| -> PathBuf {
            let e = &out.entries[idx as usize];
            let start = e.name_off as usize;
            path.join(OsStr::from_bytes(
                &out.names[start..start + e.name_len as usize],
            ))
        };
        let subdirs: Vec<RawDir> = if pending.len() == 1 {
            vec![scan_dir(&child_path(pending[0].0), pending[0].1, ctx)]
        } else {
            pending
                .par_iter()
                .map(|&(idx, dev)| scan_dir(&child_path(idx), dev, ctx))
                .collect()
        };
        for (k, (&(idx, _), sub)) in pending.iter().zip(&subdirs).enumerate() {
            let e = &mut out.entries[idx as usize];
            e.sub = k as u32;
            e.size += sub.size;
            e.disk += sub.disk;
            e.mtime = e.mtime.max(sub.mtime);
            if sub.err {
                e.flags |= flag::ERR;
            }
            if sub.err || sub.partial {
                e.flags |= flag::PARTIAL;
                out.partial = true;
            }
            out.count += sub.count;
            out.name_bytes += sub.name_bytes;
        }
        out.subdirs = subdirs;
    }

    // These live until the whole scan is done; give back the growth slack.
    out.entries.shrink_to_fit();
    out.names.shrink_to_fit();
    out.count += out.entries.len() as u64;
    out.name_bytes += out.names.len() as u64;
    out.mtime = i64::MIN;
    for e in &out.entries {
        out.size += e.size;
        out.disk += e.disk;
        if e.flags & flag::ERR == 0 {
            out.mtime = out.mtime.max(e.mtime);
        }
    }
    out
}

/// Hand freed heap memory back to the operating system.
///
/// A scan allocates one chunk per directory on a dozen threads and frees
/// them all once the tree is built. glibc keeps those pages in its per-thread
/// arenas indefinitely, which would leave the process at more than twice the
/// size of the tree for the rest of the session.
pub fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim has no preconditions; it only returns unused pages.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Configure the global scanner thread pool. Call once at startup.
pub fn init_pool(threads: usize) {
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("plat-scan-{i}"))
        // Directory depth is unbounded and work stealing stacks frames, so give
        // the workers room. The pages are only committed when touched.
        .stack_size(32 << 20)
        .build_global();
}

/// Default number of scanner threads.
pub fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Node, Tree};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plat-scan-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn scan_tree(root: &Path) -> (Tree, u64) {
        let progress = Progress::default();
        let (meta, raw) = scan(root, Options::default(), &progress).unwrap();
        let errors = progress.errors.load(Relaxed);
        (Tree::build(root.to_path_buf(), meta, raw).unwrap(), errors)
    }

    fn node<'a>(tree: &'a Tree, root: &Path, rel: &str) -> &'a Node {
        tree.node(
            tree.find(&root.join(rel))
                .unwrap_or_else(|| panic!("{rel} missing")),
        )
    }

    #[test]
    fn hard_links_are_counted_once() {
        let root = fixture("links");
        fs::write(root.join("original"), vec![1u8; 50_000]).unwrap();
        fs::hard_link(root.join("original"), root.join("second-name")).unwrap();
        fs::hard_link(root.join("original"), root.join("third-name")).unwrap();
        fs::write(root.join("separate"), vec![1u8; 7_000]).unwrap();

        let (tree, _) = scan_tree(&root);
        let names = ["original", "second-name", "third-name"];
        let counted: Vec<&Node> = names
            .iter()
            .map(|n| node(&tree, &root, n))
            .filter(|n| !n.has(flag::HARDLINK))
            .collect();
        assert_eq!(counted.len(), 1, "exactly one name carries the size");
        assert_eq!(counted[0].size, 50_000);
        let files: u64 = tree.children(Tree::ROOT).map(|c| tree.node(c).size).sum();
        assert_eq!(files, 57_000);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn symlinks_are_not_followed() {
        let root = fixture("symlinks");
        let outside = fixture("symlinks-outside");
        fs::write(outside.join("big"), vec![1u8; 300_000]).unwrap();
        fs::create_dir(root.join("real")).unwrap();
        fs::write(root.join("real/file"), vec![1u8; 1_000]).unwrap();
        symlink(&outside, root.join("to-outside")).unwrap();
        symlink(outside.join("big"), root.join("to-big")).unwrap();
        symlink(".", root.join("real/loop")).unwrap();

        let (tree, errors) = scan_tree(&root);
        assert_eq!(errors, 0);
        for link in ["to-outside", "to-big", "real/loop"] {
            let n = node(&tree, &root, link);
            assert_eq!(n.kind(), flag::SYMLINK, "{link}");
            assert_eq!(n.desc, 0, "{link} was descended into");
            assert!(n.size < 4096, "{link} took on its target's size");
        }
        // root, real, file and three links: nothing from outside leaked in.
        assert_eq!(tree.len(), 6);
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn unreadable_directories_are_flagged_all_the_way_up() {
        let root = fixture("denied");
        fs::create_dir_all(root.join("outer/locked")).unwrap();
        fs::write(root.join("outer/locked/secret"), vec![1u8; 5_000]).unwrap();
        fs::write(root.join("outer/visible"), vec![1u8; 2_000]).unwrap();
        fs::create_dir(root.join("fine")).unwrap();
        fs::set_permissions(root.join("outer/locked"), fs::Permissions::from_mode(0o000)).unwrap();

        let (tree, errors) = scan_tree(&root);
        fs::set_permissions(root.join("outer/locked"), fs::Permissions::from_mode(0o755)).unwrap();
        if crate::fsinfo::is_root() {
            // Root can read it, so there is nothing to flag.
            assert_eq!(errors, 0);
        } else {
            assert_eq!(errors, 1);
            let locked = node(&tree, &root, "outer/locked");
            assert!(locked.has(flag::ERR) && locked.desc == 0);
            assert!(node(&tree, &root, "outer").has(flag::PARTIAL));
            assert!(!node(&tree, &root, "outer").has(flag::ERR));
            assert!(tree.node(Tree::ROOT).has(flag::PARTIAL));
            assert!(!node(&tree, &root, "fine").has(flag::PARTIAL | flag::ERR));
            assert_eq!(node(&tree, &root, "outer/visible").size, 2_000);
        }
        fs::remove_dir_all(&root).unwrap();
    }

    /// Needs a private mount namespace:
    /// `unshare -Urm <test binary> --ignored mount`.
    #[test]
    #[ignore = "run under `unshare -Urm`"]
    fn mount_rules_decide_what_is_entered() {
        let root = fixture("mounts");
        fs::create_dir_all(root.join("ram")).unwrap();
        fs::write(root.join("on-disk"), vec![1u8; 3_000]).unwrap();
        let ok = std::process::Command::new("mount")
            .args(["-t", "tmpfs", "none"])
            .arg(root.join("ram"))
            .status()
            .unwrap()
            .success();
        assert!(
            ok,
            "could not mount a tmpfs; is this inside `unshare -Urm`?"
        );
        fs::write(root.join("ram/inside"), vec![1u8; 9_000]).unwrap();

        let run = |opts: Options| {
            let progress = Progress::default();
            let (meta, raw) = scan(&root, opts, &progress).unwrap();
            Tree::build(root.clone(), meta, raw).unwrap()
        };
        // A tmpfs under a tmpfs root is still a different filesystem, and a
        // virtual one: skipped by default, labelled, and given no size.
        let tree = run(Options::default());
        let ram = node(&tree, &root, "ram");
        assert!(ram.has(flag::MOUNT) && ram.has(flag::SKIPPED));
        assert_eq!((ram.size, ram.desc), (0, 0));
        assert_eq!(fsinfo::fs_name(ram.ext), "tmpfs");
        assert!(tree.find(&root.join("ram/inside")).is_none());

        let tree = run(Options {
            all_mounts: true,
            ..Options::default()
        });
        let ram = node(&tree, &root, "ram");
        assert!(ram.has(flag::MOUNT) && !ram.has(flag::SKIPPED));
        assert_eq!(node(&tree, &root, "ram/inside").size, 9_000);

        let tree = run(Options {
            one_fs: true,
            all_mounts: true,
        });
        assert!(node(&tree, &root, "ram").has(flag::SKIPPED));

        assert!(
            std::process::Command::new("umount")
                .arg(root.join("ram"))
                .status()
                .unwrap()
                .success()
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
