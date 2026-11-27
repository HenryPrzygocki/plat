// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless rendering, for tests and for looking at the interface without a
//! terminal: scan, feed a script of input, draw one frame into memory and
//! print it as ANSI text.

use std::fmt::Write as _;
use std::io::{self, Write};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::{Color, Modifier};

use crate::app::App;
use crate::ui;

/// Scan, apply `script`, and print the resulting frame to stdout.
pub fn run(app: &mut App, (w, h): (u16, u16), script: &str) -> io::Result<()> {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).map_err(io::Error::other)?;
    if script.trim() == "@scan" {
        // Show the scanning screen: draw while the scan is still running.
        app.start_scan(None);
        std::thread::sleep(std::time::Duration::from_millis(30));
        app.tick = 17;
        terminal
            .draw(|frame| ui::draw(frame, app))
            .map_err(io::Error::other)?;
        let text = to_ansi(terminal.backend().buffer());
        return io::stdout().write_all(text.as_bytes());
    }
    app.scan_blocking()?;
    let mut draw = |app: &mut App| -> io::Result<()> {
        terminal
            .draw(|frame| ui::draw(frame, app))
            .map(|_| ())
            .map_err(io::Error::other)
    };
    // Input handling relies on regions recorded by the previous draw.
    draw(app)?;
    for token in script.split_whitespace() {
        apply(app, token)?;
        app.settle();
        draw(app)?;
    }
    let text = to_ansi(terminal.backend().buffer());
    io::stdout().write_all(text.as_bytes())
}

/// Feed one script token to the app: a key name, a single character, or a
/// mouse action such as `click:10,4`.
fn apply(app: &mut App, token: &str) -> io::Result<()> {
    let bad = || io::Error::other(format!("unknown input '{token}'"));
    if let Some((action, at)) = token.split_once(':').filter(|(a, _)| a.len() > 1) {
        let (x, y) = at
            .split_once(',')
            .and_then(|(x, y)| Some((x.parse::<u16>().ok()?, y.parse::<u16>().ok()?)))
            .ok_or_else(bad)?;
        let send = |app: &mut App, kind| {
            app.on_mouse(MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            });
        };
        match action {
            "click" => send(app, MouseEventKind::Down(MouseButton::Left)),
            "dclick" => {
                send(app, MouseEventKind::Down(MouseButton::Left));
                send(app, MouseEventKind::Down(MouseButton::Left));
            }
            "rclick" => send(app, MouseEventKind::Down(MouseButton::Right)),
            "wheelup" => send(app, MouseEventKind::ScrollUp),
            "wheeldown" => send(app, MouseEventKind::ScrollDown),
            _ => return Err(bad()),
        }
        return Ok(());
    }
    let (mods, name) = match token.strip_prefix("ctrl-") {
        Some(rest) => (KeyModifiers::CONTROL, rest),
        None => (KeyModifiers::NONE, token),
    };
    let code = match name {
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "bs" => KeyCode::Backspace,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pgup" => KeyCode::PageUp,
        "pgdn" => KeyCode::PageDown,
        "del" => KeyCode::Delete,
        "f5" => KeyCode::F(5),
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => return Err(bad()),
            }
        }
    };
    app.on_key(KeyEvent::new(code, mods));
    Ok(())
}

/// Serialise a buffer as text with SGR colour sequences, one line per row.
fn to_ansi(buf: &Buffer) -> String {
    let area = *buf.area();
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        let mut last = None;
        for x in area.left()..area.right() {
            let cell = &buf[(x, y)];
            let key = (cell.fg, cell.bg, cell.modifier);
            if last != Some(key) {
                out.push_str("\x1b[0");
                if cell.modifier.contains(Modifier::BOLD) {
                    out.push_str(";1");
                }
                colour(&mut out, cell.fg, 38);
                colour(&mut out, cell.bg, 48);
                out.push('m');
                last = Some(key);
            }
            out.push_str(cell.symbol());
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

fn colour(out: &mut String, c: Color, base: u8) {
    match c {
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";{base};2;{r};{g};{b}");
        }
        Color::Indexed(i) => {
            let _ = write!(out, ";{base};5;{i}");
        }
        _ => {}
    }
}
