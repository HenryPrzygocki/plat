// SPDX-License-Identifier: GPL-3.0-or-later
use super::delete::{UNLOCK_WORDS, random_word};
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

#[test]
fn a_failed_first_scan_ends_the_session_with_its_reason() {
    let gone = crate::testutil::scratch("fatal").join("does-not-exist");
    let mut app = App::new(gone, scan::Options::default(), Theme::new(true));
    assert_eq!(app.failed(), None);
    app.start_scan(None);
    app.settle();
    assert!(
        app.failed().is_some_and(|m| m.contains("does-not-exist")),
        "{:?}",
        app.failed()
    );
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
