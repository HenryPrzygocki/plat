# plat

A disk usage analyser for the terminal: a nested treemap of where the space
went, next to a sortable list, with guarded batch deletion.

A *plat* is a surveyor's map of land divided into lots.

## Install

Needs a Rust toolchain (edition 2024) and Linux.

On Arch, build and install it as a package:

```sh
cd packaging/arch
makepkg -si                        # builds, then asks for sudo to install
```

After that `plat` works from any terminal, `sudo plat /` included, and
`sudo pacman -R plat` removes it again. Rebuild the same way after changing
the code.

Without packaging:

```sh
cargo build --release
./target/release/plat ~            # analyse your home directory
./target/release/plat              # or the current directory
```

### With and without sudo

Without sudo, directories you cannot read are flagged, counted in the header,
and their parents are shown with `≥` in front of the size.

With sudo everything is readable: `sudo plat /`. If the package is not
installed, name the binary instead: `sudo ./target/release/plat /`.

As root, a `ROOT` badge is shown, the delete dialog warns that permissions no
longer protect anything, and the trash is not offered (root's trash is not
yours). plat never writes configuration or cache files, so running it as root
leaves nothing root-owned behind.

`plat --read-only` disables everything that changes the disk.

## Options

```
plat [OPTIONS] [PATH]

  -x, --one-file-system  Stay on the filesystem PATH is on
  -a, --all-mounts       Also descend into virtual, network and image mounts
  -r, --read-only        Disable everything that changes the disk
  -j, --threads <N>      Scanner threads [default: number of CPUs]
```

By default a scan crosses onto other real filesystems (a separate `/home`,
say) and skips virtual ones (procfs, sysfs, tmpfs), network and FUSE mounts,
read-only images (squashfs, overlay), and bind mounts, whose contents are
stored, and counted, somewhere else. Skipped mounts stay visible in the list,
labelled with their type. A filesystem that can be reached at two places
inside the scan (a btrfs top level mounted next to its own subvolumes, for
one) is entered at one of them only, even with `--all-mounts`.

Scanning never mounts anything. An automount point that is not mounted yet
(an `x-systemd.automount` entry in fstab, for instance) is listed as an
autofs mount and left alone; if it is already mounted, it is scanned like any
other disk. `--all-mounts` enters automount points too, which mounts them.

## Keys

| Key | Action |
| --- | --- |
| `j` `k` | Next / previous entry |
| `l` / `enter` | Open the selected directory |
| `h` / `backspace` / `esc` | Parent directory |
| `←↓↑→` / `H J K L` | In the map: move to the neighbouring tile. In the list: same as `h j k l`. |
| `g` `G` | First / last entry |
| `tab` | Switch between map and list |
| `1` `2` `3` | List tabs: files here, largest files below, file types below |
| `space` | Mark or unmark for deletion |
| `d` / `del` | Delete the marked items, or the selected one if nothing is marked |
| `u` | Clear all marks |
| `r` / `F5` | Rescan the current directory |
| `a` | Switch between disk usage and apparent size |
| `s` | Cycle the sort order of the file list |
| `+` `-` | Treemap nesting depth |
| `y` | Copy the selected path |
| `o` | Open with the default application |
| `!` | Shell in the current directory (rescans on return) |
| `?` | Key help |
| `q` | Quit |

`h j k l` walk the tree the same way whichever pane has the focus, as in
`ncdu` or `ranger`. Moving by position across the map is on the arrow keys
and their shifted letters.

Mouse: click selects, double click opens, right click goes up, the wheel over
the map zooms in on the tile under the pointer or back out, and the path
segments in the header are clickable.

## Reading the map

Files are coloured by type. A directory large enough to hold labelled tiles is
drawn as a frame with its contents inside; a smaller one is a single block,
tinted by whichever file type makes up most of it. Everything too small to
draw is folded into one dotted "+N more" tile per directory. Marked items are
red and hatched.

On the *types* tab, moving through the list spotlights that type in the map.
On the *largest* tab, the selected file's tile is highlighted and `enter`
jumps to it.

## Deleting

Marks can be collected anywhere in the tree. `d` opens a dialog listing the
exact paths, the space they free, and three choices: cancel (the default),
move to trash, or delete permanently.

- The paths in the dialog are fixed when it opens; those are what gets
  deleted.
- Deletion works through directory file descriptors and never follows a
  symlink, including one swapped in while it runs.
- It refuses to cross onto another filesystem. A mount point (or btrfs
  subvolume) inside a doomed directory is left alone with everything on it,
  and the dialog will not accept a mount point itself. That includes bind
  mounts, which share the device number of where they come from and are
  recognised by asking the kernel whether a directory is a mount root.
- Read-only directories you own (Go's module cache, for one) are made
  writable and removed, where `rm -rf` would stop.
- If part of a directory cannot be removed, a report names what was in the
  way, and the directory is rescanned so the display matches the disk.

Trash uses `gio trash` or `trash-put`, whichever is installed. Trashing on the
same filesystem frees nothing until the trash is emptied.

### Safeguards

plat never refuses a deletion outright; that decision is yours. What it does
is recognise targets whose loss is hard or impossible to undo, say what they
are, and ask for effort in proportion.

| Target | What happens |
| --- | --- |
| Ordinary | The dialog, with cancel as the default. |
| Worth a second look | The same dialog under an amber band, with a line per item saying what it is. |
| Protected | A red band, the reason, and both destructive buttons locked until a randomly chosen word is typed. |

*Protected* covers what breaks the system or cannot be recreated:

- system directories (`/usr`, `/usr/lib`, `/etc`, `/var/lib`, ...), the
  package database, and everything under `/boot`;
- a user's home directory, and `/home` itself;
- SSH and GPG keys, password stores and keyrings, and anything inside them;
- any directory that contains one of the above.

*Worth a second look* covers:

- files installed by the package manager (anything else under `/usr`);
- system configuration under `/etc`, and the state of a service under
  `/var/lib`;
- the top level of a home directory: its folders and its hidden settings
  (`~/.cache` and the trash are exempt, since emptying them is routine);
- a git repository, because unpushed commits exist nowhere else;
- a selection of a gigabyte or more that is at least a quarter of everything
  on the filesystem.

Protected entries carry a `protected` note in the file list, and marking
anything in either group says why on the status line straight away. Home
directories are read from `/etc/passwd`. Paths are compared by component, so
`/mnt/backup/usr` is not treated as the running system's `/usr`.

## How it is fast

- **Scan.** One rayon task per directory. Each entry costs one `statx`
  relative to the open directory, so no path is resolved per file. Hard links
  are counted once. On a warm cache this is roughly 3 million entries per
  second on a 12-thread machine: 1.1 M entries in under 0.4 s, where `du`
  takes 2.2 s and `gdu` 0.6 s.
- **Tree.** One flat arena in depth-first order, 40 bytes per node, names in a
  shared byte pool. A subtree is a contiguous slice, so the "largest" and
  "types" tabs and the tint of a directory tile are linear passes with no
  pointer chasing. Children are stored largest first.
- **Drawing.** The treemap layout is cached until the directory, window size
  or tree changes. A frame takes 1 to 7 ms. When nothing is happening the
  process sleeps on input and uses no CPU.
- **Memory.** About 80 bytes per entry once the scan is done, and up to
  twice that at the peak while it runs.

## Limits

- Linux only.
- Wants a terminal with 24-bit colour and Unicode block characters. There is a
  256-colour fallback for the Linux console, which has had little attention.
- On btrfs, snapshots are counted at full size, because shared extents cannot
  be told apart by `stat`.
- Only the first link to a multiply-linked file is counted; the others show
  as zero-sized hard links.
- Sizes of directories that could not be fully read are lower bounds.

## Development

```sh
cargo test                         # unit and integration tests
plat --bench PATH                  # scan and frame timings, no interface
plat --snapshot 160x48 --keys "j enter 2" PATH   # one frame as ANSI text
```

`--snapshot` scans, feeds the given keys (`j`, `enter`, `space`, `tab`,
`click:X,Y`, ...) and prints the resulting screen. It refuses to delete
anything outside the system temp directory.

Five tests are ignored by default because of what they need:

```sh
# mount rules for scanning and deleting: need a private mount namespace
unshare -Urm "$(cargo test --no-run 2>&1 | grep -o 'target/[^)]*' | tail -1)" --ignored mount
# trash round trip: puts one probe file in the real trash and removes it again
PLAT_TRASH_PROBE_DIR=$PWD/target cargo test -- --ignored trash_round_trip
```

## Licence

Copyright (C) 2026 Henry Przygocki

plat is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version.

It is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR
A PARTICULAR PURPOSE. See [LICENSE](LICENSE) for the full text.
