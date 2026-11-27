// SPDX-License-Identifier: GPL-3.0-or-later
//! Filesystem identification, used to decide which mounts a scan descends into.

use std::os::fd::AsFd;
use std::path::Path;

use rustix::fs::{Statx, StatxAttributes};

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

/// Filesystem types: display name, class, and the `statfs` magic numbers that
/// identify it. An id stored on a node is an index into this table.
const TYPES: &[(&str, FsClass, &[u64])] = &[
    // Fixed ids first: see the constants below.
    ("other fs", FsClass::Disk, &[]),
    ("bind", FsClass::Image, &[]),
    ("duplicate", FsClass::Image, &[]),
    ("proc", FsClass::Virtual, &[0x9fa0]),
    ("sysfs", FsClass::Virtual, &[0x6265_6572]),
    ("tmpfs", FsClass::Virtual, &[0x0102_1994]),
    ("devpts", FsClass::Virtual, &[0x1cd1]),
    ("cgroup", FsClass::Virtual, &[0x0027_e0eb, 0x6367_7270]),
    ("debugfs", FsClass::Virtual, &[0x6462_6720]),
    ("tracefs", FsClass::Virtual, &[0x7472_6163]),
    ("securityfs", FsClass::Virtual, &[0x7363_6673]),
    ("pstore", FsClass::Virtual, &[0x6165_676c]),
    ("bpf", FsClass::Virtual, &[0xcafe_4a11]),
    ("configfs", FsClass::Virtual, &[0x6265_6570]),
    ("fusectl", FsClass::Virtual, &[0x6573_5543]),
    ("hugetlbfs", FsClass::Virtual, &[0x9584_58f6]),
    ("mqueue", FsClass::Virtual, &[0x1980_0202]),
    ("efivarfs", FsClass::Virtual, &[0xde5e_81e4]),
    ("binfmt_misc", FsClass::Virtual, &[0x4249_4e4d]),
    ("autofs", FsClass::Virtual, &[0x0187]),
    ("rpc_pipefs", FsClass::Virtual, &[0x6759_6969]),
    ("nsfs", FsClass::Virtual, &[0x6e73_6673]),
    ("selinuxfs", FsClass::Virtual, &[0xf97c_ff8c]),
    ("ramfs", FsClass::Virtual, &[0x8584_58f6]),
    ("overlay", FsClass::Image, &[0x794c_7630]),
    ("squashfs", FsClass::Image, &[0x7371_7368]),
    ("iso9660", FsClass::Image, &[0x9660]),
    ("udf", FsClass::Image, &[0x1501_3346]),
    ("erofs", FsClass::Image, &[0xe0f5_e1e2]),
    ("nfs", FsClass::Remote, &[0x6969]),
    ("smb", FsClass::Remote, &[0x517b, 0xfe53_4d42]),
    ("cifs", FsClass::Remote, &[0xff53_4d42]),
    ("fuse", FsClass::Remote, &[0x6573_5546]),
    ("9p", FsClass::Remote, &[0x0102_1997]),
    ("ceph", FsClass::Remote, &[0x00c3_6400]),
    ("ext", FsClass::Disk, &[0xef53]),
    ("xfs", FsClass::Disk, &[0x5846_5342]),
    ("btrfs", FsClass::Disk, &[0x9123_683e]),
    ("vfat", FsClass::Disk, &[0x4d44]),
    ("exfat", FsClass::Disk, &[0x2011_bab0]),
    ("ntfs", FsClass::Disk, &[0x5346_544e, 0x7366_746e]),
    ("f2fs", FsClass::Disk, &[0xf2f5_2010]),
    ("zfs", FsClass::Disk, &[0x2fc1_2fc1]),
    ("bcachefs", FsClass::Disk, &[0xca45_1a4e]),
];

/// A filesystem the table does not know. Treated as a real disk.
pub const OTHER: u16 = 0;
/// A mount whose device number is that of its surroundings: a directory of
/// the same filesystem made visible at a second place.
pub const BIND: u16 = 1;
/// A mount of something the scan already covers at another place.
pub const DUPLICATE: u16 = 2;

/// Classify a filesystem by its `statfs` magic number. Returns the class and
/// an id for [`fs_name`]. Unknown types are treated as real disks, so that
/// nothing is hidden by mistake.
fn classify(magic: u64) -> (FsClass, u16) {
    TYPES
        .iter()
        .position(|(_, _, magics)| magics.contains(&magic))
        .map_or((FsClass::Disk, OTHER), |i| (TYPES[i].1, i as u16))
}

/// Classify the filesystem an open descriptor is on. Unknown or unreadable
/// filesystems are treated as real disks so that nothing is hidden by mistake.
pub fn classify_fd(fd: impl AsFd) -> (FsClass, u16) {
    match rustix::fs::fstatfs(fd) {
        Ok(st) => classify(st.f_type as u64 & 0xffff_ffff),
        Err(_) => (FsClass::Disk, OTHER),
    }
}

/// Name of a filesystem type id as returned by [`classify_fd`].
pub fn fs_name(id: u16) -> &'static str {
    TYPES.get(id as usize).unwrap_or(&TYPES[OTHER as usize]).0
}

/// One device number from the two halves `statx` reports. Everything that
/// compares devices uses this, so the encoding never has to match `st_dev`.
pub fn dev_of(stx: &Statx) -> u64 {
    (stx.stx_dev_major as u64) << 32 | stx.stx_dev_minor as u64
}

/// True when the entry is the root of a mount. This is what tells a bind
/// mount apart: it has the device number of the filesystem it comes from.
/// Kernels before 5.8 do not report the attribute; there only a change of
/// device marks a mount.
pub fn is_mount_root(stx: &Statx) -> bool {
    stx.stx_attributes_mask
        .contains(StatxAttributes::MOUNT_ROOT)
        && stx.stx_attributes.contains(StatxAttributes::MOUNT_ROOT)
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
        assert_eq!(fs_name(classify(0x9fa0).1), "proc");
        assert_eq!(classify(0x9fa0).0, FsClass::Virtual);
        assert_eq!(fs_name(classify(0x0102_1994).1), "tmpfs");
        assert_eq!(fs_name(classify(0xfe53_4d42).1), "smb");
        assert_eq!(fs_name(classify(0x6969).1), "nfs");
        assert_eq!(fs_name(classify(0x5846_5342).1), "xfs");
        assert_eq!(classify(0x5846_5342).0, FsClass::Disk);
        assert_eq!(classify(0x1234_5678), (FsClass::Disk, OTHER));
        assert_eq!(fs_name(OTHER), "other fs");
        assert_eq!(fs_name(BIND), "bind");
        assert_eq!(fs_name(DUPLICATE), "duplicate");
        assert_eq!(fs_name(u16::MAX), "other fs");
        let proc = std::fs::File::open("/proc").unwrap();
        assert_eq!(fs_name(classify_fd(&proc).1), "proc");
        // No magic number is claimed twice.
        let mut all: Vec<u64> = TYPES.iter().flat_map(|t| t.2.iter().copied()).collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n);
    }
}
