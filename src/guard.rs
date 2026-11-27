// SPDX-License-Identifier: GPL-3.0-or-later
//! Guard rails for deletion.
//!
//! Nothing here forbids anything. It recognises targets whose loss is hard or
//! impossible to undo, says in plain words what they are, and tells the
//! dialog how much deliberate effort to ask for before going ahead.

use std::path::{Path, PathBuf};

/// How much damage deleting something would do.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    /// Probably fine, but worth knowing what it is first.
    Caution,
    /// Breaks the system or destroys something irreplaceable.
    Critical,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Risk {
    pub level: Level,
    /// A word or two for tight spaces, such as a list column.
    pub label: &'static str,
    /// One sentence saying what the thing is and why it matters.
    pub reason: String,
}

impl Risk {
    fn new(level: Level, label: &'static str, reason: impl Into<String>) -> Option<Risk> {
        Some(Risk {
            level,
            label,
            reason: reason.into(),
        })
    }
}

/// A path that must not go without a typed confirmation.
struct Protected {
    path: PathBuf,
    label: &'static str,
    reason: String,
    /// Whether everything inside is protected as well, not just the path.
    deep: bool,
}

pub struct Guard {
    protected: Vec<Protected>,
    /// Home directories, with the name of the user each belongs to.
    homes: Vec<(PathBuf, String)>,
    /// Whether a protected path is actually there. A directory is only
    /// warned about for holding something that exists.
    exists: fn(&Path) -> bool,
}

/// Paths the running system depends on: (path, label, reason, deep).
const SYSTEM: &[(&str, &str, &str, bool)] = &[
    (
        "/usr",
        "system",
        "System programs and libraries. Almost nothing runs without them.",
        false,
    ),
    (
        "/usr/bin",
        "system",
        "The programs of the system, including the shell and the package manager.",
        false,
    ),
    (
        "/usr/sbin",
        "system",
        "System administration programs.",
        false,
    ),
    (
        "/usr/lib",
        "system",
        "Shared libraries that every program loads.",
        false,
    ),
    (
        "/usr/lib32",
        "system",
        "Shared libraries for 32-bit programs.",
        false,
    ),
    (
        "/usr/lib64",
        "system",
        "Shared libraries that every program loads.",
        false,
    ),
    (
        "/usr/libexec",
        "system",
        "Helper programs that system services run.",
        false,
    ),
    (
        "/usr/share",
        "system",
        "Data files that installed programs need to run.",
        false,
    ),
    (
        "/usr/lib/modules",
        "system",
        "Kernel modules (drivers). The system needs the set matching its kernel to boot.",
        false,
    ),
    (
        "/usr/lib/systemd",
        "system",
        "systemd, which starts and supervises everything else on the system.",
        false,
    ),
    (
        "/bin",
        "system",
        "Where the system looks for its programs.",
        false,
    ),
    (
        "/sbin",
        "system",
        "Where the system looks for its programs.",
        false,
    ),
    (
        "/lib",
        "system",
        "Where programs look for the system's libraries.",
        false,
    ),
    (
        "/lib64",
        "system",
        "Where programs look for the system's libraries.",
        false,
    ),
    (
        "/etc",
        "system",
        "System-wide configuration: users, passwords, mounts, services.",
        false,
    ),
    (
        "/etc/passwd",
        "system",
        "The list of user accounts. Nobody can log in without it.",
        false,
    ),
    (
        "/etc/shadow",
        "system",
        "The passwords of all user accounts.",
        false,
    ),
    ("/etc/group", "system", "The list of user groups.", false),
    (
        "/etc/fstab",
        "system",
        "The table of filesystems mounted at boot.",
        false,
    ),
    (
        "/etc/sudoers",
        "system",
        "The rules for who may use sudo.",
        false,
    ),
    (
        "/boot",
        "boot",
        "Kernel and boot loader. The machine does not start without them.",
        true,
    ),
    (
        "/efi",
        "boot",
        "The EFI system partition. The machine does not start without it.",
        true,
    ),
    (
        "/var",
        "system",
        "System data that changes: package database, service state, logs.",
        false,
    ),
    (
        "/var/lib",
        "system",
        "The state of installed services and of the package manager.",
        false,
    ),
    (
        "/var/lib/pacman",
        "packages",
        "The package manager's record of everything installed.",
        true,
    ),
    (
        "/var/lib/dpkg",
        "packages",
        "The package manager's record of everything installed.",
        true,
    ),
    (
        "/var/lib/rpm",
        "packages",
        "The package manager's record of everything installed.",
        true,
    ),
    (
        "/home",
        "home",
        "The home directories of every user on this machine.",
        false,
    ),
    (
        "/root",
        "home",
        "The home directory of the root user.",
        false,
    ),
    (
        "/nix",
        "system",
        "The Nix store, which holds every installed package.",
        false,
    ),
];

/// Things in a home directory that cannot be recreated: (relative path, reason).
const SECRETS: &[(&str, &str)] = &[
    (
        ".ssh",
        "SSH keys and trusted hosts. A deleted private key cannot be recovered.",
    ),
    (
        ".gnupg",
        "GPG keys. A deleted private key cannot be recovered.",
    ),
    (
        ".password-store",
        "A password store. The passwords in it exist nowhere else.",
    ),
    (
        ".local/share/keyrings",
        "Passwords and secrets saved by desktop applications.",
    ),
];

/// Hidden entries in a home directory that only hold things that can be
/// fetched or rebuilt again.
const DISPOSABLE: &[&str] = &[".cache", ".thumbnails", ".Trash"];

impl Guard {
    /// Rules for this machine: the built-in system paths plus the home
    /// directories of its users.
    pub fn system() -> Guard {
        let mut homes = std::fs::read_to_string("/etc/passwd")
            .map(|text| parse_passwd(&text))
            .unwrap_or_default();
        // An account that is not in the local file (LDAP, systemd-homed).
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
            && home.is_absolute()
            && home != Path::new("/")
            && !homes.iter().any(|(h, _)| *h == home)
        {
            homes.push((home, "the current user".to_string()));
        }
        Guard::new(homes)
    }

    /// Rules with an explicit set of home directories.
    pub fn new(homes: Vec<(PathBuf, String)>) -> Guard {
        let mut protected: Vec<Protected> = SYSTEM
            .iter()
            .map(|&(path, label, reason, deep)| Protected {
                path: PathBuf::from(path),
                label,
                reason: reason.to_string(),
                deep,
            })
            .collect();
        for (home, user) in &homes {
            protected.push(Protected {
                path: home.clone(),
                label: "home",
                reason: format!("Home directory of {user}: everything they keep on this machine."),
                deep: false,
            });
            for &(rel, reason) in SECRETS {
                protected.push(Protected {
                    path: home.join(rel),
                    label: "keys",
                    reason: reason.to_string(),
                    deep: true,
                });
            }
        }
        Guard {
            protected,
            homes,
            exists: |path| path.symlink_metadata().is_ok(),
        }
    }

    /// Treat every protected path as present, whatever is on disk. For tests
    /// that reason about paths which do not exist on the machine running them.
    #[cfg(test)]
    pub fn assume_present(mut self) -> Guard {
        self.exists = |_| true;
        self
    }

    /// What deleting `path` would put at risk, if anything.
    ///
    /// `path` must be absolute and free of symlinks. `has_git` says whether
    /// the directory holds a `.git` entry of its own.
    pub fn assess(&self, path: &Path, is_dir: bool, has_git: bool) -> Option<Risk> {
        // The thing itself, or something inside a fully protected directory.
        // `Path::starts_with` compares whole components, so `/usr2` is not
        // `/usr` and `/mnt/old/usr` is not the running system.
        for p in &self.protected {
            if path == p.path {
                return Risk::new(Level::Critical, p.label, p.reason.clone());
            }
            if p.deep && path.starts_with(&p.path) {
                return Risk::new(
                    Level::Critical,
                    p.label,
                    format!("Part of {}. {}", p.path.display(), p.reason),
                );
            }
        }
        // A directory that would take a protected path down with it.
        if is_dir
            && let Some(p) = self
                .protected
                .iter()
                .find(|p| p.path.starts_with(path) && (self.exists)(&p.path))
        {
            // Named relative to the directory, which keeps the line short.
            let inside = p.path.strip_prefix(path).unwrap_or(&p.path);
            return Risk::new(
                Level::Critical,
                p.label,
                format!("Holds {}. {}", inside.display(), p.reason),
            );
        }
        if path.parent() == Some(Path::new("/home")) {
            return Risk::new(
                Level::Critical,
                "home",
                "Looks like a user's home directory.",
            );
        }

        for (home, user) in &self.homes {
            let Ok(rel) = path.strip_prefix(home) else {
                continue;
            };
            let mut parts = rel.components();
            let (Some(first), None) = (parts.next(), parts.next()) else {
                // Deeper than the top level of the home directory.
                if rel == Path::new(".local/share") || rel == Path::new(".local/state") {
                    return Risk::new(
                        Level::Caution,
                        "app data",
                        format!("Data that {user}'s applications keep between runs."),
                    );
                }
                break;
            };
            let name = first.as_os_str().to_string_lossy();
            if DISPOSABLE.contains(&name.as_ref()) {
                return None;
            }
            return if name.starts_with('.') {
                Risk::new(
                    Level::Caution,
                    "settings",
                    format!("Hidden settings or data of an application in {user}'s home."),
                )
            } else if is_dir {
                Risk::new(
                    Level::Caution,
                    "home folder",
                    format!("A top-level folder of {user}'s home directory."),
                )
            } else {
                None
            };
        }

        // Parts of the system that are not fatal one by one, but are not
        // spare space either.
        let under = |prefix: &str| path.starts_with(prefix);
        let child_of = |dir: &str| path.parent() == Some(Path::new(dir));
        if path == Path::new("/usr/local") || path == Path::new("/opt") || path == Path::new("/srv")
        {
            return Risk::new(
                Level::Caution,
                "system",
                "Software or data installed on this machine by hand.",
            );
        }
        if under("/usr") && !under("/usr/local") {
            return Risk::new(
                Level::Caution,
                "packaged",
                "Installed by the package manager. Remove the package instead, or what depends on this breaks.",
            );
        }
        if under("/etc") {
            return Risk::new(
                Level::Caution,
                "config",
                "Part of the system's configuration.",
            );
        }
        if child_of("/var/lib") {
            return Risk::new(
                Level::Caution,
                "service data",
                "The stored state of a system service: databases, containers, virtual machines.",
            );
        }
        if child_of("/var") {
            return Risk::new(
                Level::Caution,
                "system",
                "Services expect this directory to exist. Delete what is inside it instead.",
            );
        }
        if child_of("/opt") {
            return Risk::new(
                Level::Caution,
                "software",
                "A program installed system-wide outside the package manager.",
            );
        }

        if path.file_name().is_some_and(|n| n == ".git") {
            return Risk::new(
                Level::Caution,
                "git data",
                "The history of a git repository. Commits that were never pushed exist only here.",
            );
        }
        if has_git {
            return Risk::new(
                Level::Caution,
                "git repo",
                "A git repository. Commits and stashes that were never pushed exist only here.",
            );
        }
        None
    }
}

/// Home directories of the accounts people log in to: root and regular users.
fn parse_passwd(text: &str) -> Vec<(PathBuf, String)> {
    let mut homes: Vec<(PathBuf, String)> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let (Some(&name), Some(uid), Some(&home)) = (
            fields.first(),
            fields.get(2).and_then(|u| u.parse::<u32>().ok()),
            fields.get(5),
        ) else {
            continue;
        };
        // System accounts live in places like /var/lib/x or /; their "homes"
        // are covered by the system rules or are not homes at all.
        let regular = uid == 0 || (1000..65534).contains(&uid);
        let home = PathBuf::from(home);
        if regular
            && home.is_absolute()
            && home != Path::new("/")
            && !homes.iter().any(|(h, _)| *h == home)
        {
            homes.push((home, name.to_string()));
        }
    }
    homes
}

#[cfg(test)]
mod tests {
    use super::*;
    use Level::{Caution, Critical};

    fn guard() -> Guard {
        Guard::new(vec![
            (PathBuf::from("/home/alice"), "alice".into()),
            (PathBuf::from("/data/users/bob"), "bob".into()),
        ])
        .assume_present()
    }

    #[test]
    fn a_directory_is_only_blamed_for_what_it_really_holds() {
        // No keyring exists under this made-up home, so its .local is an
        // ordinary hidden directory rather than a protected one.
        let g = Guard::new(vec![(PathBuf::from("/nonexistent/home/zoe"), "zoe".into())]);
        let local = g.assess(Path::new("/nonexistent/home/zoe/.local"), true, false);
        assert_eq!(local.map(|r| r.level), Some(Caution));
        // The home itself is protected by name, present or not.
        let home = g.assess(Path::new("/nonexistent/home/zoe"), true, false);
        assert_eq!(home.map(|r| r.level), Some(Critical));
        let around = g.assess(Path::new("/nonexistent/home"), true, false);
        assert!(around.is_none(), "nothing real is inside");
    }

    fn level(path: &str, is_dir: bool) -> Option<Level> {
        guard()
            .assess(Path::new(path), is_dir, false)
            .map(|r| r.level)
    }

    #[test]
    fn classifies_paths() {
        let cases: &[(&str, bool, Option<Level>)] = &[
            // The running system.
            ("/usr", true, Some(Critical)),
            ("/usr/lib", true, Some(Critical)),
            ("/bin", false, Some(Critical)),
            ("/etc", true, Some(Critical)),
            ("/etc/fstab", false, Some(Critical)),
            ("/var/lib/pacman", true, Some(Critical)),
            ("/var/lib/pacman/local/zlib-1.3", true, Some(Critical)),
            ("/boot", true, Some(Critical)),
            ("/boot/vmlinuz-linux", false, Some(Critical)),
            ("/boot/EFI/BOOT/BOOTX64.EFI", false, Some(Critical)),
            // Not fatal, but not spare space.
            ("/usr/lib/python3.14", true, Some(Caution)),
            ("/usr/share/doc", true, Some(Caution)),
            ("/etc/pacman.conf", false, Some(Caution)),
            ("/var/lib/docker", true, Some(Caution)),
            ("/var/log", true, Some(Caution)),
            ("/opt/matlab", true, Some(Caution)),
            ("/usr/local", true, Some(Caution)),
            // Ordinary cleanup targets inside the system.
            ("/var/log/journal", true, None),
            ("/var/cache/pacman/pkg/zlib.pkg.tar.zst", false, None),
            ("/var/lib/docker/overlay2/abc", true, None),
            ("/usr/local/games", true, None),
            // Look-alikes: components are compared, not text.
            ("/usr2", true, None),
            ("/mnt/old/usr", true, None),
            ("/mnt/old/etc/fstab", false, None),
            ("/bootleg", true, None),
            // Homes.
            ("/home", true, Some(Critical)),
            ("/home/alice", true, Some(Critical)),
            ("/home/carol", true, Some(Critical)),
            ("/data/users/bob", true, Some(Critical)),
            ("/data/users", true, Some(Critical)),
            ("/data", true, Some(Critical)),
            ("/root", true, Some(Critical)),
            // Secrets, and anything inside or around them.
            ("/home/alice/.ssh", true, Some(Critical)),
            ("/home/alice/.ssh/id_ed25519", false, Some(Critical)),
            ("/home/alice/.gnupg/private-keys-v1.d", true, Some(Critical)),
            ("/home/alice/.local/share/keyrings", true, Some(Critical)),
            ("/home/alice/.local", true, Some(Critical)),
            ("/home/alice/.local/share", true, Some(Critical)),
            ("/data/users/bob/.ssh", true, Some(Critical)),
            // The top level of a home.
            ("/home/alice/.config", true, Some(Caution)),
            ("/home/alice/.cargo", true, Some(Caution)),
            ("/home/alice/.zshrc", false, Some(Caution)),
            ("/home/alice/Documents", true, Some(Caution)),
            ("/home/alice/.local/state", true, Some(Caution)),
            // Routine cleanup in a home stays quiet.
            ("/home/alice/.cache", true, None),
            ("/home/alice/.cache/pip", true, None),
            ("/home/alice/.cargo/registry", true, None),
            ("/home/alice/.local/share/Trash", true, None),
            ("/home/alice/.local/share/Steam", true, None),
            ("/home/alice/big-download.iso", false, None),
            ("/home/alice/Documents/old-draft", true, None),
            ("/home/alice/.config/Code/Cache", true, None),
            ("/home/alice/.sshfs-mounts", true, Some(Caution)),
            // Somewhere unremarkable.
            ("/tmp/scratch", true, None),
            ("/srv/data/dump", true, None),
        ];
        for &(path, is_dir, want) in cases {
            assert_eq!(level(path, is_dir), want, "{path}");
        }
    }

    #[test]
    fn git_repositories_are_flagged() {
        let g = guard();
        let repo = g.assess(Path::new("/home/alice/code/project"), true, true);
        assert_eq!(
            repo.map(|r| (r.level, r.label)),
            Some((Caution, "git repo"))
        );
        let dot_git = g.assess(Path::new("/home/alice/code/project/.git"), true, false);
        assert_eq!(dot_git.map(|r| r.label), Some("git data"));
        assert!(
            g.assess(Path::new("/home/alice/code/project/src"), true, false)
                .is_none()
        );
    }

    #[test]
    fn reasons_say_what_is_at_stake() {
        let g = guard();
        let home = g.assess(Path::new("/home/alice"), true, false).unwrap();
        assert!(home.reason.contains("alice"), "{}", home.reason);
        let parent = g.assess(Path::new("/data"), true, false).unwrap();
        assert!(
            parent
                .reason
                .starts_with("Holds users/bob. Home directory of bob"),
            "{}",
            parent.reason
        );
        // Every built-in reason fits on one line of the dialog.
        for &(path, _, reason, _) in SYSTEM {
            assert!(reason.len() <= 90, "{path}: {reason}");
        }
        let key = g
            .assess(Path::new("/home/alice/.ssh/id_ed25519"), false, false)
            .unwrap();
        assert!(key.reason.contains("cannot be recovered"), "{}", key.reason);
    }

    #[test]
    fn passwd_yields_real_homes_only() {
        let text = "\
root:x:0:0::/root:/usr/bin/bash
bin:x:1:1::/:/usr/bin/nologin
http:x:33:33::/srv/http:/usr/bin/nologin
nobody:x:65534:65534:Kernel Overflow User:/:/usr/bin/nologin
alice:x:1000:1000:Alice:/home/alice:/usr/bin/zsh
bob:x:1001:1001::/data/users/bob:/bin/bash
broken line without fields
";
        let homes = parse_passwd(text);
        let paths: Vec<&str> = homes.iter().map(|(p, _)| p.to_str().unwrap()).collect();
        assert_eq!(paths, ["/root", "/home/alice", "/data/users/bob"]);
        assert_eq!(homes[1].1, "alice");
    }
}
