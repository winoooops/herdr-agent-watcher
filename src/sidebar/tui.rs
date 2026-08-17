use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use crate::sidebar::config::Theme;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::text::Text;
use ratatui::widgets::Paragraph;

use crate::sidebar::layout::{clamp_scroll, ensure_visible, reanchor};
use crate::sidebar::reducer::{apply_line, State};
use crate::sidebar::view::{Line, Rendered, Role, Semantic, ViewInput};

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        if let Err(error) =
            crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)
        {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

enum WireEvent {
    Line(String),
    Ended,
}

fn spawn_reader(stream: UnixStream) -> Receiver<WireEvent> {
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    let _ = sender.send(WireEvent::Ended);
                    break;
                }
                Ok(_) if sender.send(WireEvent::Line(line)).is_err() => break,
                Ok(_) => {}
            }
        }
    });
    receiver
}

/// Connect and subscribe, or say why not. Separated because the sidebar does
/// this again every time the daemon goes away -- including when the settings
/// panel restarts it, which is a disconnect this pane caused on purpose.
fn subscribe(socket: &std::path::Path) -> Result<Receiver<WireEvent>, String> {
    let mut stream = UnixStream::connect(socket).map_err(|error| {
        format!(
            "herdr-agent-watcher daemon is not running\n(no state socket at {}: {error})",
            socket.display()
        )
    })?;
    stream
        .write_all(b"{\"method\":\"subscribe\"}\n")
        .map_err(|_| "herdr-agent-watcher daemon closed the state socket".to_string())?;
    Ok(spawn_reader(stream))
}

/// How long to keep trying before giving up on a daemon that is not coming
/// back. A restart takes a second or two; a minute is long enough that the
/// only thing still waiting is a daemon that died.
const RECONNECT_FOR: Duration = Duration::from_secs(60);
const RECONNECT_EVERY: Duration = Duration::from_millis(400);

fn wait_any_key() {
    loop {
        if matches!(crossterm::event::poll(Duration::from_secs(3600)), Ok(true))
            && matches!(crossterm::event::read(), Ok(Event::Key(_)))
        {
            return;
        }
    }
}

fn draw_message_and_wait(message: &str) -> i32 {
    if let Ok(_guard) = TerminalGuard::enter() {
        if let Ok(mut terminal) =
            ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
        {
            let _ = terminal.draw(|frame| {
                frame.render_widget(
                    Paragraph::new(Text::raw(format!("{message}\n\npress any key to close"))),
                    frame.area(),
                );
            });
            wait_any_key();
            return 1;
        }
    }
    eprintln!("{message}");
    1
}

/// True once per minute. Pure and millisecond-based so it is testable without
/// sleeping, and so the shell owns the only clock (§2.2).
fn age_tick_due(last_tick_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_tick_ms) >= 60_000
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn view_input<'a>(
    cfg: &'a crate::sidebar::config::Loaded,
    live: &'a crate::sidebar::live::Live,
    toggled: &'a std::collections::HashSet<String>,
    cursor: Option<&'a str>,
) -> ViewInput<'a> {
    ViewInput {
        cursor,
        toggled,
        hide_idle: live.hide_idle,
        scope: live.workspace_filter(),
        sort: live.sort,
        auto_expand: live.auto_expand,
        agent_mark: live.agent_mark,
        tool_calls: live.tool_calls,
        theme: live.theme,
        trace_lines: live.trace_lines,
        agents: &cfg.appearances,
        config: cfg.status,
    }
}

fn reconcile_cursor(cursor: &mut Option<String>, rendered: &Rendered, prev_index: usize) -> bool {
    let ids: Vec<&str> = rendered.spans.iter().map(|(id, _)| id.as_str()).collect();
    if let Some(current) = cursor.as_deref() {
        if ids.contains(&current) {
            return false;
        }
    }
    let next = if ids.is_empty() {
        None
    } else {
        Some(ids[prev_index.min(ids.len() - 1)].to_string())
    };
    let changed = *cursor != next;
    *cursor = next;
    changed
}

fn index_of(rendered: &Rendered, cursor: Option<&str>) -> usize {
    cursor
        .and_then(|id| rendered.spans.iter().position(|(sid, _)| sid == id))
        .unwrap_or(0)
}

fn move_cursor(cursor: &mut Option<String>, rendered: &Rendered, delta: isize) {
    if rendered.spans.is_empty() {
        *cursor = None;
        return;
    }
    let at = index_of(rendered, cursor.as_deref()) as isize;
    let next = (at + delta).clamp(0, rendered.spans.len() as isize - 1) as usize;
    *cursor = Some(rendered.spans[next].0.clone());
}

fn to_line(line: &Line, theme: Theme, truecolor: bool) -> ratatui::text::Line<'static> {
    ratatui::text::Line::from(
        line.iter()
            .map(|span| {
                ratatui::text::Span::styled(
                    span.text.clone(),
                    to_ratatui(span.style, theme, truecolor),
                )
            })
            .collect::<Vec<_>>(),
    )
}

#[derive(Default)]
struct Interaction {
    cursor: Option<String>,
    toggled: std::collections::HashSet<String>,
    offset: u16,
    follow: bool,
    notice: Option<String>,
}

enum KeyOutcome {
    Quit,
    Handled,
}

/// Which panel is open. `None` is the card list.
pub(crate) enum Dialog {
    Menu {
        cursor: usize,
    },
    Keys {
        cursor: usize,
    },
    Settings {
        cursor: usize,
        dirty: Vec<crate::sidebar::live::Setting>,
        path: Option<std::path::PathBuf>,
        source: Result<Option<String>, String>,
        /// What the daemon is actually running. The panel can change the
        /// value and save it, but only a restart makes it true -- so leaving
        /// with these apart is the one exit that asks a question.
        interval_at_open: u32,
        /// The mandatory prompt, when it is up. A sub-state of the settings
        /// panel rather than a dialog of its own, because answering it may
        /// have to write the file, and the file is here.
        confirm: Option<usize>,
    },
    Doctor {
        offset: usize,
        report: Result<crate::agents::doctor::Report, String>,
        taken_at: u64,
    },
}

impl Dialog {
    fn menu() -> Self {
        Dialog::Menu { cursor: 0 }
    }

    /// `None` for a panel that scrolls instead of selecting.
    fn cursor_mut(&mut self) -> Option<&mut usize> {
        match self {
            Dialog::Menu { cursor } | Dialog::Keys { cursor } => Some(cursor),
            Dialog::Settings { cursor, .. } => Some(cursor),
            Dialog::Doctor { .. } => None,
        }
    }

    fn offset_mut(&mut self) -> Option<&mut usize> {
        match self {
            Dialog::Doctor { offset, .. } => Some(offset),
            _ => None,
        }
    }

    /// Whether `esc` steps back to the menu or closes the sidebar's overlay
    /// entirely. Opening settings with `s` and being dropped into a menu you
    /// never saw is not going back.
    /// Whether a panel opened from here should offer a way back to the menu:
    /// either this IS the menu, or this was itself reached through it.
    fn len(&self) -> usize {
        match self {
            Dialog::Menu { .. } => MENU.len(),
            Dialog::Keys { .. } => KEYS.len(),
            Dialog::Settings { .. } => crate::sidebar::live::SETTINGS.len(),
            // Doctor scrolls; it has no selectable rows, so it has no cursor
            // to bound.
            Dialog::Doctor { .. } => 0,
        }
    }

    /// How many rows the panel will draw, which is what a scroll offset is
    /// bounded by.
    fn row_count(&self) -> usize {
        match self {
            Dialog::Doctor { report, .. } => report
                .as_ref()
                .map(|report| doctor_rows(report).len())
                .unwrap_or(1),
            other => other.len(),
        }
    }
}

/// Cycling the selected setting, forward or back. Only the settings panel has
/// anything to cycle.
fn cycle_selected(dialog: &mut Dialog, live: &mut crate::sidebar::live::Live, back: bool) {
    let Dialog::Settings { cursor, dirty, .. } = dialog else {
        return;
    };
    let Some(setting) = crate::sidebar::live::SETTINGS.get(*cursor).copied() else {
        return;
    };
    // Read here, not in `live.rs`, which stays pure and takes the workspace as
    // an argument.
    let workspace = std::env::var("HERDR_WORKSPACE_ID").ok();
    if back {
        live.cycle_back(setting, workspace.as_deref());
    } else {
        live.cycle(setting, workspace.as_deref());
    }
    if !dirty.contains(&setting) {
        dirty.push(setting);
    }
}

fn doctor_dialog() -> Dialog {
    Dialog::Doctor {
        offset: 0,
        report: crate::agents::claude_bridge::doctor_report(),
        taken_at: now_unix_ms(),
    }
}

#[cfg(test)]
fn doctor_taken_at(dialog: &Option<Dialog>) -> Option<u64> {
    match dialog {
        Some(Dialog::Doctor { taken_at, .. }) => Some(*taken_at),
        _ => None,
    }
}

fn settings_dialog(interval_at_open: u32) -> Dialog {
    let path = crate::sidebar::config::config_path();
    let source = match path.as_ref() {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("read {}: {error}", path.display())),
        },
        None => Err("HERDR_PLUGIN_CONFIG_DIR is not set".into()),
    };
    Dialog::Settings {
        cursor: 0,
        interval_at_open,
        confirm: None,
        dirty: Vec::new(),
        path,
        source,
    }
}

/// Answer the mandatory prompt. `restart` saves the new interval and reloads
/// the daemon; otherwise the value goes back to what the daemon is running,
/// and if it had already been saved the file goes back with it -- cancelling
/// means the setting was never changed, not that it was changed and ignored.
fn resolve_interval(
    dialog: &mut Dialog,
    live: &mut crate::sidebar::live::Live,
    restart: bool,
) -> String {
    let Dialog::Settings {
        interval_at_open, ..
    } = dialog
    else {
        return String::new();
    };
    let previous = *interval_at_open;

    if !restart {
        live.interval_ms = previous;
        // Only writes if the interval is among the keys this session changed;
        // `edit` touches nothing else either way.
        let _ = save_settings(dialog, live);
        return format!("interval ms put back to {previous}");
    }

    if let Err(error) = save_settings(dialog, live) {
        return format!("save failed: {error}");
    }
    match restart_daemon() {
        Ok(()) => format!("daemon reloaded at {} ms", live.interval_ms),
        Err(error) => format!("saved, but the reload failed: {error}"),
    }
}

/// The same action the plugin exposes, invoked through the herdr binary the
/// sidebar was handed. Not the daemon binary directly: restarting is herdr's
/// to sequence, including the singleton takeover.
fn restart_daemon() -> Result<(), String> {
    let herdr = std::env::var_os("HERDR_BIN_PATH")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "HERDR_BIN_PATH is not set".to_string())?;
    let plugin =
        std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "herdr-agent-watcher".to_string());
    let out = std::process::Command::new(herdr)
        .args(["plugin", "action", "invoke", "restart-daemon", "--plugin"])
        .arg(&plugin)
        .output()
        .map_err(|error| format!("cannot run herdr: {error}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn save_settings(dialog: &mut Dialog, live: &crate::sidebar::live::Live) -> Result<String, String> {
    let Dialog::Settings {
        dirty,
        path,
        source,
        ..
    } = dialog
    else {
        return Err("settings panel is not open".into());
    };
    let path = path
        .as_ref()
        .ok_or_else(|| "HERDR_PLUGIN_CONFIG_DIR is not set".to_string())?;
    let current = source.as_ref().map_err(Clone::clone)?;
    let body =
        crate::sidebar::settings_file::edit(current.as_deref().unwrap_or_default(), live, dirty)?;
    let expected = match current {
        Some(text) => crate::sidebar::settings_file::Expected::Contents(text.clone()),
        None => crate::sidebar::settings_file::Expected::Missing,
    };
    crate::sidebar::settings_file::save(path, expected, &body)?;
    *source = Ok(Some(body));
    dirty.clear();
    Ok(format!("saved {}", path.display()))
}

fn doctor_glyph(level: crate::agents::doctor::Level) -> &'static str {
    match level {
        crate::agents::doctor::Level::Ok => "✓",
        crate::agents::doctor::Level::Warn => "!",
        crate::agents::doctor::Level::Fail => "✗",
    }
}

fn doctor_remedy(remedy: &crate::agents::doctor::Remedy) -> String {
    use crate::agents::doctor::Remedy;
    match remedy {
        Remedy::PluginAction { id } => {
            format!("herdr plugin action invoke {id} --plugin herdr-agent-watcher")
        }
        Remedy::RestartSession => "recreate this Claude session".into(),
        Remedy::WaitOrInteract => {
            "wait for its next status-line render, or send it a prompt".into()
        }
        Remedy::ReopenPane => "close and reopen this pane".into(),
        Remedy::MoveTables { tables, from, to } => format!(
            "move {} from {} to {}",
            tables
                .iter()
                .map(|table| format!("[{table}]"))
                .collect::<Vec<_>>()
                .join(" and "),
            from.display(),
            to.display()
        ),
        Remedy::WriteSettingsBlock { path, block } => {
            format!("add to {}: {block}", path.display())
        }
    }
}

fn doctor_rows(report: &crate::agents::doctor::Report) -> Vec<crate::sidebar::dialog::Row> {
    use crate::sidebar::dialog::Row;
    let mut rows = Vec::new();
    for check in &report.checks {
        rows.push(Row::Entry {
            label: doctor_glyph(check.level).into(),
            value: check.summary.clone(),
            enabled: false,
        });
        if let Some(evidence) = &check.evidence {
            rows.push(Row::Note(evidence.clone()));
        }
        if let Some(remedy) = &check.remedy {
            rows.push(Row::Note(format!("→ {}", doctor_remedy(remedy))));
        }
    }
    if !report.panes.is_empty() {
        rows.push(Row::Rule);
    }
    for pane in &report.panes {
        let window = pane
            .window_size
            .map(|size| format!(" · {size} window"))
            .unwrap_or_default();
        rows.push(Row::Entry {
            label: format!("{} {}", doctor_glyph(pane.level), pane.pane_id),
            value: format!("{}{}", pane.summary, window),
            enabled: false,
        });
        if let Some(session) = &pane.agent_session {
            rows.push(Row::Note(format!("session {session}")));
        }
        if let Some(path) = &pane.shadowed_by {
            rows.push(Row::Note(format!("shadowed by {}", path.display())));
        }
        if let Some(remedy) = &pane.remedy {
            rows.push(Row::Note(format!("→ {}", doctor_remedy(remedy))));
        }
    }
    rows
}

const MENU: [(&str, &str); 2] = [("Settings", "s"), ("Doctor", "d")];

/// One inventory. The sheet is built from it, and the test presses every entry
/// through the router. Descriptions say where a key means something, because
/// `s` and `r` mean different things inside a panel and outside one.
const KEYS: [(&str, &str); 13] = [
    ("j / ↓", "move down"),
    ("k / ↑", "move up"),
    ("o / ↵", "expand a card · change a setting"),
    ("h / l · ← / →", "change a setting, back and forward"),
    ("z", "hide idle agents"),
    ("PageUp / PageDown", "scroll"),
    ("x", "menu"),
    ("?", "this sheet"),
    ("s", "settings, in a panel · save, in the settings panel"),
    ("d", "doctor, in a panel"),
    ("r", "rebuild the report, in the doctor panel"),
    ("q / esc", "close a panel, or the sidebar"),
    ("ctrl-c", "close the sidebar"),
];

/// Each key, and the state it needs in order to do anything.
///
/// The starting cursor is per key and not a constant: with two cards, `j`
/// from the last one correctly does nothing and `k` from the first correctly
/// does nothing. A single start makes one of them look like an ignored key.
///
/// `r` only means something with the doctor panel open, `↵` only inside
/// settings, and `s`/`d` only once a panel is up at all -- from the cards
/// they are deliberately dead. Each arrives with the state that gives it
/// meaning.
#[cfg(test)]
const ROUTED: [(&str, KeyCode, KeyModifiers, &str, Option<Dialog>); 13] = [
    ("j / ↓", KeyCode::Char('j'), KeyModifiers::NONE, "a", None),
    ("k / ↑", KeyCode::Char('k'), KeyModifiers::NONE, "b", None),
    ("o / ↵", KeyCode::Char('o'), KeyModifiers::NONE, "a", None),
    (
        "h / l · ← / →",
        KeyCode::Char('l'),
        KeyModifiers::NONE,
        "a",
        Some(Dialog::Settings {
            cursor: 0,
            dirty: Vec::new(),
            path: None,
            source: Ok(None),
            interval_at_open: 1000,
            confirm: None,
        }),
    ),
    ("z", KeyCode::Char('z'), KeyModifiers::NONE, "a", None),
    (
        "PageUp / PageDown",
        KeyCode::PageDown,
        KeyModifiers::NONE,
        "a",
        None,
    ),
    ("x", KeyCode::Char('x'), KeyModifiers::NONE, "a", None),
    ("?", KeyCode::Char('?'), KeyModifiers::NONE, "a", None),
    (
        "s",
        KeyCode::Char('s'),
        KeyModifiers::NONE,
        "a",
        Some(Dialog::Menu { cursor: 0 }),
    ),
    (
        "d",
        KeyCode::Char('d'),
        KeyModifiers::NONE,
        "a",
        Some(Dialog::Menu { cursor: 0 }),
    ),
    (
        "r",
        KeyCode::Char('r'),
        KeyModifiers::NONE,
        "a",
        Some(Dialog::Doctor {
            offset: 0,
            report: Ok(crate::agents::doctor::Report {
                checks: Vec::new(),
                panes: Vec::new(),
            }),
            taken_at: 0,
        }),
    ),
    ("q / esc", KeyCode::Esc, KeyModifiers::NONE, "a", None),
    (
        "ctrl-c",
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
        "a",
        None,
    ),
];

/// The panel needs a frame it can be legible in. Below this it does not open:
/// four broken characters are worse than a refusal that says why.
const MIN_DIALOG_WIDTH: u16 = 20;
const MIN_DIALOG_HEIGHT: u16 = 8;

/// Every key press belongs to exactly one of the two layers. With a panel
/// open the card list gets nothing -- a `j` that scrolls the list under an
/// open dialog is a bug that looks like a redraw.
#[allow(clippy::too_many_arguments)]
fn route(
    key: crossterm::event::KeyEvent,
    open: &mut Option<Dialog>,
    it: &mut Interaction,
    live: &mut crate::sidebar::live::Live,
    rendered: &Rendered,
    viewport: u16,
    total: usize,
    width: u16,
    height: u16,
) -> KeyOutcome {
    // Transient, so it lasts exactly until the next key -- clearing it only
    // when a later `x` succeeds leaves it on screen through every `j` and `z`
    // in between.
    it.notice = None;

    if let Some(dialog) = open.as_mut() {
        // The prompt is mandatory: while it is up nothing else in the panel
        // answers, because every other key would be a way of not deciding.
        if let Dialog::Settings {
            confirm: Some(choice),
            ..
        } = dialog
        {
            match (key.code, key.modifiers) {
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    return KeyOutcome::Quit
                }
                (KeyCode::Char('j'), _) | (KeyCode::Down, _) => *choice = 1,
                (KeyCode::Char('k'), _) | (KeyCode::Up, _) => *choice = 0,
                (KeyCode::Enter, _) | (KeyCode::Char('o'), _) => {
                    let restart = *choice == 0;
                    it.notice = Some(resolve_interval(dialog, live, restart));
                    *open = Some(Dialog::menu());
                }
                _ => {}
            }
            return KeyOutcome::Handled;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                return KeyOutcome::Quit
            }
            // Back one level, not out: a panel reached through the menu
            // returns to it. One reached with `s` or `d` was never under a
            // menu, and dropping into one you did not open is not going back.
            (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => {
                if let Dialog::Settings {
                    interval_at_open,
                    confirm,
                    ..
                } = dialog
                {
                    // Mandatory: the value and the running daemon disagree,
                    // and leaving without saying which one wins would leave a
                    // setting that looks applied and is not.
                    if confirm.is_none() && live.interval_ms != *interval_at_open {
                        *confirm = Some(0);
                        return KeyOutcome::Handled;
                    }
                    // Already up. esc does not dismiss it.
                    if confirm.is_some() {
                        return KeyOutcome::Handled;
                    }
                }
                // One level back, always: the menu is the parent of both
                // panels however you reached them, so `s` straight from the
                // cards still leaves the selector between you and the way
                // out. The menu itself is the level that closes.
                *open = (!matches!(dialog, Dialog::Menu { .. })).then(Dialog::menu);
            }
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                let last = dialog.len().saturating_sub(1);
                let rows = dialog.row_count();
                if let Some(cursor) = dialog.cursor_mut() {
                    *cursor = (*cursor + 1).min(last);
                } else if let Some(offset) = dialog.offset_mut() {
                    *offset = (*offset + 1).min(rows.saturating_sub(1));
                }
            }
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                if let Some(cursor) = dialog.cursor_mut() {
                    *cursor = cursor.saturating_sub(1);
                } else if let Some(offset) = dialog.offset_mut() {
                    *offset = offset.saturating_sub(1);
                }
            }
            // The value keys. Without a way back, `trace_lines` clamps at 20
            // and there is no key that will ever bring it down again.
            (KeyCode::Char('l'), _) | (KeyCode::Right, _) => cycle_selected(dialog, live, false),
            (KeyCode::Char('h'), _) | (KeyCode::Left, _) => cycle_selected(dialog, live, true),
            // `s` and `d` reach their panels from anywhere, including from
            // inside another one -- the menu lists them, so they have to work
            // there or the menu is lying.
            //
            // And they carry the trail: pressing `s` while the menu is open
            // was reached THROUGH the menu, so esc goes back to it. Losing
            // that is why esc closed everything from a panel the menu opened.
            (KeyCode::Char('s'), _) if !matches!(dialog, Dialog::Settings { .. }) => {
                *open = Some(settings_dialog(live.interval_ms));
            }
            (KeyCode::Char('d'), _) if !matches!(dialog, Dialog::Doctor { .. }) => {
                *open = Some(doctor_dialog());
            }
            (KeyCode::Char('s'), _) => {
                it.notice = Some(
                    save_settings(dialog, live)
                        .unwrap_or_else(|error| format!("save failed: {error}")),
                );
            }
            (KeyCode::Char('r'), _) => {
                if let Dialog::Doctor {
                    report, taken_at, ..
                } = dialog
                {
                    *report = crate::agents::claude_bridge::doctor_report();
                    *taken_at = now_unix_ms();
                }
            }
            (KeyCode::Enter, _) | (KeyCode::Char('o'), _) => {
                cycle_selected(dialog, live, false);
                if let Dialog::Menu { cursor } = dialog {
                    *open = Some(match *cursor {
                        0 => settings_dialog(live.interval_ms),
                        _ => doctor_dialog(),
                    });
                }
            }
            _ => {}
        }
        return KeyOutcome::Handled;
    }

    // `s` and `d` are panel keys, not card-list keys: from the cards they do
    // nothing. One way in -- `x` -- is what makes them unambiguous, and `s`
    // three rows from `x` on the keyboard was being pressed for it.
    match (key.code, key.modifiers) {
        (KeyCode::Char('x'), _) | (KeyCode::Char('?'), _) => {
            if width < MIN_DIALOG_WIDTH || height < MIN_DIALOG_HEIGHT {
                it.notice = Some(format!(
                    "the frame is too small for a panel ({width}x{height}; \
                     needs {MIN_DIALOG_WIDTH}x{MIN_DIALOG_HEIGHT})"
                ));
                return KeyOutcome::Handled;
            }
            *open = Some(match key.code {
                KeyCode::Char('?') => Dialog::Keys { cursor: 0 },
                _ => Dialog::menu(),
            });
            KeyOutcome::Handled
        }
        _ => apply_key(key, it, live, rendered, viewport, total),
    }
}

/// "now" already reads as a time; everything else needs "ago". `format::age`
/// returns the bare quantity, so appending unconditionally gives "now ago".
fn taken_ago(taken_at: u64, now: u64) -> String {
    let age = crate::sidebar::format::age(taken_at, now);
    if age == "now" {
        age
    } else {
        format!("{age} ago")
    }
}

fn panel_for(
    dialog: &Dialog,
    live: &crate::sidebar::live::Live,
    _cfg: &crate::sidebar::config::Loaded,
    now: u64,
) -> crate::sidebar::dialog::Panel {
    use crate::sidebar::dialog::{Panel, Row};
    match dialog {
        Dialog::Menu { cursor } => Panel {
            title: "Menu".into(),
            rows: MENU
                .iter()
                .map(|(label, key)| Row::Entry {
                    label: (*label).into(),
                    value: (*key).into(),
                    enabled: true,
                })
                .collect(),
            footer: "j/k move · ↵ open · s settings · d doctor · esc close".into(),
            cursor: Some(*cursor),
            offset: 0,
        },
        Dialog::Keys { cursor } => Panel {
            title: "Keys".into(),
            rows: KEYS
                .iter()
                .map(|(key, what)| Row::Entry {
                    label: (*key).into(),
                    value: (*what).into(),
                    enabled: false,
                })
                .collect(),
            footer: "j/k move · esc close".into(),
            cursor: Some(*cursor),
            offset: 0,
        },
        Dialog::Settings {
            cursor,
            confirm,
            interval_at_open,
            ..
        } => {
            let rows: Vec<Row> = crate::sidebar::live::SETTINGS
                .iter()
                .map(|setting| Row::Entry {
                    label: setting.label().into(),
                    value: live.value(*setting),
                    enabled: true,
                })
                .collect();
            // No standing warning: `interval ms` only needs explaining at the
            // moment it matters, which is when you try to leave with it
            // changed. Until then it is three lines of the panel spent on
            // something that has not happened.
            if let Some(choice) = confirm {
                return Panel {
                    title: "Restart the daemon?".into(),
                    rows: vec![
                        Row::Warn(format!(
                            "interval ms is {} but the daemon is running {}. It reads the \
                             file once at startup, so this only takes effect when it \
                             restarts.",
                            live.interval_ms, interval_at_open
                        )),
                        Row::Rule,
                        Row::Entry {
                            label: "restart now".into(),
                            value: "save and reload the daemon".into(),
                            enabled: true,
                        },
                        Row::Entry {
                            label: "cancel".into(),
                            value: format!("put it back to {interval_at_open}"),
                            enabled: true,
                        },
                    ],
                    footer: "j/k choose · ↵ confirm".into(),
                    // Offset by the warning and the rule above the choices.
                    cursor: Some(choice + 2),
                    offset: 0,
                };
            }
            Panel {
                title: "Settings".into(),
                rows,
                footer: "j/k row · h/l value · s save · esc back".into(),
                cursor: Some(*cursor),
                offset: 0,
            }
        }
        Dialog::Doctor {
            offset,
            report,
            taken_at,
            ..
        } => Panel {
            title: "Doctor".into(),
            rows: match report {
                Ok(report) => doctor_rows(report),
                Err(error) => vec![Row::Note(error.clone())],
            },
            // `format::age`, the same relative form the cards use for their
            // traces. A raw epoch in milliseconds is not a time anyone reads.
            footer: format!(
                "taken {} · j/k scroll · r rebuild · esc back",
                taken_ago(*taken_at, now)
            ),
            cursor: None,
            offset: *offset,
        },
    }
}

fn apply_key(
    key: crossterm::event::KeyEvent,
    it: &mut Interaction,
    live: &mut crate::sidebar::live::Live,
    rendered: &Rendered,
    viewport: u16,
    total: usize,
) -> KeyOutcome {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return KeyOutcome::Quit,
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => return KeyOutcome::Quit,
        (KeyCode::Char('j'), _) => {
            move_cursor(&mut it.cursor, rendered, 1);
            it.follow = true;
        }
        (KeyCode::Char('k'), _) => {
            move_cursor(&mut it.cursor, rendered, -1);
            it.follow = true;
        }
        (KeyCode::Char('o'), _) | (KeyCode::Enter, _) => {
            if let Some(id) = it.cursor.clone() {
                if !it.toggled.remove(&id) {
                    it.toggled.insert(id);
                }
            }
            it.follow = true;
        }
        (KeyCode::Char('z'), _) => {
            live.hide_idle = !live.hide_idle;
            it.follow = true;
        }
        (KeyCode::Up, _) | (KeyCode::Down, _) | (KeyCode::PageUp, _) | (KeyCode::PageDown, _) => {
            if viewport == 0 {
                return KeyOutcome::Handled;
            }
            let step = match key.code {
                KeyCode::Up | KeyCode::Down => 1,
                _ => viewport.max(1),
            };
            let before = it.offset;
            it.offset = match key.code {
                KeyCode::Up | KeyCode::PageUp => it.offset.saturating_sub(step),
                _ => clamp_scroll(it.offset.saturating_add(step), total, viewport),
            };
            if it.offset != before {
                it.follow = false;
            }
        }
        _ => {}
    }
    KeyOutcome::Handled
}

fn to_ratatui(
    style: crate::sidebar::style::Style,
    theme: Theme,
    truecolor: bool,
) -> ratatui::style::Style {
    use ratatui::style::{Color, Modifier, Style as RStyle};
    let mut s = RStyle::default();
    s = match (style.role, theme) {
        (Role::Emphasis, Theme::Inherit) => s.add_modifier(Modifier::BOLD),
        (Role::Label, Theme::Inherit) | (Role::Rule, Theme::Inherit) => s.fg(Color::DarkGray),
        (Role::Body, Theme::Inherit) => s,
        (Role::Body, Theme::Lumon) => s.fg(Color::Rgb(0x7f, 0xe9, 0xff)),
        (Role::Emphasis, Theme::Lumon) => s.fg(Color::Rgb(0xe8, 0xf6, 0xfa)),
        (Role::Label, Theme::Lumon) | (Role::Rule, Theme::Lumon) => {
            s.fg(Color::Rgb(0x2c, 0x65, 0x77))
        }
    };
    if let Some(sem) = style.semantic {
        s = s.fg(match (sem, theme) {
            (Semantic::Good, Theme::Inherit) => Color::Green,
            (Semantic::Warn, Theme::Inherit) => Color::Yellow,
            (Semantic::Bad, Theme::Inherit) => Color::Red,
            (Semantic::Accent, Theme::Inherit) => Color::Blue,
            (Semantic::Good, Theme::Lumon) => Color::Rgb(0x6e, 0xe7, 0xa8),
            (Semantic::Warn, Theme::Lumon) => Color::Rgb(0xff, 0xd4, 0x79),
            (Semantic::Bad, Theme::Lumon) => Color::Rgb(0xff, 0x8b, 0x8b),
            (Semantic::Accent, Theme::Lumon) => Color::Rgb(0xb9, 0xf0, 0xff),
        });
    }
    if let Some((r, g, b)) = style.rgb {
        if matches!(theme, Theme::Lumon) || truecolor {
            s = s.fg(Color::Rgb(r, g, b));
        } else if let Some(a) = style.ansi {
            s = s.fg(Color::Indexed(a));
        }
    }
    if style.reverse {
        s = s.add_modifier(Modifier::REVERSED);
    }
    s
}

fn truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false)
}

pub fn run() -> i32 {
    let socket = crate::daemon::state_socket_path();
    let mut wire = match subscribe(&socket) {
        Ok(wire) => Some(wire),
        Err(message) => return draw_message_and_wait(&message),
    };
    let mut lost_at: Option<std::time::Instant> = None;
    let mut next_try = std::time::Instant::now();

    let guard = match TerminalGuard::enter() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("unable to initialize sidebar terminal: {error}");
            return 1;
        }
    };
    let mut terminal =
        match ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout())) {
            Ok(terminal) => terminal,
            Err(error) => {
                eprintln!("unable to initialize sidebar terminal: {error}");
                return 1;
            }
        };
    // A log-write failure changes only the notice wording; it is not counted as
    // another configuration problem (§4.3).
    let mut cfg = crate::sidebar::config::Loaded::load();
    cfg.write_problem_log();

    let mut state = State::default();
    // The daemon's interval is not in `Loaded` -- it deliberately skips
    // [daemon] -- so the panel reads it from the same place the daemon does.
    let mut live = crate::sidebar::live::Live::from_config(
        &cfg,
        crate::daemon::config::DaemonConfig::load()
            .interval
            .as_millis() as u32,
    );
    let mut open: Option<Dialog> = None;
    let mut it = Interaction {
        follow: true,
        ..Default::default()
    };
    let mut viewport: u16 = 0;
    let mut total: usize = 0;
    let mut frame_size = ratatui::layout::Size {
        width: crate::sidebar::view::MIN_WIDTH,
        height: 1,
    };
    let mut last_rendered = Rendered {
        scrollable: Vec::new(),
        pinned: Vec::new(),
        spans: Vec::new(),
    };
    let mut last_age_tick_ms: u64 = now_unix_ms();
    let mut dirty = true;

    loop {
        // Retrying is a state of the loop, not a loop of its own: sleeping
        // inside a reconnect means no redraw and no key handling until it
        // finishes, which is exactly when the reader most wants to see the
        // panel is still alive.
        if wire.is_none() {
            let now = std::time::Instant::now();
            if now >= next_try {
                match subscribe(&socket) {
                    Ok(fresh) => {
                        wire = Some(fresh);
                        lost_at = None;
                        it.notice = None;
                        // The state is left alone on purpose: subscribing
                        // replies with a full snapshot that replaces it, so
                        // clearing here would only blank the panel in the gap
                        // before that snapshot lands.
                    }
                    Err(_) => next_try = now + RECONNECT_EVERY,
                }
                dirty = true;
            }
            if let Some(lost) = lost_at {
                if now.duration_since(lost) > RECONNECT_FOR {
                    drop(terminal);
                    drop(guard);
                    return draw_message_and_wait("herdr-agent-watcher daemon did not come back");
                }
                it.notice = Some(format!(
                    "daemon disconnected; reconnecting… ({}s)",
                    now.duration_since(lost).as_secs()
                ));
            }
        }

        while let Some(rx) = wire.as_ref() {
            match rx.try_recv() {
                Ok(WireEvent::Line(line)) => match apply_line(&mut state, &line) {
                    Ok(()) => dirty = true,
                    Err(message) => {
                        drop(terminal);
                        drop(guard);
                        return draw_message_and_wait(&message);
                    }
                },
                // Not a dead end. The daemon going away is usually a restart
                // -- and since the settings panel can order one, it is a
                // disconnect this pane asked for. Drop the wire and let the
                // loop retry, so the cards stay on screen and keys keep
                // working while it does.
                Ok(WireEvent::Ended) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    wire = None;
                    lost_at = Some(std::time::Instant::now());
                    next_try = std::time::Instant::now();
                    dirty = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        let now = now_unix_ms();
        if age_tick_due(last_age_tick_ms, now) {
            last_age_tick_ms = now;
            dirty = true;
        }

        if dirty {
            frame_size = terminal.size().unwrap_or(ratatui::layout::Size {
                width: crate::sidebar::view::MIN_WIDTH,
                height: 1,
            });
            let size = frame_size;
            it.toggled.retain(|id| state.panes.contains_key(id));

            let prev_index = index_of(&last_rendered, it.cursor.as_deref());
            let prev_span = it
                .cursor
                .as_deref()
                .and_then(|id| last_rendered.span_for(id));

            let mut out = crate::sidebar::view::render(
                &state,
                &view_input(&cfg, &live, &it.toggled, it.cursor.as_deref()),
                size.width,
                now,
            );
            let recovered = reconcile_cursor(&mut it.cursor, &out, prev_index);
            if recovered {
                out = crate::sidebar::view::render(
                    &state,
                    &view_input(&cfg, &live, &it.toggled, it.cursor.as_deref()),
                    size.width,
                    now,
                );
            }

            if let Some(notice) = &it.notice {
                out.pinned
                    .push(vec![crate::sidebar::view::Span::body(notice.clone())]);
            }

            let pinned_keep = out.pinned.len().min(size.height as usize);
            viewport = size.height.saturating_sub(pinned_keep as u16);
            total = out.scrollable.len();
            it.offset = match it.cursor.as_deref().and_then(|id| out.span_for(id)) {
                Some(span) if it.follow || recovered => {
                    ensure_visible(it.offset, span, viewport, total)
                }
                Some(span) => reanchor(it.offset, prev_span.unwrap_or(span), span, viewport, total),
                None => 0,
            };

            let color = truecolor();
            let body: Vec<_> = out
                .scrollable
                .iter()
                .map(|line| to_line(line, live.theme, color))
                .collect();
            let foot: Vec<_> = out.pinned[out.pinned.len() - pinned_keep..]
                .iter()
                .map(|line| to_line(line, live.theme, color))
                .collect();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    if matches!(live.theme, Theme::Lumon) {
                        frame.render_widget(
                            ratatui::widgets::Block::default().style(
                                ratatui::style::Style::default()
                                    .bg(ratatui::style::Color::Rgb(0x04, 0x14, 0x1c)),
                            ),
                            area,
                        );
                    }
                    let split = ratatui::layout::Layout::vertical([
                        ratatui::layout::Constraint::Length(viewport),
                        ratatui::layout::Constraint::Length(pinned_keep as u16),
                    ])
                    .split(area);
                    frame.render_widget(
                        Paragraph::new(body.clone()).scroll((it.offset, 0)),
                        split[0],
                    );
                    frame.render_widget(Paragraph::new(foot.clone()), split[1]);
                    if let Some(dialog) = open.as_ref() {
                        let panel = panel_for(dialog, &live, &cfg, now);
                        let w = area.width.min(60);
                        let h = area.height.saturating_sub(2).min(20);
                        let rect = ratatui::layout::Rect {
                            x: area.x + (area.width - w) / 2,
                            y: area.y + (area.height - h) / 2,
                            width: w,
                            height: h,
                        };
                        let lines: Vec<_> = crate::sidebar::dialog::render(&panel, w, h)
                            .iter()
                            .map(|line| to_line(line, live.theme, color))
                            .collect();
                        frame.render_widget(ratatui::widgets::Clear, rect);
                        frame.render_widget(Paragraph::new(lines), rect);
                    }
                })
                .expect("draw sidebar");
            last_rendered = out;
            dirty = false;
        }

        if crossterm::event::poll(Duration::from_millis(100)).unwrap_or(false) {
            match crossterm::event::read() {
                Ok(Event::Key(key)) => {
                    dirty = true;
                    if let KeyOutcome::Quit = route(
                        key,
                        &mut open,
                        &mut it,
                        &mut live,
                        &last_rendered,
                        viewport,
                        total,
                        frame_size.width,
                        frame_size.height,
                    ) {
                        return 0;
                    }
                }
                Ok(Event::Resize(_, _)) => dirty = true,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::layout::LineSpan;
    use crate::sidebar::view::Span;

    fn two_cards() -> Rendered {
        Rendered {
            scrollable: (0..40).map(|_| vec![Span::body("x")]).collect(),
            pinned: vec![],
            spans: vec![
                (
                    "a".into(),
                    LineSpan {
                        start: 0,
                        height: 3,
                    },
                ),
                (
                    "b".into(),
                    LineSpan {
                        start: 4,
                        height: 20,
                    },
                ),
            ],
        }
    }

    fn press(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn live_default() -> crate::sidebar::live::Live {
        crate::sidebar::live::Live::from(&crate::sidebar::config::Loaded::from_missing())
    }

    #[test]
    fn the_age_tick_fires_once_a_minute_and_not_before() {
        assert!(!age_tick_due(0, 59_999));
        assert!(age_tick_due(0, 60_000));
        assert!(!age_tick_due(60_000, 90_000));
        assert!(!age_tick_due(90_000, 0));
    }

    #[test]
    fn x_opens_the_menu_and_esc_closes_it() {
        let r = two_cards();
        let mut it = Interaction {
            follow: true,
            ..Default::default()
        };
        let mut live = live_default();
        let mut open: Option<Dialog> = None;

        route(
            press(KeyCode::Char('x')),
            &mut open,
            &mut it,
            &mut live,
            &r,
            10,
            40,
            40,
            20,
        );
        assert!(open.is_some(), "x opens the menu");

        route(
            press(KeyCode::Esc),
            &mut open,
            &mut it,
            &mut live,
            &r,
            10,
            40,
            40,
            20,
        );
        assert!(open.is_none(), "esc closes it rather than quitting");
    }

    /// One key is not enough: a router that forwards `k` and `PageDown` passes
    /// a test that only presses `j`.
    #[test]
    fn no_key_reaches_the_card_list_while_a_panel_is_open() {
        let r = two_cards();
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('o'),
            KeyCode::Enter,
            KeyCode::Char('z'),
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ] {
            let mut it = Interaction {
                follow: true,
                offset: 7,
                ..Default::default()
            };
            it.cursor = Some("a".into());
            let mut live = live_default();
            let before = (
                it.offset,
                it.cursor.clone(),
                it.toggled.clone(),
                live.hide_idle,
            );
            let mut open = Some(Dialog::menu());

            route(
                press(code),
                &mut open,
                &mut it,
                &mut live,
                &r,
                10,
                40,
                40,
                20,
            );
            assert_eq!(
                (
                    it.offset,
                    it.cursor.clone(),
                    it.toggled.clone(),
                    live.hide_idle
                ),
                before,
                "{code:?} reached the card list"
            );
        }
    }

    #[test]
    fn a_panels_close_keys_close_it_rather_than_quitting() {
        let r = two_cards();
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut it = Interaction::default();
            let mut live = live_default();
            let mut open = Some(Dialog::menu());
            let outcome = route(
                press(code),
                &mut open,
                &mut it,
                &mut live,
                &r,
                10,
                40,
                40,
                20,
            );
            assert!(
                matches!(outcome, KeyOutcome::Handled),
                "{code:?} quit the sidebar"
            );
            assert!(open.is_none());
        }
    }

    #[test]
    fn the_same_keys_still_quit_when_no_panel_is_open() {
        let r = two_cards();
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut it = Interaction::default();
            let mut live = live_default();
            let mut open = None;
            assert!(matches!(
                route(
                    press(code),
                    &mut open,
                    &mut it,
                    &mut live,
                    &r,
                    10,
                    40,
                    40,
                    20
                ),
                KeyOutcome::Quit
            ));
        }
    }

    #[test]
    fn a_fresh_report_is_not_taken_now_ago() {
        assert_eq!(taken_ago(1_000, 1_000), "now");
        assert_eq!(taken_ago(1_000, 1_000 + 120_000), "2m ago");
    }

    /// The menu lists `s` and `d`, so reaching a panel that way was reached
    /// through the menu -- and esc has somewhere to go back to. Without this,
    /// the advertised shortcut and the `↵` beside it behave differently.
    #[test]
    fn a_panel_opened_from_the_menu_goes_back_to_it_however_it_was_opened() {
        let r = two_cards();
        for code in [KeyCode::Enter, KeyCode::Char('s')] {
            let mut it = Interaction::default();
            let mut live = live_default();
            let mut open = Some(Dialog::menu());
            route(
                press(code),
                &mut open,
                &mut it,
                &mut live,
                &r,
                10,
                40,
                60,
                24,
            );
            assert!(
                matches!(open, Some(Dialog::Settings { .. })),
                "{code:?} did not open settings"
            );
            route(
                press(KeyCode::Esc),
                &mut open,
                &mut it,
                &mut live,
                &r,
                10,
                40,
                60,
                24,
            );
            assert!(
                matches!(open, Some(Dialog::Menu { .. })),
                "{code:?}: esc should step back to the menu, not close everything"
            );
        }
    }

    /// A config the panel cannot parse still opens -- showing the values in
    /// force, which are the defaults `Loaded` fell back to -- and refuses to
    /// save. Saving anyway would discard whatever the operator was in the
    /// middle of writing.
    #[test]
    fn a_broken_config_opens_the_panel_and_refuses_the_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let broken = "[daemon]\ninterval_ms = 3000\n[list\nscope = \"workspace\"\n";
        std::fs::write(&path, broken).unwrap();

        let live = live_default();
        let mut dialog = Dialog::Settings {
            cursor: 0,
            interval_at_open: crate::sidebar::live::DEFAULT_INTERVAL_MS,
            confirm: None,
            dirty: vec![crate::sidebar::live::Setting::Sort],
            path: Some(path.clone()),
            source: Ok(Some(broken.to_string())),
        };
        let error = save_settings(&mut dialog, &live).expect_err("must refuse");
        assert!(error.contains("not valid TOML"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            broken,
            "the file the operator was editing is untouched"
        );
    }

    /// The other broken case: the file could not be read at all.
    #[test]
    fn an_unreadable_config_refuses_the_save_with_the_read_error() {
        let live = live_default();
        let mut dialog = Dialog::Settings {
            cursor: 0,
            interval_at_open: crate::sidebar::live::DEFAULT_INTERVAL_MS,
            confirm: None,
            dirty: vec![crate::sidebar::live::Setting::Sort],
            path: Some("/nowhere/config.toml".into()),
            source: Err("read /nowhere/config.toml: no such file".into()),
        };
        let error = save_settings(&mut dialog, &live).expect_err("must refuse");
        assert!(error.contains("no such file"), "{error}");
    }

    fn settings_with_interval(at_open: u32, confirm: Option<usize>) -> Dialog {
        Dialog::Settings {
            cursor: 0,
            dirty: Vec::new(),
            path: None,
            source: Ok(None),
            interval_at_open: at_open,
            confirm,
        }
    }

    /// Leaving with the value and the running daemon apart would leave a
    /// setting that looks applied and is not.
    #[test]
    fn esc_with_a_changed_interval_raises_a_prompt_instead_of_closing() {
        let r = two_cards();
        let mut it = Interaction::default();
        let mut live = live_default();
        live.interval_ms = 5000;
        let mut open = Some(settings_with_interval(1000, None));

        route(
            press(KeyCode::Esc),
            &mut open,
            &mut it,
            &mut live,
            &r,
            10,
            40,
            60,
            24,
        );
        assert!(
            matches!(
                open,
                Some(Dialog::Settings {
                    confirm: Some(_),
                    ..
                })
            ),
            "esc should ask, not close"
        );
    }

    #[test]
    fn an_unchanged_interval_leaves_esc_alone() {
        let r = two_cards();
        let mut it = Interaction::default();
        let mut live = live_default();
        let interval = live.interval_ms;
        let mut open = Some(settings_with_interval(interval, None));

        route(
            press(KeyCode::Esc),
            &mut open,
            &mut it,
            &mut live,
            &r,
            10,
            40,
            60,
            24,
        );
        assert!(
            matches!(open, Some(Dialog::Menu { .. })),
            "nothing to ask about, so esc is an ordinary step back"
        );
    }

    /// Mandatory means mandatory: every key that is not a choice is a way of
    /// not deciding.
    #[test]
    fn the_prompt_cannot_be_dismissed() {
        let r = two_cards();
        for code in [
            KeyCode::Esc,
            KeyCode::Char('q'),
            KeyCode::Char('s'),
            KeyCode::Char('d'),
            KeyCode::Char('x'),
            KeyCode::Char('h'),
        ] {
            let mut it = Interaction::default();
            let mut live = live_default();
            live.interval_ms = 5000;
            let mut open = Some(settings_with_interval(1000, Some(0)));
            route(
                press(code),
                &mut open,
                &mut it,
                &mut live,
                &r,
                10,
                40,
                60,
                24,
            );
            assert!(
                matches!(
                    open,
                    Some(Dialog::Settings {
                        confirm: Some(_),
                        ..
                    })
                ),
                "{code:?} dismissed a mandatory prompt"
            );
        }
    }

    #[test]
    fn cancelling_puts_the_interval_back() {
        let r = two_cards();
        let mut it = Interaction::default();
        let mut live = live_default();
        live.interval_ms = 5000;
        let mut open = Some(settings_with_interval(1000, Some(1)));
        route(
            press(KeyCode::Enter),
            &mut open,
            &mut it,
            &mut live,
            &r,
            10,
            40,
            60,
            24,
        );
        assert_eq!(live.interval_ms, 1000, "cancel means it was never changed");
        assert!(matches!(open, Some(Dialog::Menu { .. })));
        assert!(
            it.notice.as_deref().is_some_and(|n| n.contains("1000")),
            "{:?}",
            it.notice
        );
    }

    /// The cards have one way into the dialogs, and `s`/`d` are not it: they
    /// are panel keys. `s` sits three rows from `x` and was being pressed for
    /// it, which is the whole reason the cards ignore both.
    #[test]
    fn the_cards_take_only_x_into_the_dialogs() {
        let r = two_cards();
        let mut it = Interaction::default();
        let mut live = live_default();
        let mut open = None;
        let go = |key, open: &mut Option<Dialog>, live: &mut _, it: &mut _| {
            route(press(key), open, it, live, &r, 10, 40, 60, 24);
        };

        for key in [KeyCode::Char('s'), KeyCode::Char('d')] {
            go(key, &mut open, &mut live, &mut it);
            assert!(open.is_none(), "the cards ignore panel keys");
        }

        go(KeyCode::Char('x'), &mut open, &mut live, &mut it);
        assert!(matches!(open, Some(Dialog::Menu { .. })), "x is the way in");

        // And from there they are the accelerators the menu advertises.
        go(KeyCode::Char('d'), &mut open, &mut live, &mut it);
        assert!(
            matches!(open, Some(Dialog::Doctor { .. })),
            "d works in a panel"
        );

        go(KeyCode::Esc, &mut open, &mut live, &mut it);
        assert!(
            matches!(open, Some(Dialog::Menu { .. })),
            "esc is one level back"
        );
    }

    #[test]
    fn the_notice_lasts_exactly_one_key() {
        let r = two_cards();
        let mut it = Interaction::default();
        let mut live = live_default();
        let mut open = None;
        route(
            press(KeyCode::Char('x')),
            &mut open,
            &mut it,
            &mut live,
            &r,
            10,
            40,
            19,
            20,
        );
        assert!(it.notice.is_some(), "the refusal said why");

        route(
            press(KeyCode::Char('j')),
            &mut open,
            &mut it,
            &mut live,
            &r,
            10,
            40,
            19,
            20,
        );
        assert!(
            it.notice.is_none(),
            "and did not linger through the next key"
        );
    }

    #[test]
    fn a_frame_too_small_declines_to_open() {
        let r = two_cards();
        for (w, h) in [(19u16, 20u16), (40, 7)] {
            let mut it = Interaction::default();
            let mut live = live_default();
            let mut open = None;
            route(
                press(KeyCode::Char('x')),
                &mut open,
                &mut it,
                &mut live,
                &r,
                10,
                40,
                w,
                h,
            );
            assert!(open.is_none(), "{w}x{h} should decline");
            assert!(
                it.notice.as_deref().is_some_and(|n| n.contains("small")),
                "and say why: {:?}",
                it.notice
            );
        }
    }

    /// Comparing two hard-coded arrays proves nothing: deleting a route
    /// changes neither. This presses every key the sheet describes, through
    /// the real router, and asserts it did something. A key documented but not
    /// routed falls to the router's `_ => {}` and changes nothing.
    #[test]
    fn every_key_the_sheet_describes_does_something() {
        let r = two_cards();
        for (key, code, modifiers, start, context) in ROUTED {
            let mut it = Interaction {
                follow: true,
                offset: 5,
                cursor: Some(start.into()),
                ..Default::default()
            };
            let mut live = live_default();
            let mut open = context;
            let before = (
                it.offset,
                it.cursor.clone(),
                it.toggled.clone(),
                // The whole of `live`, not just `hide_idle`: a key that only
                // moves a setting is still a key that did something.
                live.clone(),
                open.is_some(),
                open.as_ref().map(std::mem::discriminant),
                doctor_taken_at(&open),
            );
            let outcome = route(
                crossterm::event::KeyEvent::new(code, modifiers),
                &mut open,
                &mut it,
                &mut live,
                &r,
                10,
                40,
                60,
                24,
            );
            let after = (
                it.offset,
                it.cursor.clone(),
                it.toggled.clone(),
                live.clone(),
                open.is_some(),
                open.as_ref().map(std::mem::discriminant),
                doctor_taken_at(&open),
            );
            assert!(
                matches!(outcome, KeyOutcome::Quit) || before != after,
                "{key} is in the sheet but the router ignores it"
            );
        }
    }

    /// The test above drives ROUTED; the panel renders KEYS. Without this,
    /// renaming a key in KEYS -- the array the reader actually sees -- passes.
    #[test]
    fn the_sheet_and_the_driven_table_describe_the_same_keys() {
        let sheet: std::collections::BTreeSet<&str> = KEYS.iter().map(|(key, _)| *key).collect();
        let driven: std::collections::BTreeSet<&str> =
            ROUTED.iter().map(|(key, ..)| *key).collect();
        let pending: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        assert_eq!(
            sheet
                .difference(&driven)
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            pending,
            "a key in the sheet that nothing presses"
        );
        assert!(
            driven.difference(&sheet).next().is_none(),
            "a key pressed by the table but missing from the sheet"
        );
    }

    /// Through the real router, and asserting on what gets drawn. A panel
    /// wired to nothing passes every test in `live.rs`.
    #[test]
    fn changing_sort_through_the_panel_reorders_the_drawn_cards() {
        use crate::sidebar::live::Setting;
        let mut state = crate::sidebar::reducer::State::default();
        for (id, position, seq) in [("first", 0u32, 1u64), ("second", 1, 99)] {
            let mut t = crate::daemon::store::PaneTelemetry::with_agent("claude");
            t.position = Some(position);
            t.updated_seq = seq;
            t.card_state = crate::daemon::store::CardState::Running;
            state.panes.insert(id.to_string(), t);
        }
        // Somewhere for a save to land, so the test can prove none did.
        let config_dir = tempfile::tempdir().expect("tempdir");
        let cfg = crate::sidebar::config::Loaded::from_missing();
        let toggled = std::collections::HashSet::new();
        let mut live = crate::sidebar::live::Live::from(&cfg);

        let order = |live: &crate::sidebar::live::Live| {
            crate::sidebar::view::render(&state, &view_input(&cfg, live, &toggled, None), 60, 0)
                .spans
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&live), vec!["first", "second"], "position order");

        // The panel's ↵ on the sort row, through the router.
        let mut it = Interaction::default();
        let mut open = Some(Dialog::Settings {
            cursor: 0,
            interval_at_open: crate::sidebar::live::DEFAULT_INTERVAL_MS,
            confirm: None,
            dirty: Vec::new(),
            path: None,
            source: Ok(None),
        });
        let r = two_cards();
        crate::test_env::with_env(
            &[
                ("HERDR_PLUGIN_CONFIG_DIR", Some(config_dir.path().into())),
                ("HERDR_WORKSPACE_ID", Some("w4".into())),
            ],
            || {
                route(
                    press(KeyCode::Enter),
                    &mut open,
                    &mut it,
                    &mut live,
                    &r,
                    10,
                    40,
                    60,
                    24,
                );
            },
        );

        assert_eq!(live.value(Setting::Sort), "smart");
        assert_eq!(
            order(&live),
            vec!["second", "first"],
            "smart ranks by recency"
        );

        // "with no file written" has to observe a file. A route that also
        // saved would pass every assertion above.
        assert!(
            !config_dir.path().join("config.toml").exists(),
            "cycling a setting must not write anything"
        );
    }

    fn rows_text(rows: &[crate::sidebar::dialog::Row]) -> String {
        use crate::sidebar::dialog::Row;
        rows.iter()
            .map(|row| match row {
                Row::Entry { label, value, .. } => format!("{label} {value}"),
                Row::Note(note) | Row::Warn(note) => note.clone(),
                Row::Rule => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A mapper that drops pane findings passes every other test here.
    #[test]
    fn every_part_of_a_report_survives_flattening() {
        use crate::agents::doctor::{Check, CheckId, Level, PaneFinding, Remedy, Report};
        let report = Report {
            checks: vec![
                Check {
                    id: CheckId::ConfigValid,
                    level: Level::Ok,
                    summary: "config.toml accepted".into(),
                    evidence: None,
                    remedy: None,
                },
                Check {
                    id: CheckId::DaemonReachable,
                    level: Level::Fail,
                    summary: "daemon is not answering".into(),
                    evidence: Some("/tmp/state.sock".into()),
                    remedy: Some(Remedy::PluginAction {
                        id: "restart-daemon",
                    }),
                },
            ],
            panes: vec![PaneFinding {
                pane_id: "w1:p1".into(),
                agent_session: Some("s1".into()),
                level: Level::Warn,
                summary: "no metrics yet".into(),
                window_size: Some(200_000),
                shadowed_by: None,
                remedy: Some(Remedy::ReopenPane),
            }],
        };
        let rows = doctor_rows(&report);
        let text = rows_text(&rows);
        for needle in [
            "config.toml accepted",
            "daemon is not answering",
            "/tmp/state.sock",
            "restart-daemon",
            "w1:p1",
            "no metrics yet",
            "200000",
            "reopen",
        ] {
            assert!(text.contains(needle), "{needle} was dropped:\n{text}");
        }
    }

    #[test]
    fn cursor_keys_move_the_selection_and_resume_following() {
        let r = two_cards();
        let mut live = live_default();
        let mut it = Interaction {
            cursor: Some("a".into()),
            follow: false,
            ..Default::default()
        };
        apply_key(press(KeyCode::Char('j')), &mut it, &mut live, &r, 10, 40);
        assert_eq!(it.cursor.as_deref(), Some("b"));
        assert!(it.follow, "moving the cursor re-attaches (§3.5)");
        apply_key(press(KeyCode::Char('j')), &mut it, &mut live, &r, 10, 40);
        assert_eq!(it.cursor.as_deref(), Some("b"), "no wrapping (§3.2)");
    }

    #[test]
    fn o_toggles_the_selected_card_and_z_flips_the_idle_filter() {
        let r = two_cards();
        let mut live = live_default();
        let mut it = Interaction {
            cursor: Some("b".into()),
            ..Default::default()
        };
        apply_key(press(KeyCode::Char('o')), &mut it, &mut live, &r, 10, 40);
        assert!(it.toggled.contains("b"));
        apply_key(press(KeyCode::Enter), &mut it, &mut live, &r, 10, 40);
        assert!(it.toggled.is_empty(), "the same key closes it again");
        apply_key(press(KeyCode::Char('z')), &mut it, &mut live, &r, 10, 40);
        assert!(live.hide_idle);
    }

    #[test]
    fn paging_detaches_following_only_when_the_offset_actually_moves() {
        let r = two_cards();
        let mut live = live_default();
        let mut it = Interaction {
            follow: true,
            ..Default::default()
        };
        apply_key(press(KeyCode::PageDown), &mut it, &mut live, &r, 10, 40);
        assert_eq!(it.offset, 10, "one viewport");
        assert!(!it.follow, "the user took the wheel");

        let mut it = Interaction {
            follow: true,
            ..Default::default()
        };
        apply_key(press(KeyCode::PageUp), &mut it, &mut live, &r, 10, 40);
        assert_eq!(it.offset, 0);
        assert!(
            it.follow,
            "a no-op key must not silently stop following (§3.5)"
        );

        let mut it = Interaction {
            follow: true,
            ..Default::default()
        };
        apply_key(press(KeyCode::PageDown), &mut it, &mut live, &r, 0, 40);
        assert!(it.follow);
    }

    #[test]
    fn every_config_key_reaches_the_view_input_the_shell_builds() {
        let cfg = crate::sidebar::config::Loaded::from_toml(
            "[appearance]\ntheme = \"lumon\"\nagent_mark = \"initial\"\n\n\
             [cards]\nauto_expand = \"all\"\ntool_calls = \"jar\"\ntrace_lines = 9\n\n\
             [list]\nsort = \"group\"\nhide_idle = true\n",
        );
        assert_eq!(cfg.status.problems, 0, "the fixture itself must be valid");

        let toggled = std::collections::HashSet::new();
        let live = crate::sidebar::live::Live::from(&cfg);
        let v = view_input(&cfg, &live, &toggled, Some("p1"));
        assert_eq!(v.theme, Theme::Lumon);
        assert_eq!(v.agent_mark, crate::sidebar::config::AgentMark::Initial);
        assert_eq!(v.auto_expand, crate::sidebar::config::AutoExpand::All);
        assert_eq!(v.tool_calls, crate::sidebar::config::ToolCallStyle::Jar);
        assert_eq!(v.trace_lines, 9);
        assert_eq!(v.sort, crate::sidebar::view::Sort::Group);
        assert!(v.hide_idle);
        assert_eq!(v.cursor, Some("p1"));
        assert_eq!(v.config.problems, 0);
        assert!(
            std::ptr::eq(v.agents, &cfg.appearances),
            "the resolved appearances, not a fresh built-in table"
        );
    }

    #[test]
    fn a_sixteen_colour_terminal_still_tells_the_agents_apart() {
        use ratatui::style::Color;
        for id in crate::sidebar::agent_ids::CANONICAL_IDS {
            let look = crate::sidebar::agent_ids::appearance(id).expect("known id");
            let mark = crate::sidebar::style::Style {
                role: Role::Body,
                rgb: Some(look.rgb),
                ansi: Some(look.ansi),
                ..Default::default()
            };
            assert_eq!(
                to_ratatui(mark, Theme::Inherit, false).fg,
                Some(Color::Indexed(look.ansi)),
                "{id} must survive a terminal without truecolor (§1.1)"
            );
            assert_eq!(
                to_ratatui(mark, Theme::Inherit, true).fg,
                Some(Color::Rgb(look.rgb.0, look.rgb.1, look.rgb.2)),
                "{id} uses its brand colour when the terminal can show it"
            );
            assert_eq!(
                to_ratatui(mark, Theme::Lumon, false).fg,
                Some(Color::Rgb(look.rgb.0, look.rgb.1, look.rgb.2)),
                "lumon is a committed RGB palette and does not consult the terminal"
            );
        }
    }

    #[test]
    fn quit_keys_quit_and_nothing_else_does() {
        let r = two_cards();
        let mut it = Interaction::default();
        let mut live = live_default();
        assert!(matches!(
            apply_key(press(KeyCode::Char('q')), &mut it, &mut live, &r, 10, 40),
            KeyOutcome::Quit
        ));
        assert!(matches!(
            apply_key(press(KeyCode::Esc), &mut it, &mut live, &r, 10, 40),
            KeyOutcome::Quit
        ));
        let ctrl_c = crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            apply_key(ctrl_c, &mut it, &mut live, &r, 10, 40),
            KeyOutcome::Quit
        ));
        let plain_c = press(KeyCode::Char('c'));
        assert!(matches!(
            apply_key(plain_c, &mut it, &mut live, &r, 10, 40),
            KeyOutcome::Handled
        ));
    }

    #[test]
    fn cursor_recovery_picks_the_nearest_surviving_row() {
        let rendered = two_cards();
        let mut cursor = Some("gone".to_string());
        assert!(reconcile_cursor(&mut cursor, &rendered, 5));
        assert_eq!(cursor.as_deref(), Some("b"), "clamps to the last row");

        let mut cursor = Some("a".to_string());
        assert!(!reconcile_cursor(&mut cursor, &rendered, 0));
    }
    /// The trip the reader actually makes: `x` for the menu, `\u{21b5}` into a
    /// panel, `esc` back to the menu, and out. Every earlier esc test starts
    /// with the panel already open, so none of them cross the menu boundary
    /// that the reader complains about.
    #[test]
    fn esc_walks_back_out_through_the_menu_one_level_at_a_time() {
        let r = two_cards();
        let mut it = Interaction::default();
        let mut live = live_default();
        let mut open = None;
        let go = |key, open: &mut Option<Dialog>, live: &mut _, it: &mut _| {
            route(press(key), open, it, live, &r, 10, 40, 60, 24);
        };

        go(KeyCode::Char('x'), &mut open, &mut live, &mut it);
        assert!(
            matches!(open, Some(Dialog::Menu { .. })),
            "x opens the menu"
        );

        go(KeyCode::Enter, &mut open, &mut live, &mut it);
        assert!(
            matches!(open, Some(Dialog::Settings { .. })),
            "\u{21b5} on the first row opens settings"
        );

        go(KeyCode::Esc, &mut open, &mut live, &mut it);
        assert!(
            matches!(open, Some(Dialog::Menu { .. })),
            "esc goes back to the menu it came from"
        );

        go(KeyCode::Esc, &mut open, &mut live, &mut it);
        assert!(open.is_none(), "esc from the menu closes");
    }

    /// Same trip, but with the interval touched on the way in, so the
    /// mandatory prompt stands between the panel and the menu.
    #[test]
    fn answering_the_prompt_still_lands_on_the_menu() {
        let r = two_cards();
        let mut it = Interaction::default();
        let mut live = live_default();
        let mut open = None;
        let go = |key, open: &mut Option<Dialog>, live: &mut _, it: &mut _| {
            route(press(key), open, it, live, &r, 10, 40, 60, 24);
        };

        go(KeyCode::Char('x'), &mut open, &mut live, &mut it);
        go(KeyCode::Enter, &mut open, &mut live, &mut it);
        // Down to the interval row, then move it.
        let interval = crate::sidebar::live::SETTINGS
            .iter()
            .position(|s| matches!(s, crate::sidebar::live::Setting::IntervalMs))
            .expect("an interval row");
        for _ in 0..interval {
            go(KeyCode::Char('j'), &mut open, &mut live, &mut it);
        }
        let before = live.interval_ms;
        go(KeyCode::Char('l'), &mut open, &mut live, &mut it);
        assert_ne!(live.interval_ms, before, "l moves the interval");

        go(KeyCode::Esc, &mut open, &mut live, &mut it);
        assert!(
            matches!(
                open,
                Some(Dialog::Settings {
                    confirm: Some(_),
                    ..
                })
            ),
            "esc raises the prompt instead of leaving"
        );

        // Cancel: put it back, and still land on the menu.
        go(KeyCode::Char('j'), &mut open, &mut live, &mut it);
        go(KeyCode::Enter, &mut open, &mut live, &mut it);
        assert_eq!(live.interval_ms, before, "cancel puts the interval back");
        assert!(
            matches!(open, Some(Dialog::Menu { .. })),
            "answering lands on the menu it came from"
        );
    }
}
