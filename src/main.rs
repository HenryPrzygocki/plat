// SPDX-License-Identifier: GPL-3.0-or-later

mod app;
mod ext;
mod fmt;
mod fsinfo;
mod guard;
mod ops;
mod scan;
mod snapshot;
mod theme;
mod tree;
mod treemap;
mod ui;

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{DefaultTerminal, Terminal};

use app::{App, Effect};
use theme::Theme;

struct Args {
    path: PathBuf,
    opts: scan::Options,
    threads: usize,
    read_only: bool,
    bench: bool,
    /// Render one frame at this size instead of running interactively.
    snapshot: Option<(u16, u16)>,
    /// Input to feed before the snapshot is taken.
    keys: String,
}

const HELP: &str = "\
plat - disk usage analyser with a treemap

Usage: plat [OPTIONS] [PATH]

Arguments:
  [PATH]  Directory to analyse [default: current directory]

Options:
  -x, --one-file-system  Stay on the filesystem PATH is on
  -a, --all-mounts       Also descend into virtual, network and image mounts
  -r, --read-only        Disable everything that changes the disk
  -j, --threads <N>      Scanner threads [default: number of CPUs]
  -h, --help             Print help
  -V, --version          Print version

Press ? inside plat for the key bindings.
";

fn parse_args() -> Result<Args, lexopt::Error> {
    use lexopt::prelude::*;
    let mut args = Args {
        path: PathBuf::from("."),
        opts: scan::Options::default(),
        threads: scan::default_threads(),
        read_only: false,
        bench: false,
        snapshot: None,
        keys: String::new(),
    };
    let mut parser = lexopt::Parser::from_env();
    while let Some(arg) = parser.next()? {
        match arg {
            Short('x') | Long("one-file-system") => args.opts.one_fs = true,
            Short('a') | Long("all-mounts") => args.opts.all_mounts = true,
            Short('r') | Long("read-only") => args.read_only = true,
            Short('j') | Long("threads") => {
                args.threads = parser.value()?.parse::<usize>()?.clamp(1, 256);
            }
            Long("bench") => args.bench = true,
            Long("snapshot") => {
                let v = parser.value()?.string()?;
                let size = v
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
                args.snapshot = Some(size.ok_or("expected WIDTHxHEIGHT")?);
            }
            Long("keys") => args.keys = parser.value()?.string()?,
            Short('h') | Long("help") => {
                print!("{HELP}");
                std::process::exit(0);
            }
            Short('V') | Long("version") => {
                println!("plat {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Value(v) => args.path = PathBuf::from(v),
            _ => return Err(arg.unexpected()),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("plat: {e}\nTry 'plat --help'.");
            return ExitCode::from(2);
        }
    };
    let root = match std::fs::canonicalize(&args.path) {
        Ok(p) if p.is_dir() => p,
        Ok(p) => {
            eprintln!("plat: {}: not a directory", p.display());
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("plat: {}: {e}", args.path.display());
            return ExitCode::from(1);
        }
    };
    scan::init_pool(args.threads);
    ops::raise_fd_limit();

    if args.bench || args.snapshot.is_some() {
        // These write to stdout and may be piped into `head`; die quietly on
        // a closed pipe like any other command-line tool.
        // SAFETY: restoring the default disposition of a signal is always sound.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    if args.bench {
        return bench(root, args.opts);
    }
    if let Some(size) = args.snapshot {
        // A scripted run may only delete inside the system temp directory.
        let disposable = root.starts_with(std::env::temp_dir()) && root != std::env::temp_dir();
        let mut app = App::new(root, args.opts, Theme::new(true));
        app.read_only = args.read_only;
        app.allow_delete = disposable;
        return match snapshot::run(&mut app, size, &args.keys) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("plat: {e}");
                ExitCode::from(1)
            }
        };
    }

    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        eprintln!("plat: needs an interactive terminal");
        return ExitCode::from(1);
    }
    let mut app = App::new(root, args.opts, Theme::detect());
    app.read_only = args.read_only;
    app.start_scan(None);

    let mut terminal = ratatui::init();
    enable_mouse();
    // ratatui's panic hook restores the screen; ours runs first and also lets
    // go of the mouse.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_mouse();
        previous(info);
        // A panic on a worker thread would otherwise leave the interface
        // running on a terminal that has just been restored.
        std::process::exit(101);
    }));
    let result = run(&mut terminal, &mut app);
    disable_mouse();
    ratatui::restore();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("plat: {e}");
            ExitCode::from(1)
        }
    }
}

/// Ask for button presses and wheel events only, in SGR encoding. Motion
/// reporting stays off, so moving the pointer over the window costs nothing.
fn enable_mouse() {
    let mut out = io::stdout();
    let _ = out.write_all(b"\x1b[?1000h\x1b[?1006h");
    let _ = out.flush();
}

fn disable_mouse() {
    let mut out = io::stdout();
    let _ = out.write_all(b"\x1b[?1006l\x1b[?1000l");
    let _ = out.flush();
}

/// Standard base64, for the clipboard escape sequence.
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Copy text to the clipboard: through the terminal (OSC 52, which also works
/// over ssh and under sudo), and through a clipboard tool where one is at
/// hand, for terminals that ignore the escape sequence.
fn copy_to_clipboard(text: &str) {
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = out.flush();

    let tools: [(&str, &str, &[&str]); 2] = [
        ("WAYLAND_DISPLAY", "wl-copy", &[]),
        ("DISPLAY", "xclip", &["-selection", "clipboard"]),
    ];
    for (var, tool, args) in tools {
        if std::env::var_os(var).is_none() {
            continue;
        }
        let child = Command::new(tool)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            // Reap it without holding up the interface.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return;
        }
    }
}

/// Leave the interface, run the user's shell in `dir`, and come back.
fn run_shell(terminal: &mut DefaultTerminal, dir: &Path) -> io::Result<()> {
    disable_mouse();
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, Show)?;
    println!("plat: shell in {} - exit to return", dir.display());
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let status = Command::new(&shell).current_dir(dir).status();
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, Hide)?;
    enable_mouse();
    // A new terminal object starts with empty buffers, so the next draw
    // repaints everything. `Terminal::clear` would do that too, but it first
    // asks the terminal where the cursor is and fails if no answer comes.
    *terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    status.map(|_| ())
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    let mut dirty = true;
    loop {
        dirty |= app.poll_jobs();
        dirty |= app.expire_toast();
        if let Some(message) = app.failed() {
            return Err(io::Error::other(message.to_string()));
        }
        if dirty || app.busy() {
            terminal.draw(|frame| ui::draw(frame, app))?;
            dirty = false;
        }

        // Animate while something is running; otherwise sleep until there is
        // input or the toast is due to disappear.
        let ready = if app.busy() {
            event::poll(Duration::from_millis(40))?
        } else if let Some(toast) = &app.toast {
            event::poll(toast.until.saturating_duration_since(Instant::now()))?
        } else {
            true
        };
        if !ready {
            continue;
        }
        loop {
            let effect = match event::read()? {
                Event::Key(key) => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                _ => Effect::None,
            };
            dirty = true;
            match effect {
                Effect::None => {}
                Effect::Quit => return Ok(()),
                Effect::Copy(text) => copy_to_clipboard(&text),
                Effect::Shell(dir) => {
                    run_shell(terminal, &dir)?;
                    app.after_shell();
                }
            }
            // Handle everything already queued before redrawing.
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}

/// Scan and build the tree without any interface, printing timings.
fn bench(root: PathBuf, opts: scan::Options) -> ExitCode {
    let progress = scan::Progress::default();
    let t0 = Instant::now();
    let (meta, raw) = match scan::scan(&root, opts, &progress) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("plat: {}: {e}", root.display());
            return ExitCode::from(1);
        }
    };
    let t_scan = t0.elapsed();
    let t1 = Instant::now();
    let tree = match tree::Tree::build(root, meta, raw) {
        Ok(t) => t,
        Err(_) => {
            eprintln!("plat: too many files for one scan");
            return ExitCode::from(1);
        }
    };
    let t_build = t1.elapsed();
    let rootn = tree.node(tree::Tree::ROOT);
    use std::sync::atomic::Ordering::Relaxed;
    println!(
        "nodes {}  files {}  dirs {}  unreadable {}",
        fmt::count(tree.len() as u64),
        fmt::count(progress.files.load(Relaxed)),
        fmt::count(progress.dirs.load(Relaxed)),
        fmt::count(progress.errors.load(Relaxed)),
    );
    println!(
        "disk {} ({} KiB)  apparent {}",
        fmt::size(rootn.disk),
        rootn.disk / 1024,
        fmt::size(rootn.size)
    );
    println!(
        "scan {:.3} s  build {:.3} s  total {:.3} s",
        t_scan.as_secs_f64(),
        t_build.as_secs_f64(),
        t0.elapsed().as_secs_f64()
    );
    let root = tree.root_path().to_path_buf();
    drop(tree);

    // Frame cost on a large window: first draw (layout from scratch), a
    // repeat (layout cached), and the two tabs that walk the whole subtree.
    let mut app = App::new(root, opts, Theme::new(true));
    if app.scan_blocking().is_err() {
        return ExitCode::from(1);
    }
    let Ok(mut terminal) = Terminal::new(ratatui::backend::TestBackend::new(220, 60));
    let mut frame = |app: &mut App, label: &str| {
        let t = Instant::now();
        let _ = terminal.draw(|f| ui::draw(f, app));
        println!("  {label:<28} {:>7.2} ms", t.elapsed().as_secs_f64() * 1e3);
    };
    println!("frame times at 220x60:");
    frame(&mut app, "first frame");
    frame(&mut app, "repeat frame");
    for (key, label) in [
        ('2', "largest files tab"),
        ('3', "file types tab"),
        ('1', "back to files"),
    ] {
        app.on_key(event::KeyEvent::new(
            event::KeyCode::Char(key),
            event::KeyModifiers::NONE,
        ));
        frame(&mut app, label);
    }
    if let Ok(status) = std::fs::read_to_string("/proc/self/status")
        && let Some(line) = status.lines().find(|l| l.starts_with("VmHWM"))
    {
        println!("peak memory {}", line.trim_start_matches("VmHWM:").trim());
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_the_rfc_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in cases {
            assert_eq!(super::base64(plain.as_bytes()), encoded);
        }
    }
}
