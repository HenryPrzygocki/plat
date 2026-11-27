// SPDX-License-Identifier: GPL-3.0-or-later
//! Filesystem identification, used to decide which mounts a scan descends into.

use std::os::fd::AsFd;
use std::path::Path;

/// What kind of filesystem a mount point leads to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsClass {
    /// Backed by real storage. Scanned by default.
    Disk,
    /// Kernel interfaces and RAM: procfs, sysfs, tmpfs and friends.
    Virtual,
    /// Network and FUSE filesystems, which can be slow or remote.
    Remote,
    /// Read-only images and overlays whose contents are already counted elsewhere.
    Image,
}

/// Names of filesystem types, indexed by the id stored on skipped mount nodes.
const NAMES: &[&str] = &[
    "other fs",
    "proc",
    "sysfs",
    "tmpfs",
    "devpts",
    "cgroup",
    "debugfs",
    "tracefs",
    "securityfs",
    "pstore",
    "bpf",
    "configfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "efivarfs",
    "binfmt_misc",
    "autofs",
    "rpc_pipefs",
    "nsfs",
    "selinuxfs",
    "ramfs",
    "overlay",
    "squashfs",
    "iso9660",
    "udf",
    "erofs",
    "nfs",
    "smb",
    "cifs",
    "fuse",
    "9p",
    "ceph",
    "ext",
    "xfs",
    "btrfs",
    "vfat",
    "exfat",
    "ntfs",
    "f2fs",
    "zfs",
    "bcachefs",
];

/// Classify a filesystem by its `statfs` magic number. Returns the class and an
/// index into the name table.
fn classify(magic: u64) -> (FsClass, u16) {
    use FsClass::*;
    let (class, name) = match magic {
        0x9fa0 => (Virtual, "proc"),
        0x6265_6572 => (Virtual, "sysfs"),
        0x0102_1994 => (Virtual, "tmpfs"),
        0x1cd1 => (Virtual, "devpts"),
        0x0027_e0eb | 0x6367_7270 => (Virtual, "cgroup"),
        0x6462_6720 => (Virtual, "debugfs"),
        0x7472_6163 => (Virtual, "tracefs"),
        0x7363_6673 => (Virtual, "securityfs"),
        0x6165_676c => (Virtual, "pstore"),
        0xcafe_4a11 => (Virtual, "bpf"),
        0x6265_6570 => (Virtual, "configfs"),
        0x6573_5543 => (Virtual, "fusectl"),
        0x9584_58f6 => (Virtual, "hugetlbfs"),
        0x1980_0202 => (Virtual, "mqueue"),
        0xde5e_81e4 => (Virtual, "efivarfs"),
        0x4249_4e4d => (Virtual, "binfmt_misc"),
        0x0187 => (Virtual, "autofs"),
        0x6759_6969 => (Virtual, "rpc_pipefs"),
        0x6e73_6673 => (Virtual, "nsfs"),
        0xf97c_ff8c => (Virtual, "selinuxfs"),
        0x8584_58f6 => (Virtual, "ramfs"),
        0x794c_7630 => (Image, "overlay"),
        0x7371_7368 => (Image, "squashfs"),
        0x9660 => (Image, "iso9660"),
        0x1501_3346 => (Image, "udf"),
        0xe0f5_e1e2 => (Image, "erofs"),
        0x6969 => (Remote, "nfs"),
        0x517b | 0xfe53_4d42 => (Remote, "smb"),
        0xff53_4d42 => (Remote, "cifs"),
        0x6573_5546 => (Remote, "fuse"),
        0x0102_1997 => (Remote, "9p"),
        0x00c3_6400 => (Remote, "ceph"),
        0xef53 => (Disk, "ext"),
        0x5846_5342 => (Disk, "xfs"),
        0x9123_683e => (Disk, "btrfs"),
        0x4d44 => (Disk, "vfat"),
        0x2011_bab0 => (Disk, "exfat"),
        0x5346_544e | 0x7366_746e => (Disk, "ntfs"),
        0xf2f5_2010 => (Disk, "f2fs"),
        0x2fc1_2fc1 => (Disk, "zfs"),
        0xca45_1a4e => (Disk, "bcachefs"),
        _ => (Disk, "other fs"),
    };
    let id = NAMES.iter().position(|&n| n == name).unwrap_or(0) as u16;
    (class, id)
}

/// Classify the filesystem an open descriptor is on. Unknown or unreadable
/// filesystems are treated as real disks so that nothing is hidden by mistake.
pub fn classify_fd(fd: impl AsFd) -> (FsClass, u16) {
    match rustix::fs::fstatfs(fd) {
        Ok(st) => classify(st.f_type as u64 & 0xffff_ffff),
        Err(_) => (FsClass::Disk, 0),
    }
}

/// Name of a filesystem type id as returned by [`classify_fd`].
pub fn fs_name(id: u16) -> &'static str {
    NAMES.get(id as usize).copied().unwrap_or(NAMES[0])
}

/// Capacity of the filesystem holding `path`: (total bytes, bytes available to
/// the current user).
pub fn capacity(path: &Path) -> Option<(u64, u64)> {
    let st = rustix::fs::statvfs(path).ok()?;
    let total = st.f_blocks.saturating_mul(st.f_frsize);
    let avail = st.f_bavail.saturating_mul(st.f_frsize);
    (total > 0).then_some((total, avail))
}

/// True when running with root privileges.
pub fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_resolves() {
        assert_eq!(classify(0x9fa0), (FsClass::Virtual, 1));
        assert_eq!(fs_name(classify(0x0102_1994).1), "tmpfs");
        assert_eq!(fs_name(classify(0x6969).1), "nfs");
        assert_eq!(fs_name(classify(0x5846_5342).1), "xfs");
        assert_eq!(classify(0x5846_5342).0, FsClass::Disk);
        assert_eq!(classify(0x1234_5678), (FsClass::Disk, 0));
        let proc = std::fs::File::open("/proc").unwrap();
        assert_eq!(classify_fd(&proc), (FsClass::Virtual, 1));
    }
}
