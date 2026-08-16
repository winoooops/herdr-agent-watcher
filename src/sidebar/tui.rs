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
    Menu { cursor: usize },
    Keys { cursor: usize },
}

impl Dialog {
    fn menu() -> Self {
        Dialog::Menu { cursor: 0 }
    }

    fn cursor_mut(&mut self) -> &mut usize {
        match self {
            Dialog::Menu { cursor } | Dialog::Keys { cursor } => cursor,
        }
    }

    fn len(&self) -> usize {
        match self {
            Dialog::Menu { .. } => MENU.len(),
            // The sheet's contents arrive in Task 4; naming KEYS here would
            // make this task's commit fail to compile on its own.
            Dialog::Keys { .. } => 0,
        }
    }
}

const MENU: [(&str, &str); 2] = [("Settings", "s"), ("Doctor", "d")];

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
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                return KeyOutcome::Quit
            }
            (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => *open = None,
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                let last = dialog.len().saturating_sub(1);
                let cursor = dialog.cursor_mut();
                *cursor = (*cursor + 1).min(last);
            }
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                let cursor = dialog.cursor_mut();
                *cursor = cursor.saturating_sub(1);
            }
            _ => {}
        }
        return KeyOutcome::Handled;
    }

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

fn panel_for(
    dialog: &Dialog,
    _live: &crate::sidebar::live::Live,
    _cfg: &crate::sidebar::config::Loaded,
) -> crate::sidebar::dialog::Panel {
    use crate::sidebar::dialog::{Panel, Row};
    match dialog {
        Dialog::Menu { cursor } => Panel {
            title: "Agent Watcher".into(),
            rows: MENU
                .iter()
                .map(|(label, key)| Row::Entry {
                    label: (*label).into(),
                    value: (*key).into(),
                    enabled: true,
                })
                .collect(),
            footer: "j/k move · ↵ open · esc close".into(),
            cursor: *cursor,
        },
        Dialog::Keys { cursor } => Panel {
            title: "Keys".into(),
            rows: Vec::new(),
            footer: "esc close".into(),
            cursor: *cursor,
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
    let Ok(mut stream) = UnixStream::connect(&socket) else {
        return draw_message_and_wait(&format!(
            "herdr-agent-watcher daemon is not running\n(no state socket at {})",
            socket.display()
        ));
    };
    if stream.write_all(b"{\"method\":\"subscribe\"}\n").is_err() {
        return draw_message_and_wait("herdr-agent-watcher daemon closed the state socket");
    }
    let wire = spawn_reader(stream);

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
    let mut live = crate::sidebar::live::Live::from(&cfg);
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
        loop {
            match wire.try_recv() {
                Ok(WireEvent::Line(line)) => match apply_line(&mut state, &line) {
                    Ok(()) => dirty = true,
                    Err(message) => {
                        drop(terminal);
                        drop(guard);
                        return draw_message_and_wait(&message);
                    }
                },
                Ok(WireEvent::Ended) => {
                    drop(terminal);
                    drop(guard);
                    return draw_message_and_wait("herdr-agent-watcher daemon disconnected");
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    drop(terminal);
                    drop(guard);
                    return draw_message_and_wait("herdr-agent-watcher daemon disconnected");
                }
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
                        let panel = panel_for(dialog, &live, &cfg);
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
}
