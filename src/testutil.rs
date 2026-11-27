// SPDX-License-Identifier: GPL-3.0-or-later
//! Helpers shared by the tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// A fresh, empty directory under the system temp dir, in canonical form as a
/// scan reports paths. Every call gets a directory of its own, so tests that
/// run in parallel never share one, whatever `tag` they pass.
pub fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Relaxed);
    let dir = std::env::temp_dir().join(format!("plat-test-{}-{n}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A scratch directory holding files of the given sizes: (relative path, bytes).
pub fn with_files(tag: &str, files: &[(&str, usize)]) -> PathBuf {
    let root = scratch(tag);
    for (rel, size) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; *size]).unwrap();
    }
    root
}

/// `mount ARGS... TARGET`. Only works in a private mount namespace, which is
/// why the tests that call it are ignored unless run under `unshare -Urm`.
pub fn mount(args: &[&str], target: &Path) {
    let ok = Command::new("mount")
        .args(args)
        .arg(target)
        .status()
        .unwrap()
        .success();
    assert!(
        ok,
        "could not mount on {}; is this inside `unshare -Urm`?",
        target.display()
    );
}

pub fn umount(target: &Path) {
    let ok = Command::new("umount").arg(target).status().unwrap().success();
    assert!(ok, "could not unmount {}", target.display());
}
