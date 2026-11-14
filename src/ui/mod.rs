//! ratatui rendering and key handling on top of the pure `Core`.

mod keys;
pub mod view;

pub use keys::{UiOutcome, on_key, on_mouse, on_paste};
pub use view::{Mode, View};

use std::ops::Range;
use std::time::Duration;

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TLine, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::core::{Core, PauseReason, Tab, TabState};
use crate::event::Holder;
use crate::lines::LineKind;
use crate::scrollback::{VisibleRow, strip_sgr, wrap_segments};
use crate::store::LineEnding;
use keys::BAUDS;

/// `HH:MM:SS.mmm` plus one space.
const TS_WIDTH: u16 = 13;
const STATUS_TTL: Duration = Duration::from_secs(5);

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn glyph(s: &TabState) -> &'static str {
    match s {
        TabState::Connected => "●",
        TabState::Paused(_) => "⏸",
        TabState::Waiting | TabState::Connecting => "○",
        TabState::Error(_) => "!",
    }
}

fn rule(f: &mut Frame, r: Rect) {
    f.render_widget(Paragraph::new("─".repeat(r.width as usize)).style(dim()), r);
}

pub fn render(f: &mut Frame, core: &Core, view: &View) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        view.last_log_rows.set(0);
        return;
    }
    let [bar, rule1, log, rule2, send, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    view.last_log_rows.set(log.height);
    let tab = core.tabs().get(core.focus());

    render_bar(f, core, tab, bar);
    rule(f, rule1);
    render_log(f, tab, view, log);
    rule(f, rule2);
    render_send(f, tab, view, send);
    render_status(f, tab, view, status);

    match view.mode {
        Mode::Help => overlay_help(f, area),
        Mode::BaudPicker => overlay_baud(f, view, area),
        Mode::OtherPorts => overlay_ports(f, core, view, area),
        _ => {}
    }
}

fn render_bar(f: &mut Frame, core: &Core, tab: Option<&Tab>, r: Rect) {
    let right = tab.map(|t| format!("{} 8N1 ", t.baud)).unwrap_or_default();
    let rw = right.width() as u16;
    f.render_widget(Paragraph::new(right).alignment(Alignment::Right), r);
    let mut spans = vec![Span::styled(
        " comrade ",
        Style::new().add_modifier(Modifier::BOLD),
    )];
    for (i, t) in core.tabs().iter().enumerate() {
        let mut st = Style::new();
        if i == core.focus() {
            st = st.add_modifier(Modifier::BOLD | Modifier::REVERSED);
        } else if t.highlight {
            st = st.fg(Color::Yellow).add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(
            format!("[{} {} {}]", i + 1, t.name, glyph(&t.state)),
            st,
        ));
        spans.push(Span::raw(" "));
    }
    let lw = r.width.saturating_sub(rw + 1);
    f.render_widget(Paragraph::new(TLine::from(spans)), Rect { width: lw, ..r });
}

/// Spans for `text` with colors; plain fallback when parsing disagrees with
/// the SGR-stripped text (so byte ranges from `wrap_segments` stay valid).
fn styled_spans(text: &str, plain: &str) -> Vec<Span<'static>> {
    if text.contains('\x1b')
        && let Ok(t) = text.into_text()
    {
        let spans: Vec<Span<'static>> = t.lines.into_iter().flat_map(|l| l.spans).collect();
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        if joined == plain {
            return spans;
        }
    }
    vec![Span::raw(plain.to_string())]
}

/// Cut spans to the byte `range` of their concatenated content.
fn slice_spans(spans: Vec<Span<'static>>, range: Range<usize>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut off = 0;
    for s in spans {
        let len = s.content.len();
        let (a, b) = (range.start.max(off), range.end.min(off + len));
        if a < b {
            out.push(Span::styled(
                s.content[a - off..b - off].to_string(),
                s.style,
            ));
        }
        off += len;
    }
    out
}

/// Highlight ASCII-case-insensitive matches inside each span.
fn highlight(spans: Vec<Span<'static>>, needle: &str) -> Vec<Span<'static>> {
    if needle.is_empty() {
        return spans;
    }
    let n = needle.to_ascii_lowercase();
    let hl = Style::new().bg(Color::Yellow).fg(Color::Black);
    let mut out = Vec::new();
    for s in spans {
        let lower = s.content.to_ascii_lowercase();
        let mut at = 0;
        while let Some(p) = lower[at..].find(&n) {
            let (a, b) = (at + p, at + p + n.len());
            if a > at {
                out.push(Span::styled(s.content[at..a].to_string(), s.style));
            }
            out.push(Span::styled(s.content[a..b].to_string(), s.style.patch(hl)));
            at = b;
        }
        if at < s.content.len() {
            out.push(Span::styled(s.content[at..].to_string(), s.style));
        }
    }
    out
}

fn separator_line(text: &str, width: u16) -> TLine<'static> {
    let plain = strip_sgr(text);
    let w = plain.width();
    let total = width as usize;
    let s = if w >= total {
        plain.into_owned()
    } else {
        let left = (total - w) / 2;
        format!(
            "{}{}{}",
            "─".repeat(left),
            plain,
            "─".repeat(total - w - left)
        )
    };
    TLine::styled(s, dim())
}

fn data_line(r: &VisibleRow, body_w: u16, ts: bool, wrap: bool, search: &str) -> TLine<'static> {
    let plain = strip_sgr(&r.line.text);
    let segs = wrap_segments(&plain, body_w);
    let range = if wrap {
        segs.get(r.segment)
    } else {
        segs.first()
    }
    .cloned()
    .unwrap_or(0..0);
    let base = match r.line.kind {
        LineKind::Error => Style::new().fg(Color::Red),
        LineKind::Info => dim(),
        _ => Style::new(),
    };
    let spans = slice_spans(styled_spans(&r.line.text, &plain), range);
    let mut out = Vec::with_capacity(spans.len() + 1);
    if ts {
        out.push(if r.segment == 0 || !wrap {
            Span::styled(format!("{} ", r.line.at.format("%H:%M:%S%.3f")), dim())
        } else {
            Span::raw(" ".repeat(TS_WIDTH as usize))
        });
    }
    out.extend(highlight(spans, search).into_iter().map(|mut s| {
        s.style = base.patch(s.style);
        s
    }));
    TLine::from(out)
}

fn render_log(f: &mut Frame, tab: Option<&Tab>, view: &View, r: Rect) {
    let Some(tab) = tab else {
        f.render_widget(Paragraph::new(" waiting for a board…").style(dim()), r);
        return;
    };
    let ts = view.timestamps && r.width > TS_WIDTH;
    let ts_w = if ts { TS_WIDTH } else { 0 };
    let body_w = r.width.saturating_sub(ts_w).max(1);
    let rows = tab
        .scroll
        .visible(r.height, r.width, view.wrap, tab.asm.partial(), ts_w);
    let lines: Vec<TLine> = rows
        .iter()
        .map(|row| {
            if row.line.kind == LineKind::Separator {
                if row.segment == 0 {
                    separator_line(&row.line.text, r.width)
                } else {
                    TLine::default()
                }
            } else {
                data_line(row, body_w, ts, view.wrap, &view.search)
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), r);

    if !tab.scroll.follow() && r.height > 0 {
        let n = tab.scroll.new_since_freeze();
        let msg = if n > 0 {
            format!(" +{n} new ")
        } else {
            " scrolled · End follows ".to_string()
        };
        let line = Rect {
            y: r.y + r.height - 1,
            height: 1,
            ..r
        };
        f.render_widget(
            Paragraph::new(msg).alignment(Alignment::Right).style(
                Style::new()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ),
            Rect {
                x: line.x + line.width.saturating_sub(24),
                width: line.width.min(24),
                ..line
            },
        );
    }
}

fn render_send(f: &mut Frame, tab: Option<&Tab>, view: &View, r: Rect) {
    let le = match tab.map(|t| t.line_ending) {
        Some(LineEnding::CrLf) => "\\r\\n",
        Some(LineEnding::None) => "none",
        _ => "\\n",
    };
    let right = format!("[Enter sends {le}] ");
    let rw = right.width() as u16;
    f.render_widget(
        Paragraph::new(right)
            .alignment(Alignment::Right)
            .style(dim()),
        r,
    );
    let (prefix, text) = match view.mode {
        Mode::Search => (" /", &view.search),
        _ => (" >", &view.input),
    };
    let style = if matches!(view.mode, Mode::Send | Mode::Search) {
        Style::new()
    } else {
        dim()
    };
    let lw = r.width.saturating_sub(rw + 1);
    // Show the tail of the input so the cursor (at the end) stays visible.
    let avail = (lw as usize).saturating_sub(prefix.width() + 2);
    let mut tail_w = 0;
    let mut start = text.len();
    for (i, c) in text.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if tail_w + w > avail {
            break;
        }
        tail_w += w;
        start = i;
    }
    f.render_widget(
        Paragraph::new(format!("{prefix} {}", &text[start..])).style(style),
        Rect { width: lw, ..r },
    );
    if matches!(view.mode, Mode::Send | Mode::Search) && r.width > 0 && r.height > 0 {
        let x = (prefix.width() + 1 + tail_w) as u16;
        f.set_cursor_position((r.x + x.min(r.width - 1), r.y));
    }
}

fn render_status(f: &mut Frame, tab: Option<&Tab>, view: &View, r: Rect) {
    let hints = "↑↓ scroll  End follow  / search  b baud  p pause  ? help";
    let mut spans = Vec::new();
    let (g, node, rate) = match tab {
        Some(t) => (
            glyph(&t.state),
            t.devnode
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "—".into()),
            view.rate_kbps.get(&t.key).copied().unwrap_or(0.0),
        ),
        None => ("○", "—".into(), 0.0),
    };
    spans.push(Span::raw(format!(" {g} {node}  {rate:.1} kB/s   ")));
    let msg = view
        .status_msg
        .as_ref()
        .filter(|(_, at)| at.elapsed() < STATUS_TTL)
        .map(|(m, _)| m.clone());
    match tab.map(|t| &t.state) {
        Some(TabState::Paused(PauseReason::Upload(Holder::Named { pid, comm }))) => {
            spans.push(Span::styled(
                format!("⏸ upload in progress ({comm}, pid {pid})"),
                Style::new().fg(Color::Yellow),
            ));
        }
        Some(TabState::Paused(PauseReason::Upload(Holder::Unknown))) => {
            spans.push(Span::styled(
                "⏸ opened by another program",
                Style::new().fg(Color::Yellow),
            ));
        }
        Some(TabState::Paused(PauseReason::Manual)) => {
            spans.push(Span::styled("⏸ paused", Style::new().fg(Color::Yellow)));
        }
        Some(TabState::Error(e)) => {
            spans.push(Span::styled(e.clone(), Style::new().fg(Color::Red)));
        }
        _ => match msg {
            Some(m) => spans.push(Span::raw(m)),
            None => spans.push(Span::styled(hints, dim())),
        },
    }
    f.render_widget(Paragraph::new(TLine::from(spans)), r);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn overlay_help(f: &mut Frame, area: Rect) {
    const HELP: &[&str] = &[
        "1-9 Tab S-Tab  switch tab",
        "Up Down PgUp PgDn Home End, wheel  scroll / follow",
        "/ n N          search / next / previous",
        "i Enter        focus send box",
        "b              baud picker",
        "p              pause / resume",
        "r              reset board",
        "t              toggle timestamps",
        "w              toggle wrap",
        "c              other ports",
        "s              save scrollback",
        "C              clear scrollback",
        "x              close tab",
        "q Ctrl-C       quit",
        "",
        "Send box: Enter send, Ctrl-E line ending, Up/Down history, Esc back",
        "",
        "If ports are grabbed by ModemManager/brltty, disable them for these devices.",
    ];
    let r = centered(area, 80, HELP.len() as u16 + 2);
    f.render_widget(Clear, r);
    let items: Vec<TLine> = HELP.iter().map(|s| TLine::raw(format!(" {s}"))).collect();
    f.render_widget(
        Paragraph::new(items).block(
            Block::new()
                .borders(Borders::ALL)
                .title(" Help (any key closes) "),
        ),
        r,
    );
}

fn picker(f: &mut Frame, area: Rect, title: &str, items: Vec<ListItem<'static>>, sel: usize) {
    let r = centered(area, 56, items.len() as u16 + 2);
    f.render_widget(Clear, r);
    let list = List::new(items)
        .block(Block::new().borders(Borders::ALL).title(title.to_string()))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut st = ListState::default().with_selected(Some(sel));
    f.render_stateful_widget(list, r, &mut st);
}

fn overlay_baud(f: &mut Frame, view: &View, area: Rect) {
    let mut items: Vec<ListItem> = BAUDS.iter().map(|b| ListItem::new(b.to_string())).collect();
    items.push(ListItem::new(if view.picker_idx == BAUDS.len() {
        format!("custom…  {}_", view.custom_baud)
    } else {
        "custom…".to_string()
    }));
    picker(
        f,
        area,
        " Baud (Enter selects, Esc cancels) ",
        items,
        view.picker_idx,
    );
}

fn overlay_ports(f: &mut Frame, core: &Core, view: &View, area: Rect) {
    let mut items: Vec<ListItem> = core
        .other_ports()
        .iter()
        .map(|d| {
            ListItem::new(format!(
                "{}  {:04x}:{:04x}  {}",
                d.devnode.display(),
                d.vid,
                d.pid,
                d.product.as_deref().unwrap_or("")
            ))
        })
        .collect();
    if items.is_empty() {
        items.push(ListItem::new("no other ports"));
    }
    picker(
        f,
        area,
        " Other ports (Enter connects, Esc cancels) ",
        items,
        view.picker_idx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{BoardKey, DeviceInfo};
    use crate::core::{Action, Core, PauseReason, TabState};
    use crate::event::{Event, Holder, Now};
    use crate::lines::{Line, LineKind};
    use chrono::{Local, TimeZone};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use std::collections::HashMap;

    fn now() -> Now {
        Now {
            mono: std::time::Instant::now(),
            wall: Local::now(),
        }
    }

    fn dev(serial: &str, node: &str) -> DeviceInfo {
        DeviceInfo {
            devnode: node.into(),
            vid: 0x10c4,
            pid: 0xea60,
            serial: Some(serial.into()),
            id_path: None,
            product: Some("Prod".into()),
            gid: 0,
        }
    }

    fn core_with(n: usize) -> Core {
        let mut c = Core::new(HashMap::new(), 115_200, vec![]);
        for i in 0..n {
            c.handle(
                Event::DeviceAdded(dev(&format!("S{i}"), &format!("/dev/ttyUSB{i}"))),
                now(),
            );
        }
        c
    }

    fn line(text: &str, kind: LineKind) -> Line {
        Line {
            at: Local.with_ymd_and_hms(2026, 1, 2, 14, 2, 31).unwrap(),
            kind,
            text: text.into(),
        }
    }

    fn draw(core: &Core, view: &View, w: u16, h: u16) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, core, view)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    #[test]
    fn tab_bar_shows_glyphs() {
        let mut c = core_with(3);
        c.tabs_mut()[0].state = TabState::Connected;
        c.tabs_mut()[1].state = TabState::Paused(PauseReason::Manual);
        c.tabs_mut()[2].state = TabState::Waiting;
        let rows = draw(&c, &View::new(), 80, 10);
        assert!(rows[0].contains('●'), "{}", rows[0]);
        assert!(rows[0].contains('⏸'));
        assert!(rows[0].contains('○'));
        assert!(rows[0].contains("115200 8N1"));
    }

    #[test]
    fn frozen_view_shows_new_count() {
        let mut c = core_with(1);
        c.tabs_mut()[0].state = TabState::Connected;
        for i in 0..30 {
            c.tabs_mut()[0]
                .scroll
                .push(line(&format!("l{i}"), LineKind::Data));
        }
        c.tabs_mut()[0].scroll.scroll_up(5);
        for _ in 0..3 {
            c.tabs_mut()[0].scroll.push(line("new", LineKind::Data));
        }
        let rows = draw(&c, &View::new(), 80, 12);
        assert!(rows.iter().any(|r| r.contains("+3 new")), "{rows:#?}");
    }

    #[test]
    fn paused_status_names_holder() {
        let mut c = core_with(1);
        c.tabs_mut()[0].state = TabState::Paused(PauseReason::Upload(Holder::Named {
            pid: 4121,
            comm: "esptool".into(),
        }));
        let rows = draw(&c, &View::new(), 100, 10);
        assert!(
            rows.iter()
                .any(|r| r.contains("upload in progress (esptool, pid 4121)")),
            "{rows:#?}"
        );
        c.tabs_mut()[0].state = TabState::Paused(PauseReason::Upload(Holder::Unknown));
        let rows = draw(&c, &View::new(), 100, 10);
        assert!(rows.iter().any(|r| r.contains("opened by another program")));
    }

    #[test]
    fn status_shows_rate_and_devnode() {
        let mut c = core_with(1);
        c.tabs_mut()[0].state = TabState::Connected;
        let mut v = View::new();
        v.rate_kbps
            .insert(BoardKey(c.tabs()[0].key.0.clone()), 1.24);
        let rows = draw(&c, &v, 100, 10);
        assert!(
            rows.iter()
                .any(|r| r.contains("/dev/ttyUSB0") && r.contains("1.2 kB/s"))
        );
    }

    #[test]
    fn render_tiny_terminal_no_panic() {
        let mut c = core_with(2);
        c.tabs_mut()[0]
            .scroll
            .push(line("日本語 hello", LineKind::Data));
        let mut v = View::new();
        for (w, h) in [(10, 3), (1, 1), (80, 0), (0, 0), (0, 10), (3, 40)] {
            for mode in [
                Mode::Log,
                Mode::Send,
                Mode::Search,
                Mode::BaudPicker,
                Mode::OtherPorts,
                Mode::Help,
            ] {
                v.mode = mode;
                draw(&c, &v, w, h);
            }
        }
        draw(
            &Core::new(HashMap::new(), 9600, vec![]),
            &View::new(),
            20,
            8,
        );
    }

    #[test]
    fn keys_map() {
        let mut c = core_with(2);
        let mut v = View::new();
        assert_eq!(
            on_key(&mut v, &mut c, ch('2')),
            UiOutcome::Action(Action::Focus(1))
        );
        assert_eq!(
            on_key(&mut v, &mut c, ch('p')),
            UiOutcome::Action(Action::TogglePause)
        );
        assert_eq!(
            on_key(&mut v, &mut c, ch('r')),
            UiOutcome::Action(Action::Reset)
        );
        assert_eq!(
            on_key(&mut v, &mut c, ch('C')),
            UiOutcome::Action(Action::ClearTab)
        );
        assert_eq!(
            on_key(&mut v, &mut c, ch('x')),
            UiOutcome::Action(Action::CloseTab)
        );
        assert_eq!(on_key(&mut v, &mut c, ch('s')), UiOutcome::Save);
        assert_eq!(
            on_key(&mut v, &mut c, key(KeyCode::Tab)),
            UiOutcome::Action(Action::NextTab)
        );
        assert_eq!(
            on_key(&mut v, &mut c, key(KeyCode::BackTab)),
            UiOutcome::Action(Action::PrevTab)
        );
        assert_eq!(on_key(&mut v, &mut c, ch('q')), UiOutcome::Quit);
        on_key(&mut v, &mut c, ch('i'));
        assert_eq!(v.mode, Mode::Send);
        on_key(&mut v, &mut c, ch('h'));
        on_key(&mut v, &mut c, ch('i'));
        assert_eq!(
            on_key(&mut v, &mut c, key(KeyCode::Enter)),
            UiOutcome::Action(Action::Send("hi".into()))
        );
        assert_eq!(v.history, vec!["hi".to_string()]);
        assert_eq!(
            on_key(&mut v, &mut c, key(KeyCode::Enter)),
            UiOutcome::Action(Action::Send(String::new()))
        );
        assert_eq!(
            on_key(
                &mut v,
                &mut c,
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)
            ),
            UiOutcome::Action(Action::CycleLineEnding)
        );
        on_key(&mut v, &mut c, key(KeyCode::Up));
        assert_eq!(v.input, "hi");
        on_key(&mut v, &mut c, key(KeyCode::Esc));
        assert_eq!(v.mode, Mode::Log);
        assert_eq!(
            on_key(
                &mut v,
                &mut c,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            UiOutcome::Quit
        );
    }

    #[test]
    fn baud_picker_flow() {
        let mut c = core_with(1);
        let mut v = View::new();
        on_key(&mut v, &mut c, ch('b'));
        assert_eq!(v.mode, Mode::BaudPicker);
        // 115200 preselected
        assert_eq!(
            on_key(&mut v, &mut c, key(KeyCode::Enter)),
            UiOutcome::Action(Action::SetBaud(115_200))
        );
        assert_eq!(v.mode, Mode::Log);
        on_key(&mut v, &mut c, ch('b'));
        for d in "250000".chars() {
            on_key(&mut v, &mut c, ch(d));
        }
        assert_eq!(
            on_key(&mut v, &mut c, key(KeyCode::Enter)),
            UiOutcome::Action(Action::SetBaud(250_000))
        );
    }

    #[test]
    fn search_jumps_and_steps() {
        let mut c = core_with(1);
        for t in ["a", "needle 1", "b", "needle 2", "c"] {
            c.tabs_mut()[0].scroll.push(line(t, LineKind::Data));
        }
        c.tabs_mut()[0].scroll.jump_to(0);
        let mut v = View::new();
        on_key(&mut v, &mut c, ch('/'));
        for d in "needle".chars() {
            on_key(&mut v, &mut c, ch(d));
        }
        assert_eq!(
            c.tabs()[0].scroll.position(),
            3,
            "nothing older: wraps to newest"
        );
        on_key(&mut v, &mut c, key(KeyCode::Enter));
        assert_eq!(v.mode, Mode::Log);
        on_key(&mut v, &mut c, ch('n'));
        assert_eq!(c.tabs()[0].scroll.position(), 1);
        on_key(&mut v, &mut c, ch('N'));
        assert_eq!(c.tabs()[0].scroll.position(), 3);
    }

    fn follow_core(n: usize, hits: &[usize]) -> Core {
        let mut c = core_with(1);
        for i in 0..n {
            let t = if hits.contains(&i) {
                format!("needle {i}")
            } else {
                format!("l{i}")
            };
            c.tabs_mut()[0].scroll.push(line(&t, LineKind::Data));
        }
        c
    }
    fn type_str(v: &mut View, c: &mut Core, s: &str) {
        for d in s.chars() {
            on_key(v, c, ch(d));
        }
    }

    #[test]
    fn incremental_search_in_follow_state() {
        let mut c = follow_core(100, &[10, 50]);
        let mut v = View::new();
        on_key(&mut v, &mut c, ch('/'));
        type_str(&mut v, &mut c, "needle 1");
        assert_eq!(c.tabs()[0].scroll.position(), 10);
        // backspace to a shorter needle: newest match from origin
        on_key(&mut v, &mut c, key(KeyCode::Backspace));
        assert_eq!(c.tabs()[0].scroll.position(), 50);
        // empty needle restores follow
        for _ in 0..7 {
            on_key(&mut v, &mut c, key(KeyCode::Backspace));
        }
        assert!(c.tabs()[0].scroll.follow());
    }

    #[test]
    fn search_n_wraps_both_ways() {
        let mut c = follow_core(100, &[10, 50]);
        let mut v = View::new();
        on_key(&mut v, &mut c, ch('/'));
        type_str(&mut v, &mut c, "needle");
        on_key(&mut v, &mut c, key(KeyCode::Enter));
        assert_eq!(c.tabs()[0].scroll.position(), 50);
        on_key(&mut v, &mut c, ch('n'));
        assert_eq!(c.tabs()[0].scroll.position(), 10);
        on_key(&mut v, &mut c, ch('n'));
        assert_eq!(c.tabs()[0].scroll.position(), 50, "wrap to newest");
        on_key(&mut v, &mut c, ch('N'));
        assert_eq!(c.tabs()[0].scroll.position(), 10, "wrap to oldest");
        on_key(&mut v, &mut c, ch('N'));
        assert_eq!(c.tabs()[0].scroll.position(), 50);
    }

    #[test]
    fn esc_empty_search_resumes_follow() {
        let mut c = follow_core(20, &[]);
        let mut v = View::new();
        on_key(&mut v, &mut c, ch('/'));
        on_key(&mut v, &mut c, key(KeyCode::Esc));
        assert!(c.tabs()[0].scroll.follow());
        assert_eq!(v.mode, Mode::Log);
    }

    #[test]
    fn send_box_scrolls_horizontally() {
        let c = core_with(1);
        let mut v = View::new();
        v.mode = Mode::Send;
        v.input = "0123456789abcdefghijklmnopqrstuvwxyz".into();
        let mut t = Terminal::new(TestBackend::new(40, 10)).unwrap();
        t.draw(|f| render(f, &c, &v)).unwrap();
        let buf = t.backend().buffer().clone();
        let y = (0..10)
            .find(|&y| {
                (0..40)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains("Enter sends")
            })
            .unwrap();
        let row: String = (0..40).map(|x| buf[(x, y)].symbol()).collect();
        assert!(row.contains("xyz"), "{row}");
        assert!(!row.contains("0123"), "{row}");
        let pos = t.get_cursor_position().unwrap();
        assert_eq!(pos.y, y);
        let idx = row.find("xyz").unwrap() + 3; // ascii only
        assert_eq!(pos.x as usize, idx);
    }

    #[test]
    fn overlay_ignores_wheel_and_baud_backspace() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let mut c = follow_core(30, &[]);
        let mut v = View::new();
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        v.mode = Mode::Help;
        assert_eq!(on_mouse(&mut v, &mut c, wheel), UiOutcome::None);
        assert!(c.tabs()[0].scroll.follow());
        v.mode = Mode::Log;
        assert_eq!(on_mouse(&mut v, &mut c, wheel), UiOutcome::Redraw);
        v.mode = Mode::BaudPicker;
        v.picker_idx = 0;
        v.custom_baud = "123".into();
        on_key(&mut v, &mut c, key(KeyCode::Backspace));
        assert_eq!(v.custom_baud, "123");
    }

    #[test]
    fn help_mentions_modemmanager_and_brltty() {
        let c = core_with(1);
        let mut v = View::new();
        v.mode = Mode::Help;
        let rows = draw(&c, &v, 100, 30);
        let want = "If ports are grabbed by ModemManager/brltty, disable them for these devices.";
        assert!(rows.iter().any(|r| r.contains(want)), "{rows:#?}");
    }

    #[test]
    fn other_ports_enter_connects() {
        let mut c = Core::new(HashMap::new(), 115_200, vec![]);
        let mut d = dev("P", "/dev/ttyACM9");
        d.vid = 0x04e8;
        d.pid = 0x6860;
        c.handle(Event::DeviceAdded(d), now());
        let mut v = View::new();
        on_key(&mut v, &mut c, ch('c'));
        assert_eq!(v.mode, Mode::OtherPorts);
        let rows = draw(&c, &v, 80, 20);
        assert!(
            rows.iter()
                .any(|r| r.contains("/dev/ttyACM9") && r.contains("04e8:6860"))
        );
        assert_eq!(
            on_key(&mut v, &mut c, key(KeyCode::Enter)),
            UiOutcome::Action(Action::ConnectOther("/dev/ttyACM9".into()))
        );
    }

    #[test]
    fn timestamps_toggle_changes_render() {
        let mut c = core_with(1);
        c.tabs_mut()[0].scroll.push(line("hello", LineKind::Data));
        let mut v = View::new();
        let with = draw(&c, &v, 60, 10);
        assert!(
            with.iter().any(|r| r.contains("14:02:31.000 hello")),
            "{with:#?}"
        );
        on_key(&mut v, &mut c, ch('t'));
        let without = draw(&c, &v, 60, 10);
        assert!(!without.iter().any(|r| r.contains("14:02:31")));
        assert!(without.iter().any(|r| r.contains("hello")));
    }

    #[test]
    fn wrapped_continuation_keeps_text() {
        let mut c = core_with(1);
        c.tabs_mut()[0]
            .scroll
            .push(line("\x1b[31mabcdefghij\x1b[0m", LineKind::Data));
        let mut v = View::new();
        v.timestamps = false;
        let rows = draw(&c, &v, 5, 12);
        assert!(rows.iter().any(|r| r.starts_with("abcde")));
        assert!(rows.iter().any(|r| r.starts_with("fghij")));
    }

    #[test]
    fn wrapped_continuation_keeps_color() {
        let mut c = core_with(1);
        c.tabs_mut()[0]
            .scroll
            .push(line("\x1b[31mabcdefghij\x1b[0m", LineKind::Data));
        let mut v = View::new();
        v.timestamps = false;
        let mut t = Terminal::new(TestBackend::new(5, 12)).unwrap();
        t.draw(|f| render(f, &c, &v)).unwrap();
        let buf = t.backend().buffer();
        let red = (0..12).filter(|&y| buf[(0, y)].symbol() == "f").count();
        assert_eq!(red, 1);
        let y = (0..12).find(|&y| buf[(0, y)].symbol() == "f").unwrap();
        assert_eq!(buf[(0, y)].fg, ratatui::style::Color::Red);
    }

    #[test]
    fn paste_only_in_send_mode() {
        let mut v = View::new();
        assert_eq!(on_paste(&mut v, "x"), UiOutcome::None);
        v.mode = Mode::Send;
        on_paste(&mut v, "ab\r\ncd");
        assert_eq!(v.input, "abcd");
    }
}
