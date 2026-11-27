// SPDX-License-Identifier: GPL-3.0-or-later
//! Operations that change the disk.
//!
//! Deletion works relative to open directory descriptors and never through a
//! path once it has started, so nothing it does can be redirected by a symlink
//! that appears mid-way. That matters most when running as root over
//! directories other users can write to. It also refuses to cross onto
//! another filesystem: a mount point inside a doomed directory is left alone,
//! together with everything mounted there.

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, mpsc};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Statx, StatxFlags};
use rustix::io::Errno;

use crate::fsinfo::{dev_of, is_mount_root};

/// Entries are looked at without following a symlink or triggering an
/// automount.
const STAT_FLAGS: AtFlags = AtFlags::SYMLINK_NOFOLLOW.union(AtFlags::NO_AUTOMOUNT);

fn stat_fd(fd: impl AsFd) -> Result<Statx, Errno> {
    rustix::fs::statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::BASIC_STATS)
}

/// Something that could not be deleted, and why.
#[derive(Debug)]
pub struct Failure {
    /// The entry that could not be removed.
    pub path: PathBuf,
    pub reason: Reason,
}

/// Why an entry could not be deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// A system call failed.
    Os(Errno),
    /// The job was cancelled before this entry was reached.
    Cancelled,
    /// A mount point or subvolume, which deletion never enters.
    Mount,
    /// Something else is there now than when it was looked at.
    Changed,
    /// Entries kept appearing while it was being emptied.
    KeepsFilling,
    /// A path with no last component, such as `/`.
    FilesystemRoot,
    /// A name no path can hold, because it contains a NUL byte.
    InvalidName,
    /// What the trash helper said.
    Trash(String),
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Reason::Os(Errno::ACCESS | Errno::PERM) => "permission denied",
            Reason::Os(Errno::BUSY) => "in use (mount point?)",
            Reason::Os(Errno::ROFS) => "read-only filesystem",
            Reason::Os(Errno::NOENT) => "no longer exists",
            Reason::Os(Errno::LOOP | Errno::NOTDIR) | Reason::Changed => "changed since the scan",
            Reason::Os(e) => return write!(f, "{}", io::Error::from(*e)),
            Reason::Cancelled => "cancelled",
            Reason::Mount => "a mount point or subvolume, left alone with everything on it",
            Reason::KeepsFilling => "directory keeps filling up",
            Reason::FilesystemRoot => "refusing to delete a filesystem root",
            Reason::InvalidName => "invalid name",
            Reason::Trash(text) => text,
        };
        f.write_str(text)
    }
}

impl Failure {
    fn at(name: &CStr, reason: Reason) -> Failure {
        Failure {
            path: PathBuf::from(OsStr::from_bytes(name.to_bytes())),
            reason,
        }
    }

    /// Record that the failing entry lives inside `dir`.
    fn inside(mut self, dir: &CStr) -> Failure {
        self.path = Path::new(OsStr::from_bytes(dir.to_bytes())).join(&self.path);
        self
    }
}

/// Shared state of a running deletion.
#[derive(Default)]
pub struct Counters {
    /// Files and directories removed so far.
    pub removed: AtomicU64,
    /// Set to stop as soon as possible.
    pub cancel: AtomicBool,
}

/// Open a directory by absolute path without following a symlink in any
/// component. The result is only good for looking things up beneath it.
fn open_beneath_root(path: &Path) -> Result<OwnedFd, Errno> {
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut fd = rustix::fs::open("/", flags, Mode::empty())?;
    for comp in path.components() {
        match comp {
            Component::RootDir => {}
            Component::Normal(name) => {
                fd = rustix::fs::openat(&fd, name, flags, Mode::empty())?;
            }
            // Scanned paths are canonical, so anything else means trouble.
            _ => return Err(Errno::INVAL),
        }
    }
    Ok(fd)
}

/// Delete `path` and everything below it.
///
/// `path` must be absolute and free of symlinks, `.` and `..`, which holds for
/// every path that comes out of a scan.
pub fn remove(path: &Path, counters: &Counters) -> Result<(), Failure> {
    let whole = |reason: Reason| Failure {
        path: path.to_path_buf(),
        reason,
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(whole(Reason::FilesystemRoot));
    };
    let name = CString::new(name.as_bytes()).map_err(|_| whole(Reason::InvalidName))?;
    let dir = open_beneath_root(parent).map_err(|e| whole(Reason::Os(e)))?;
    let dev = dev_of(&stat_fd(&dir).map_err(|e| whole(Reason::Os(e)))?);
    remove_entry(dir.as_fd(), &name, dev, counters).map_err(|f| Failure {
        // Report the full path of whatever was in the way.
        path: parent.join(f.path),
        reason: f.reason,
    })
}

/// Remove one directory entry of any type.
fn remove_entry(
    parent: BorrowedFd,
    name: &CStr,
    dev: u64,
    counters: &Counters,
) -> Result<(), Failure> {
    if counters.cancel.load(Relaxed) {
        return Err(Failure::at(name, Reason::Cancelled));
    }
    // Unlinking a directory fails with EISDIR, so files cost one system call
    // and need no stat first. A symlink is removed itself, never followed.
    match rustix::fs::unlinkat(parent, name, AtFlags::empty()) {
        Ok(()) => {
            counters.removed.fetch_add(1, Relaxed);
            Ok(())
        }
        Err(Errno::ISDIR) => remove_dir(parent, name, dev, counters),
        Err(e) => Err(Failure::at(name, Reason::Os(e))),
    }
}

fn remove_dir(
    parent: BorrowedFd,
    name: &CStr,
    dev: u64,
    counters: &Counters,
) -> Result<(), Failure> {
    let fail = |e: Errno| Failure::at(name, Reason::Os(e));
    // Look before opening: opening an automount point would mount it. A
    // bind mount keeps the device number of where it comes from, so the
    // device alone does not show every mount.
    let seen =
        rustix::fs::statx(parent, name, STAT_FLAGS, StatxFlags::BASIC_STATS).map_err(fail)?;
    if dev_of(&seen) != dev || is_mount_root(&seen) {
        return Err(Failure::at(name, Reason::Mount));
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(parent, name, flags, Mode::empty()).map_err(fail)?;
    // And check that what was opened is what was looked at.
    let st = stat_fd(&fd).map_err(fail)?;
    if (dev_of(&st), st.stx_ino) != (dev, seen.stx_ino) || is_mount_root(&st) {
        return Err(Failure::at(name, Reason::Changed));
    }

    // Directories without write permission are common in caches (Go
    // modules, for one). If this one is ours, it is made writable for as
    // long as it takes, and put back as it was if it ends up staying.
    let mode = Mode::from_raw_mode(st.stx_mode as u32);
    let (mut tried, mut unlocked) = (false, false);
    let mut empty_and_remove = || {
        let mut first_failure: Option<Failure> = None;
        // Removing entries while reading a directory can make the reader
        // skip some, so go round again if it turns out not to be empty.
        for _ in 0..4 {
            let mut reader = Dir::read_from(&fd).map_err(fail)?;
            while let Some(entry) = reader.read() {
                let entry = entry.map_err(fail)?;
                let child = entry.file_name();
                if child == c"." || child == c".." {
                    continue;
                }
                let attempt = |counters: &Counters| {
                    if entry.file_type() == FileType::Directory {
                        remove_dir(fd.as_fd(), child, dev, counters)
                    } else {
                        remove_entry(fd.as_fd(), child, dev, counters)
                    }
                };
                let mut result = attempt(counters);
                // EACCES on an entry of this directory is what a missing
                // write permission here gives. EPERM (immutable files, sticky
                // directories) is not something a chmod can help with.
                if let Err(f) = &result
                    && f.reason == Reason::Os(Errno::ACCESS)
                    && f.path.components().count() == 1
                    && !tried
                {
                    tried = true;
                    unlocked = rustix::fs::fchmod(&fd, mode | Mode::RWXU).is_ok();
                    if unlocked {
                        result = attempt(counters);
                    }
                }
                if let Err(f) = result {
                    if f.reason == Reason::Cancelled {
                        return Err(f.inside(name));
                    }
                    first_failure.get_or_insert(f.inside(name));
                }
            }
            match rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR) {
                Ok(()) => {
                    counters.removed.fetch_add(1, Relaxed);
                    return Ok(());
                }
                Err(Errno::NOTEMPTY | Errno::EXIST) if first_failure.is_none() => continue,
                Err(e) => return Err(first_failure.unwrap_or_else(|| fail(e))),
            }
        }
        Err(first_failure.unwrap_or_else(|| Failure::at(name, Reason::KeepsFilling)))
    };
    let result = empty_and_remove();
    if result.is_err() && unlocked {
        let _ = rustix::fs::fchmod(&fd, mode);
    }
    result
}

/// A program that moves files to the desktop trash.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrashTool {
    Gio,
    TrashPut,
}

impl TrashTool {
    /// The first trash helper found on `PATH`, if any.
    pub fn find() -> Option<TrashTool> {
        let path = std::env::var_os("PATH")?;
        let on_path = |name: &str| std::env::split_paths(&path).any(|dir| dir.join(name).is_file());
        if on_path("gio") {
            Some(TrashTool::Gio)
        } else if on_path("trash-put") {
            Some(TrashTool::TrashPut)
        } else {
            None
        }
    }
}

/// Move `path` to the trash. `path` must be absolute.
pub fn trash(path: &Path, tool: TrashTool) -> Result<(), Failure> {
    let mut cmd = match tool {
        TrashTool::Gio => {
            let mut c = Command::new("gio");
            c.arg("trash");
            c
        }
        TrashTool::TrashPut => Command::new("trash-put"),
    };
    let fail = |reason: String| Failure {
        path: path.to_path_buf(),
        reason: Reason::Trash(reason),
    };
    let out = cmd
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| fail(format!("could not run the trash helper: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    // Helpers print "tool: <path or URI>: message"; keep the message.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let reason = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.rsplit(": ").next().unwrap_or(l).to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "could not be moved to the trash".into());
    Err(fail(reason))
}

/// How a batch of items is disposed of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Delete,
    Trash(TrashTool),
}

/// What happened to one item of a deletion.
#[derive(Debug)]
pub enum Outcome {
    Deleted,
    Failed(Failure),
    /// Not attempted, because the job was cancelled first.
    Skipped,
}

/// A deletion running on a background thread.
pub struct DeleteJob {
    pub counters: Arc<Counters>,
    /// Index of the item being worked on.
    pub current: Arc<AtomicUsize>,
    rx: mpsc::Receiver<Vec<Outcome>>,
}

impl DeleteJob {
    /// Start disposing of `items`, in order. Each item is a path and the
    /// number of entries it stands for, which feeds the progress counter when
    /// a whole item goes at once.
    pub fn start(items: Vec<(PathBuf, u64)>, method: Method) -> io::Result<DeleteJob> {
        let counters = Arc::new(Counters::default());
        let current = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let (c, cur) = (counters.clone(), current.clone());
        std::thread::Builder::new()
            .name("plat-delete".into())
            // Recursion depth follows directory depth.
            .stack_size(64 << 20)
            .spawn(move || {
                let outcomes = items
                    .iter()
                    .enumerate()
                    .map(|(i, (path, entries))| {
                        cur.store(i, Relaxed);
                        if c.cancel.load(Relaxed) {
                            return Outcome::Skipped;
                        }
                        let result = match method {
                            Method::Delete => remove(path, &c),
                            Method::Trash(tool) => trash(path, tool).inspect(|()| {
                                c.removed.fetch_add(*entries, Relaxed);
                            }),
                        };
                        match result {
                            Ok(()) => Outcome::Deleted,
                            Err(f) => Outcome::Failed(f),
                        }
                    })
                    .collect();
                let _ = tx.send(outcomes);
            })?;
        Ok(DeleteJob {
            counters,
            current,
            rx,
        })
    }

    /// The outcomes, once the job has finished.
    pub fn try_finish(&self) -> Option<Vec<Outcome>> {
        match self.rx.try_recv() {
            Ok(outcomes) => Some(outcomes),
            Err(mpsc::TryRecvError::Empty) => None,
            // The thread died without reporting; nothing more will arrive.
            Err(mpsc::TryRecvError::Disconnected) => Some(Vec::new()),
        }
    }
}

/// Raise the open-file limit as far as allowed. Deleting holds two
/// descriptors per level of directory depth.
pub fn raise_fd_limit() {
    use rustix::process::{Resource, getrlimit, setrlimit};
    let mut limit = getrlimit(Resource::Nofile);
    if limit.current != limit.maximum {
        limit.current = limit.maximum;
        let _ = setrlimit(Resource::Nofile, limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{mount, scratch, umount};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn removes_a_tree_and_counts_entries() {
        let root = scratch("tree");
        let victim = root.join("victim");
        fs::create_dir_all(victim.join("a/b/c")).unwrap();
        fs::write(victim.join("a/one"), b"1").unwrap();
        fs::write(victim.join("a/b/two"), b"22").unwrap();
        fs::write(victim.join("a/b/c/three"), b"333").unwrap();
        fs::write(root.join("keep"), b"keep").unwrap();

        let counters = Counters::default();
        remove(&victim, &counters).unwrap();
        assert!(!victim.exists());
        assert!(root.join("keep").exists());
        // victim, a, b, c and three files.
        assert_eq!(counters.removed.load(Relaxed), 7);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removes_a_single_file() {
        let root = scratch("file");
        let file = root.join("only");
        fs::write(&file, b"x").unwrap();
        remove(&file, &Counters::default()).unwrap();
        assert!(!file.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn never_follows_symlinks() {
        let root = scratch("links");
        let outside = root.join("outside");
        fs::create_dir_all(outside.join("deep")).unwrap();
        fs::write(outside.join("sentinel"), b"alive").unwrap();
        fs::write(outside.join("deep/sentinel"), b"alive").unwrap();

        let victim = root.join("victim");
        fs::create_dir_all(victim.join("sub")).unwrap();
        symlink(&outside, victim.join("link-to-dir")).unwrap();
        symlink(outside.join("sentinel"), victim.join("sub/link-to-file")).unwrap();
        symlink("../../outside", victim.join("sub/relative-link")).unwrap();

        remove(&victim, &Counters::default()).unwrap();
        assert!(!victim.exists());
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"alive");
        assert_eq!(fs::read(outside.join("deep/sentinel")).unwrap(), b"alive");

        // A symlink as the item itself is removed, its target untouched.
        let link = root.join("top-link");
        symlink(&outside, &link).unwrap();
        remove(&link, &Counters::default()).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(outside.join("sentinel").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refuses_a_path_through_a_symlink() {
        let root = scratch("via-link");
        let real = root.join("real");
        fs::create_dir_all(real.join("inner")).unwrap();
        fs::write(real.join("inner/file"), b"x").unwrap();
        symlink(&real, root.join("alias")).unwrap();

        // Same file, but reached through a symlinked directory.
        let err = remove(&root.join("alias/inner"), &Counters::default()).unwrap_err();
        assert_eq!(err.reason.to_string(), "changed since the scan");
        assert!(real.join("inner/file").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn permissions_are_put_back_when_a_directory_stays() {
        if crate::fsinfo::is_root() {
            return; // root opens anything, so nothing would stay
        }
        // `locked` can only be emptied after a chmod, and still cannot be,
        // because `sealed` inside it cannot even be opened.
        let root = scratch("restore");
        let locked = root.join("victim/locked");
        fs::create_dir_all(locked.join("sealed")).unwrap();
        fs::write(locked.join("sealed/file"), b"x").unwrap();
        let set = |path: &Path, mode| fs::set_permissions(path, fs::Permissions::from_mode(mode));
        set(&locked.join("sealed"), 0o000).unwrap();
        set(&locked, 0o555).unwrap();

        let err = remove(&root.join("victim"), &Counters::default()).unwrap_err();
        assert_eq!(err.path, locked.join("sealed"));
        let mode = fs::metadata(&locked).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o555, "left at {mode:o}");

        set(&locked.join("sealed"), 0o755).unwrap();
        set(&locked, 0o755).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn clears_read_only_directories() {
        let root = scratch("readonly");
        let victim = root.join("victim");
        fs::create_dir_all(victim.join("locked/deeper")).unwrap();
        fs::write(victim.join("locked/file"), b"x").unwrap();
        fs::write(victim.join("locked/deeper/file"), b"x").unwrap();
        for d in ["locked/deeper", "locked"] {
            fs::set_permissions(victim.join(d), fs::Permissions::from_mode(0o555)).unwrap();
        }
        remove(&victim, &Counters::default()).unwrap();
        assert!(!victim.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Needs a private mount namespace:
    /// `unshare -Urm <test binary> --ignored mount`.
    #[test]
    #[ignore = "run under `unshare -Urm`"]
    fn refuses_to_cross_a_mount() {
        let root = scratch("mount");
        let victim = root.join("victim");
        let mnt = victim.join("mnt");
        fs::create_dir_all(victim.join("plain")).unwrap();
        fs::create_dir_all(&mnt).unwrap();
        fs::write(victim.join("plain/file"), b"x").unwrap();
        mount(&["-t", "tmpfs", "none"], &mnt);
        fs::create_dir_all(mnt.join("inner")).unwrap();
        fs::write(mnt.join("inner/sentinel"), b"alive").unwrap();

        // A directory that contains a mount: the mount is left alone, the
        // rest is removed.
        let err = remove(&victim, &Counters::default()).unwrap_err();
        assert_eq!(err.path, mnt);
        assert_eq!(err.reason, Reason::Mount);
        assert_eq!(fs::read(mnt.join("inner/sentinel")).unwrap(), b"alive");
        assert!(!victim.join("plain").exists());

        // The mount point itself as the item.
        let err = remove(&mnt, &Counters::default()).unwrap_err();
        assert_eq!(err.reason, Reason::Mount);
        assert_eq!(fs::read(mnt.join("inner/sentinel")).unwrap(), b"alive");

        umount(&mnt);
        fs::remove_dir_all(&root).unwrap();
    }

    /// A bind mount has the device number of the filesystem it comes from, so
    /// it passes for an ordinary directory unless the kernel is asked.
    /// Needs a private mount namespace, like the test above.
    #[test]
    #[ignore = "run under `unshare -Urm`"]
    fn refuses_to_enter_a_bind_mount() {
        let root = scratch("bind");
        let real = root.join("real");
        fs::create_dir_all(real.join("inner")).unwrap();
        fs::write(real.join("inner/sentinel"), b"alive").unwrap();
        let victim = root.join("victim");
        let alias = victim.join("alias");
        fs::create_dir_all(&alias).unwrap();
        fs::write(victim.join("plain"), b"x").unwrap();
        mount(&["--bind", real.to_str().unwrap()], &alias);

        // Inside a doomed directory: the rest goes, the bind mount stays.
        let err = remove(&victim, &Counters::default()).unwrap_err();
        assert_eq!(err.path, alias);
        assert_eq!(fs::read(real.join("inner/sentinel")).unwrap(), b"alive");
        assert!(!victim.join("plain").exists());

        // The bind mount itself as the item.
        let err = remove(&alias, &Counters::default()).unwrap_err();
        assert!(
            err.reason.to_string().contains("mount point"),
            "{}",
            err.reason
        );
        assert_eq!(fs::read(real.join("inner/sentinel")).unwrap(), b"alive");

        umount(&alias);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Puts one uniquely named file in the real trash and takes it out
    /// again, so it only runs on request. `PLAT_TRASH_PROBE_DIR` must name a
    /// directory on a filesystem that supports trashing.
    #[test]
    #[ignore = "touches the desktop trash"]
    fn trash_round_trip() {
        let Some(tool) = TrashTool::find() else {
            eprintln!("no trash helper installed; nothing to test");
            return;
        };
        let dir = PathBuf::from(std::env::var_os("PLAT_TRASH_PROBE_DIR").expect("probe dir"));
        let name = format!("plat-trash-probe-{}", std::process::id());
        let file = dir.join(&name);
        fs::write(&file, b"probe").unwrap();
        trash(&file, tool).unwrap();
        assert!(!file.exists());

        // Find it in the home trash and remove just that entry.
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let trash_dir = home.join(".local/share/Trash");
        let trashed = trash_dir.join("files").join(&name);
        assert_eq!(fs::read(&trashed).unwrap(), b"probe");
        fs::remove_file(&trashed).unwrap();
        fs::remove_file(trash_dir.join("info").join(format!("{name}.trashinfo"))).unwrap();

        // A path that cannot be trashed reports why.
        let err = trash(&dir.join("does-not-exist"), tool).unwrap_err();
        assert!(
            !err.reason.to_string().is_empty() && !err.reason.to_string().contains("file://"),
            "{}",
            err.reason
        );
    }

    #[test]
    fn reports_the_entry_that_failed() {
        let root = scratch("missing");
        let err = remove(&root.join("nope"), &Counters::default()).unwrap_err();
        assert_eq!(err.path, root.join("nope"));
        assert_eq!(err.reason, Reason::Os(Errno::NOENT));
        assert_eq!(err.reason.to_string(), "no longer exists");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cancel_stops_before_touching_anything() {
        let root = scratch("cancel");
        let victim = root.join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("file"), b"x").unwrap();
        let counters = Counters::default();
        counters.cancel.store(true, Relaxed);
        let err = remove(&victim, &counters).unwrap_err();
        assert_eq!(err.reason, Reason::Cancelled);
        assert!(victim.join("file").exists());
        fs::remove_dir_all(&root).unwrap();
    }
}
