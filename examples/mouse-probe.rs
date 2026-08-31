//! Diagnostic: does this terminal/host deliver SGR mouse events to a
//! full-screen app? Reproduces the sidebar's terminal lifecycle from the
//! 2026-08-30 sidebar-mouse spec §6 — raw mode, alternate screen, mouse
//! capture — because wheel transport differs between primary and
//! alternate screens, and a primary-screen probe could green-light a
//! mode the sidebar never runs in.
//!
//! Run it, click around, scroll both ways, press `q`. The summary that
//! survives on the primary screen is what the spec's gate scores.

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};

/// Restores in the sidebar's order even on panic: capture off first,
/// then leave the alternate screen, then raw mode off. The probe must
/// not leak capture into the caller's shell either.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = crossterm::execute!(out, DisableMouseCapture);
        let _ = crossterm::execute!(out, LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn main() -> std::io::Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    if let Err(error) = crossterm::execute!(out, EnterAlternateScreen, EnableMouseCapture) {
        let _ = crossterm::terminal::disable_raw_mode();
        return Err(error);
    }
    let restore = Restore;

    print!("mouse-probe: click, scroll both ways, then press q\r\n");
    out.flush()?;

    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut plain_left_downs = 0u32;
    let mut max_col = 0u16;
    let mut max_row = 0u16;

    loop {
        if !crossterm::event::poll(Duration::from_millis(100))? {
            continue;
        }
        match crossterm::event::read()? {
            Event::Key(key) => match (key.code, key.modifiers) {
                (KeyCode::Char('q'), _) => break,
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => break,
                // Tally other keys: a host that translates the wheel
                // into arrow presses ("alternate scroll") instead of
                // forwarding SGR scroll shows up here as Key(Up/Down).
                (code, _) => {
                    *counts.entry(format!("Key({code:?})")).or_default() += 1;
                    print!("Key({code:?})\r\n");
                    out.flush()?;
                }
            },
            Event::Mouse(mouse) => {
                *counts.entry(format!("{:?}", mouse.kind)).or_default() += 1;
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && mouse.modifiers.is_empty()
                {
                    plain_left_downs += 1;
                }
                max_col = max_col.max(mouse.column);
                max_row = max_row.max(mouse.row);
                print!(
                    "{:?} col={} row={} mods={:?}\r\n",
                    mouse.kind, mouse.column, mouse.row, mouse.modifiers
                );
                out.flush()?;
            }
            _ => {}
        }
    }

    drop(restore);

    println!("mouse-probe summary");
    println!("  Down(Left) with empty modifiers: {plain_left_downs}");
    for (kind, count) in &counts {
        println!("  {kind}: {count}");
    }
    println!("  max coordinate seen: col={max_col} row={max_row}");
    if counts.is_empty() {
        println!("  NO MOUSE EVENTS ARRIVED");
    }
    Ok(())
}
