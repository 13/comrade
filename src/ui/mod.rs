//! ratatui rendering and key handling on top of the pure `Core`.

mod keys;
pub mod theme;
pub mod view;

pub use keys::{UiOutcome, on_key, on_mouse, on_paste};
pub use view::{Mode, View};

use std::ops::Range;
use std::time::Duration;

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::scrollbar;
use ratatui::text::{Line as TLine, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState,
};
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
/// Log areas at least this wide keep their last column for the scrollbar.
const SCROLLBAR_MIN_W: u16 = 20;
const BRAND: &str = " ◆ comrade ";

fn glyph(s: &TabState) -> &'static str {
    match s {
        TabState::Connected => "●",
        TabState::Paused(_) => "⏸",
        TabState::Waiting | TabState::Connecting => "○",
        TabState::Error(_) => "!",
    }
}

fn line_ending(tab: Option<&Tab>) -> &'static str {
    match tab.map(|t| t.line_ending) {
        Some(LineEnding::CrLf) => "\\r\\n",
        Some(LineEnding::None) => "none",
        _ => "\\n",
    }
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

pub fn render(f: &mut Frame, core: &Core, view: &View) {
    let area = f.area();
    view.tab_hits.borrow_mut().clear();
    if area.width == 0 || area.height == 0 {
        view.last_log_rows.set(0);
        return;
    }
    let [bar, rule, log, rule2, send, status] = Layout::vertical([
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

    render_bar(f, core, tab, view, bar, rule);
    render_log(f, tab, view, log);
    f.render_widget(
        Paragraph::new("─".repeat(rule2.width as usize)).style(theme::muted()),
        rule2,
    );
    render_send(f, tab, view, send);
    render_status(f, tab, view, status);

    if matches!(view.mode, Mode::Help | Mode::BaudPicker | Mode::OtherPorts) {
        dim_backdrop(f, area);
    }
    match view.mode {
        Mode::Help => overlay_help(f, area),
        Mode::BaudPicker => overlay_baud(f, tab, view, area),
        Mode::OtherPorts => overlay_ports(f, core, view, area),
        _ => {}
    }
}

/// Tab labels and their widths, ` 1 name ● `.
fn tab_label(i: usize, t: &Tab, focused: bool) -> Vec<Span<'static>> {
    let name_style = if focused {
        Style::new().add_modifier(Modifier::BOLD)
    } else if t.highlight {
        theme::warn().add_modifier(Modifier::BOLD)
    } else {
        theme::muted()
    };
    vec![
        Span::styled(format!(" {} ", i + 1), theme::muted()),
        Span::styled(t.name.clone(), name_style),
        Span::raw(" "),
        Span::styled(glyph(&t.state), theme::state(&t.state)),
        Span::raw(" "),
    ]
}

/// Contiguous run of tabs around `focus` that fits in `avail` columns, with
/// room for the `‹ ` / ` ›` overflow markers when not everything fits.
fn visible_tabs(widths: &[usize], focus: usize, avail: usize) -> Range<usize> {
    if widths.iter().sum::<usize>() <= avail || widths.is_empty() {
        return 0..widths.len();
    }
    let focus = focus.min(widths.len() - 1);
    let avail = avail.saturating_sub(4);
    let (mut a, mut b, mut used) = (focus, focus + 1, widths[focus]);
    loop {
        if b < widths.len() && used + widths[b] <= avail {
            used += widths[b];
            b += 1;
        } else if a > 0 && used + widths[a - 1] <= avail {
            a -= 1;
            used += widths[a];
        } else {
            return a..b;
        }
    }
}

fn render_bar(f: &mut Frame, core: &Core, tab: Option<&Tab>, view: &View, r: Rect, rule: Rect) {
    let right: Vec<Span> = match tab {
        Some(t) => vec![
            Span::styled(
                t.baud.to_string(),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled(" 8N1 ", theme::muted()),
        ],
        None => vec![],
    };
    let rw = spans_width(&right) as u16;
    f.render_widget(
        Paragraph::new(TLine::from(right)).alignment(Alignment::Right),
        r,
    );

    let mut spans = vec![Span::styled(BRAND, theme::brand())];
    let mut x = r.x + BRAND.width() as u16;
    let labels: Vec<Vec<Span>> = core
        .tabs()
        .iter()
        .enumerate()
        .map(|(i, t)| tab_label(i, t, i == core.focus()))
        .collect();
    let widths: Vec<usize> = labels.iter().map(|l| spans_width(l)).collect();
    let avail = (r.width.saturating_sub(rw + 1) as usize).saturating_sub(BRAND.width());
    let shown = visible_tabs(&widths, core.focus(), avail);
    let mut underline = None;
    if shown.start > 0 {
        spans.push(Span::styled("‹ ", theme::muted()));
        x += 2;
    }
    let mut hits = view.tab_hits.borrow_mut();
    for (i, label) in labels.into_iter().enumerate() {
        if !shown.contains(&i) {
            continue;
        }
        let w = widths[i] as u16;
        if i == core.focus() {
            underline = Some((x, w));
        }
        hits.push((x..x + w, i));
        spans.extend(label);
        x += w;
    }
    if shown.end < widths.len() {
        spans.push(Span::styled(" ›", theme::muted()));
    }
    let lw = r.width.saturating_sub(rw + 1);
    f.render_widget(Paragraph::new(TLine::from(spans)), Rect { width: lw, ..r });

    // Rule under the bar, heavier and in the accent color under the focused tab.
    f.render_widget(
        Paragraph::new("─".repeat(rule.width as usize)).style(theme::muted()),
        rule,
    );
    if let Some((ux, uw)) = underline {
        let ux = ux.min(rule.right());
        let uw = uw.min(rule.right() - ux);
        f.render_widget(
            Paragraph::new("━".repeat(uw as usize)).style(theme::accent()),
            Rect {
                x: ux,
                width: uw,
                ..rule
            },
        );
    }
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
    let hl = theme::search_hit();
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

/// `──── label · HH:MM:SS ────`, re-centered to `width` with the label
/// colored by kind (reconnected, reset, upload).
fn separator_line(text: &str, width: u16) -> TLine<'static> {
    let plain = strip_sgr(text);
    let core = plain.trim_matches(|c| c == '─' || c == ' ');
    let (label, rest) = match core.split_once(" · ") {
        Some((l, r)) => (l.to_string(), format!(" · {r}")),
        None => (core.to_string(), String::new()),
    };
    let w = label.width() + rest.width() + 2;
    let total = width as usize;
    if w >= total {
        return TLine::styled(plain.into_owned(), theme::muted());
    }
    let left = (total - w) / 2;
    TLine::from(vec![
        Span::styled(format!("{} ", "─".repeat(left)), theme::muted()),
        Span::styled(label.clone(), theme::separator_label(&label)),
        Span::styled(
            format!("{rest} {}", "─".repeat(total - w - left)),
            theme::muted(),
        ),
    ])
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
        LineKind::Error => theme::err(),
        LineKind::Info => theme::muted().add_modifier(Modifier::ITALIC),
        _ => Style::new(),
    };
    let spans = slice_spans(styled_spans(&r.line.text, &plain), range);
    let mut out = Vec::with_capacity(spans.len() + 1);
    if ts {
        out.push(if r.segment == 0 || !wrap {
            Span::styled(
                format!("{} ", r.line.at.format("%H:%M:%S%.3f")),
                theme::muted(),
            )
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

fn render_empty(f: &mut Frame, r: Rect) {
    let lines = vec![
        TLine::styled("◆ comrade", theme::brand()),
        TLine::default(),
        TLine::raw("waiting for a board…"),
        TLine::styled(
            "Plug in a USB board. It connects on its own.",
            theme::muted(),
        ),
        TLine::from(vec![
            Span::styled("c", theme::key()),
            Span::styled(" other ports   ", theme::muted()),
            Span::styled("?", theme::key()),
            Span::styled(" help   ", theme::muted()),
            Span::styled("q", theme::key()),
            Span::styled(" quit", theme::muted()),
        ]),
    ];
    let h = (lines.len() as u16).min(r.height);
    let y = r.y + (r.height - h) / 2;
    f.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        Rect { y, height: h, ..r },
    );
}

fn render_log(f: &mut Frame, tab: Option<&Tab>, view: &View, r: Rect) {
    let Some(tab) = tab else {
        render_empty(f, r);
        return;
    };
    // The last column is kept free for the scrollbar so wrapping does not
    // shift when it appears.
    let bar_w = u16::from(r.width >= SCROLLBAR_MIN_W);
    let body = Rect {
        width: r.width - bar_w,
        ..r
    };
    let ts = view.timestamps && body.width > TS_WIDTH;
    let ts_w = if ts { TS_WIDTH } else { 0 };
    let body_w = body.width.saturating_sub(ts_w).max(1);
    let filtering = view.filtering();
    let rows = if filtering {
        tab.scroll.visible_filtered(
            &view.search,
            view.filter_skip,
            body.height,
            body.width,
            view.wrap,
            ts_w,
        )
    } else {
        tab.scroll
            .visible(body.height, body.width, view.wrap, tab.asm.partial(), ts_w)
    };
    let lines: Vec<TLine> = rows
        .iter()
        .map(|row| {
            if row.line.kind == LineKind::Separator {
                if row.segment == 0 {
                    separator_line(&row.line.text, body.width)
                } else {
                    TLine::default()
                }
            } else {
                data_line(row, body_w, ts, view.wrap, &view.search)
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), body);

    if filtering && r.height > 0 {
        let msg = if view.filter_skip > 0 {
            format!(" filter \"{}\" · older · End newest ", view.search)
        } else {
            format!(" filter \"{}\" · f off ", view.search)
        };
        let w = (msg.width() as u16).min(body.width);
        f.render_widget(
            Paragraph::new(msg).style(theme::pill()),
            Rect {
                x: body.right() - w,
                y: r.bottom() - 1,
                width: w,
                height: 1,
            },
        );
        return;
    }
    if tab.scroll.follow() || r.height == 0 {
        return;
    }
    if bar_w == 1 && tab.scroll.len() > 1 {
        let mut st = ScrollbarState::new(tab.scroll.len())
            .position(tab.scroll.position())
            .viewport_content_length(r.height as usize);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .symbols(scrollbar::VERTICAL)
                .begin_symbol(None)
                .end_symbol(None)
                .track_style(theme::muted())
                .thumb_style(theme::accent()),
            r,
            &mut st,
        );
    }
    let n = tab.scroll.new_since_freeze();
    let msg = if n > 0 {
        format!(" ↓ +{n} new · End ")
    } else {
        " scrolled · End follows ".to_string()
    };
    let w = (msg.width() as u16).min(body.width);
    f.render_widget(
        Paragraph::new(msg).style(theme::pill()),
        Rect {
            x: body.right() - w,
            y: r.bottom() - 1,
            width: w,
            height: 1,
        },
    );
}

fn render_send(f: &mut Frame, tab: Option<&Tab>, view: &View, r: Rect) {
    if view.mode == Mode::Raw {
        f.render_widget(
            Paragraph::new(TLine::from(vec![
                Span::styled(" ⌨", theme::warn().add_modifier(Modifier::BOLD)),
                Span::raw(" "),
                Span::styled(
                    "raw mode: every key goes to the board, Ctrl-] leaves",
                    theme::warn(),
                ),
            ])),
            r,
        );
        return;
    }
    let active = matches!(view.mode, Mode::Send | Mode::Search);
    let right: Vec<Span> = match view.mode {
        Mode::Search => vec![
            Span::styled("Enter", theme::key()),
            Span::styled(" keep  ", theme::muted()),
            Span::styled("Esc", theme::key()),
            Span::styled(" cancel ", theme::muted()),
        ],
        _ => vec![
            Span::styled("Enter sends ", theme::muted()),
            Span::styled(line_ending(tab), theme::accent()),
            Span::raw(" "),
        ],
    };
    let rw = spans_width(&right) as u16;
    f.render_widget(
        Paragraph::new(TLine::from(right)).alignment(Alignment::Right),
        r,
    );
    let (prefix, text, pstyle) = match view.mode {
        Mode::Search => (" /", &view.search, theme::warn()),
        Mode::Send => (" ❯", &view.input, theme::accent()),
        _ => (" ❯", &view.input, theme::muted()),
    };
    let pstyle = pstyle.add_modifier(Modifier::BOLD);
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
    let body = if !active && text.is_empty() {
        Span::styled(
            "press i to type, / to search",
            theme::muted().add_modifier(Modifier::ITALIC),
        )
    } else if active {
        Span::raw(text[start..].to_string())
    } else {
        Span::styled(text[start..].to_string(), theme::muted())
    };
    f.render_widget(
        Paragraph::new(TLine::from(vec![
            Span::styled(prefix, pstyle),
            Span::raw(" "),
            body,
        ])),
        Rect { width: lw, ..r },
    );
    if active && r.width > 0 && r.height > 0 {
        let x = (prefix.width() + 1 + tail_w) as u16;
        f.set_cursor_position((r.x + x.min(r.width - 1), r.y));
    }
}

fn mode_badge(mode: Mode) -> (&'static str, Color) {
    match mode {
        Mode::Log => (" LOG ", Color::Blue),
        Mode::Send => (" SEND ", Color::Green),
        Mode::Raw => (" RAW ", Color::Red),
        Mode::Search => (" SEARCH ", Color::Yellow),
        Mode::BaudPicker => (" BAUD ", Color::Magenta),
        Mode::OtherPorts => (" PORTS ", Color::Magenta),
        Mode::Help => (" HELP ", Color::Magenta),
    }
}

fn state_text(s: &TabState) -> String {
    match s {
        TabState::Connected => "● connected".into(),
        TabState::Connecting => "○ connecting".into(),
        TabState::Waiting => "○ waiting".into(),
        TabState::Paused(PauseReason::Upload(Holder::Named { pid, comm })) => {
            format!("⏸ upload in progress ({comm}, pid {pid})")
        }
        TabState::Paused(PauseReason::Upload(Holder::Unknown)) => {
            "⏸ opened by another program".into()
        }
        TabState::Paused(PauseReason::Manual) => "⏸ paused".into(),
        TabState::Error(e) => format!("! {e}"),
    }
}

/// Key hints for the status line, most important first.
fn hints(view: &View, tab: Option<&Tab>) -> Vec<(&'static str, &'static str)> {
    match view.mode {
        Mode::Send => vec![
            ("Enter", "send"),
            ("^E", "line ending"),
            ("↑↓", "history"),
            ("Esc", "back"),
        ],
        Mode::Raw => vec![("^]", "leave raw mode")],
        Mode::Search => vec![("Enter", "keep"), ("Esc", "cancel")],
        Mode::BaudPicker | Mode::OtherPorts => {
            vec![("↑↓", "select"), ("Enter", "apply"), ("Esc", "cancel")]
        }
        Mode::Help => vec![("any key", "close")],
        Mode::Log if tab.is_none() => vec![("c", "other ports"), ("?", "help"), ("q", "quit")],
        Mode::Log => {
            let mut h = Vec::new();
            if tab.is_some_and(|t| !t.scroll.follow()) {
                h.push(("End", "follow"));
            }
            if !view.search.is_empty() {
                h.push(("n/N", "next/prev"));
                h.push(("f", if view.filter { "unfilter" } else { "filter" }));
            }
            h.extend([
                ("?", "help"),
                ("/", "search"),
                ("i", "send"),
                ("I", "raw"),
                ("b", "baud"),
                ("p", "pause"),
                ("r", "reset"),
                ("q", "quit"),
            ]);
            h
        }
    }
}

fn render_status(f: &mut Frame, tab: Option<&Tab>, view: &View, r: Rect) {
    let (label, color) = mode_badge(view.mode);
    let mut left = vec![
        Span::styled(label, theme::mode_badge(color)),
        Span::raw(" "),
    ];
    match tab {
        Some(t) => {
            left.push(Span::styled(state_text(&t.state), theme::state(&t.state)));
            let node = t
                .devnode
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "—".into());
            let rate = view.rate_kbps.get(&t.key).copied().unwrap_or(0.0);
            left.push(Span::styled(
                format!("  {node}  {rate:.1} kB/s"),
                theme::muted(),
            ));
        }
        None => left.push(Span::styled("○ no board", theme::muted())),
    }
    let lw = spans_width(&left);
    f.render_widget(Paragraph::new(TLine::from(left)), r);

    let avail = (r.width as usize).saturating_sub(lw + 3);
    let msg = view
        .status_msg
        .as_ref()
        .filter(|(_, at)| at.elapsed() < STATUS_TTL)
        .map(|(m, _)| m.clone());
    let right: Vec<Span> = match msg {
        Some(m) => vec![Span::styled(
            format!("{m} "),
            theme::accent().add_modifier(Modifier::BOLD),
        )],
        None => {
            let mut out: Vec<Span> = Vec::new();
            let mut used = 0;
            for (k, d) in hints(view, tab) {
                let w = k.width() + d.width() + 3;
                if used + w > avail {
                    break;
                }
                used += w;
                out.push(Span::styled(k, theme::key()));
                out.push(Span::styled(format!(" {d}  "), theme::muted()));
            }
            out
        }
    };
    let rw = (spans_width(&right) as u16).min(r.width);
    f.render_widget(
        Paragraph::new(TLine::from(right)),
        Rect {
            x: r.right() - rw,
            width: rw,
            ..r
        },
    );
}

/// Fade everything behind a modal so the popup reads as the focus.
fn dim_backdrop(f: &mut Frame, area: Rect) {
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            cell.modifier.insert(Modifier::DIM);
        }
    }
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

/// Rounded, accent-bordered popup frame with hints in the bottom border.
fn popup<'a>(title: &'a str, footer: &[(&'a str, &'a str)]) -> Block<'a> {
    let mut foot = vec![Span::raw(" ")];
    for (k, d) in footer {
        foot.push(Span::styled(*k, theme::key()));
        foot.push(Span::styled(format!(" {d} "), theme::muted()));
    }
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(TLine::from(vec![
            Span::raw(" "),
            Span::styled(title, Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ]))
        .title_bottom(TLine::from(foot).alignment(Alignment::Right))
        .padding(Padding::horizontal(1))
}

const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Tabs",
        &[
            ("1-9 Tab S-Tab", "switch (or click tab)"),
            ("x", "close tab"),
            ("c", "other ports"),
        ],
    ),
    (
        "Scrollback",
        &[
            ("↑↓ PgUp PgDn", "scroll (or wheel)"),
            ("Home End", "oldest / follow"),
            ("/ n N", "search / next / prev"),
            ("t w", "timestamps / wrap"),
            ("s C", "save / clear log"),
        ],
    ),
    (
        "Board",
        &[
            ("y", "copy the bottom line to the clipboard"),
            ("m", "mouse on/off (off: select text with the terminal)"),
            ("f", "filter: show only lines that match the search"),
            ("i Enter", "focus send box"),
            ("I", "raw mode: keys go to the board, Ctrl-] leaves"),
            ("b", "baud picker"),
            ("p", "pause / resume"),
            ("r", "reset board"),
            ("q Ctrl-C", "quit"),
        ],
    ),
    (
        "Send box",
        &[
            ("Enter", "send"),
            ("Ctrl-E", "cycle line ending"),
            ("↑↓", "history"),
            ("Esc", "back to log"),
        ],
    ),
];

fn help_section(section: &str, keys: &[(&str, &str)]) -> Vec<TLine<'static>> {
    const KEY_W: usize = 15;
    let mut lines = vec![TLine::styled(
        section.to_string(),
        theme::accent().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )];
    for (k, d) in keys {
        let pad = KEY_W.saturating_sub(k.width());
        lines.push(TLine::from(vec![
            Span::styled(format!("  {k}{}", " ".repeat(pad)), theme::key()),
            Span::raw(d.to_string()),
        ]));
    }
    lines
}

/// Two columns when the terminal is wide enough, one otherwise.
fn overlay_help(f: &mut Frame, area: Rect) {
    const NOTE: &str =
        "If ports are grabbed by ModemManager/brltty, disable them for these devices.";
    let two = area.width >= 84;
    let mut cols: Vec<Vec<TLine>> = vec![Vec::new(); if two { 2 } else { 1 }];
    let n = cols.len();
    for (i, (section, keys)) in HELP.iter().enumerate() {
        let col = &mut cols[i * n / HELP.len()];
        if !col.is_empty() {
            col.push(TLine::default());
        }
        col.extend(help_section(section, keys));
    }
    let body_h = cols.iter().map(Vec::len).max().unwrap_or(0) as u16;
    let r = centered(area, if two { 84 } else { 82 }, body_h + 4);
    f.render_widget(Clear, r);
    let block = popup("Keys", &[("any key", "close")]);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [body, _, note] = Layout::vertical([
        Constraint::Length(body_h),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let parts = Layout::horizontal(vec![Constraint::Ratio(1, n as u32); n]).split(body);
    for (col, rect) in cols.into_iter().zip(parts.iter()) {
        f.render_widget(Paragraph::new(col), *rect);
    }
    f.render_widget(
        Paragraph::new(NOTE).style(theme::muted().add_modifier(Modifier::ITALIC)),
        note,
    );
}

fn picker(
    f: &mut Frame,
    area: Rect,
    title: &str,
    footer: &[(&str, &str)],
    items: Vec<ListItem<'static>>,
    sel: usize,
    w: u16,
) {
    let r = centered(area, w, items.len() as u16 + 2);
    f.render_widget(Clear, r);
    let list = List::new(items)
        .block(popup(title, footer))
        .highlight_style(theme::selected())
        .highlight_symbol("› ");
    let mut st = ListState::default().with_selected(Some(sel));
    f.render_stateful_widget(list, r, &mut st);
}

fn overlay_baud(f: &mut Frame, tab: Option<&Tab>, view: &View, area: Rect) {
    let cur = tab.map(|t| t.baud);
    let mut items: Vec<ListItem> = BAUDS
        .iter()
        .map(|&b| {
            let mark = if Some(b) == cur {
                Span::styled("  ● current", theme::ok())
            } else {
                Span::raw("")
            };
            ListItem::new(TLine::from(vec![Span::raw(format!("{b:>9}")), mark]))
        })
        .collect();
    items.push(ListItem::new(if view.picker_idx == BAUDS.len() {
        TLine::from(vec![
            Span::raw("  custom… "),
            Span::styled(
                format!("{}▏", view.custom_baud),
                Style::new().add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        TLine::raw("  custom…")
    }));
    picker(
        f,
        area,
        "Baud rate",
        &[("Enter", "apply"), ("Esc", "cancel")],
        items,
        view.picker_idx,
        36,
    );
}

fn overlay_ports(f: &mut Frame, core: &Core, view: &View, area: Rect) {
    let mut items: Vec<ListItem> = core
        .other_ports()
        .iter()
        .map(|d| {
            ListItem::new(TLine::from(vec![
                Span::styled(
                    d.devnode.display().to_string(),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {:04x}:{:04x}  ", d.vid, d.pid), theme::muted()),
                Span::raw(d.product.clone().unwrap_or_default()),
            ]))
        })
        .collect();
    if items.is_empty() {
        items.push(ListItem::new(TLine::styled(
            "no other USB serial ports",
            theme::muted().add_modifier(Modifier::ITALIC),
        )));
    }
    picker(
        f,
        area,
        "Other ports",
        &[("Enter", "connect"), ("Esc", "cancel")],
        items,
        view.picker_idx,
        64,
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
    fn raw_mode_sends_keys_and_ctrl_bracket_leaves() {
        let mut c = core_with(1);
        let mut v = View::new();
        assert_eq!(on_key(&mut v, &mut c, ch('I')), UiOutcome::Redraw);
        assert_eq!(v.mode, Mode::Raw);
        let send = |b: &[u8]| UiOutcome::Action(Action::SendRaw(b.to_vec()));
        assert_eq!(on_key(&mut v, &mut c, ch('q')), send(b"q"));
        assert_eq!(on_key(&mut v, &mut c, key(KeyCode::Up)), send(b"\x1b[A"));
        assert_eq!(on_key(&mut v, &mut c, key(KeyCode::Tab)), send(b"\t"));
        assert_eq!(on_key(&mut v, &mut c, key(KeyCode::Esc)), send(b"\x1b"));
        let ctrl = |k| KeyEvent::new(k, KeyModifiers::CONTROL);
        // Ctrl-C reaches the board instead of quitting comrade.
        assert_eq!(on_key(&mut v, &mut c, ctrl(KeyCode::Char('c'))), send(&[3]));
        assert_eq!(on_paste(&mut v, "a\nb"), send(b"a\rb"));
        assert_eq!(
            on_key(&mut v, &mut c, ctrl(KeyCode::Char(']'))),
            UiOutcome::Redraw
        );
        assert_eq!(v.mode, Mode::Log);
    }

    #[test]
    fn y_copies_bottom_line_and_m_toggles_mouse() {
        let mut c = core_with(1);
        let mut v = View::new();
        assert_eq!(on_key(&mut v, &mut c, ch('y')), UiOutcome::None);
        c.tabs_mut()[0].scroll.push(crate::lines::Line {
            at: chrono::Local::now(),
            kind: crate::lines::LineKind::Data,
            text: "\x1b[31mhello\x1b[0m".into(),
        });
        assert_eq!(
            on_key(&mut v, &mut c, ch('y')),
            UiOutcome::Copy("hello".into())
        );
        assert_eq!(on_key(&mut v, &mut c, ch('m')), UiOutcome::ToggleMouse);
    }

    #[test]
    fn f_filters_to_search_matches() {
        let mut c = core_with(1);
        let mut v = View::new();
        for t in ["alpha", "beta", "alpha two", "gamma"] {
            c.tabs_mut()[0].scroll.push(crate::lines::Line {
                at: chrono::Local::now(),
                kind: crate::lines::LineKind::Data,
                text: t.into(),
            });
        }
        // No search yet: f does nothing but explain.
        on_key(&mut v, &mut c, ch('f'));
        assert!(!v.filter);
        for k in "/alpha".chars() {
            on_key(&mut v, &mut c, ch(k));
        }
        on_key(&mut v, &mut c, key(KeyCode::Enter));
        on_key(&mut v, &mut c, ch('f'));
        assert!(v.filter);
        let rows = draw(&c, &v, 60, 10);
        let body = rows.join("\n");
        assert!(body.contains("alpha two"), "{body}");
        assert!(!body.contains("beta") && !body.contains("gamma"), "{body}");
        assert!(body.contains("filter \"alpha\""));
        on_key(&mut v, &mut c, key(KeyCode::Esc));
        assert!(!v.filter && v.search.is_empty());
        assert!(draw(&c, &v, 60, 10).join("\n").contains("gamma"));
    }

    #[test]
    fn raw_mode_row_and_badge() {
        let c = core_with(1);
        let mut v = View::new();
        v.mode = Mode::Raw;
        let rows = draw(&c, &v, 80, 10);
        assert!(rows.iter().any(|r| r.contains("RAW")));
        assert!(rows.iter().any(|r| r.contains("Ctrl-] leaves")));
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
        let idx = row.find("xyz").unwrap() + 3;
        assert_eq!(
            pos.x as usize,
            row[..idx].width(),
            "display column, not byte"
        );
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

    #[test]
    fn tab_overflow_keeps_focus_visible() {
        assert_eq!(visible_tabs(&[5, 5, 5], 0, 20), 0..3);
        let r = visible_tabs(&[10; 10], 7, 40);
        assert!(r.contains(&7), "{r:?}");
        assert!(r.len() * 10 <= 36, "{r:?}");
        let mut c = core_with(12);
        c.action(Action::Focus(11), now());
        let rows = draw(&c, &View::new(), 60, 10);
        assert!(rows[0].contains("12 "), "{}", rows[0]);
        assert!(rows[0].contains('‹'), "{}", rows[0]);
    }

    #[test]
    fn click_on_tab_focuses_it() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut c = core_with(3);
        let mut v = View::new();
        let rows = draw(&c, &v, 100, 10);
        let col = rows[0][..rows[0].find(" 3 ").unwrap() + 1].width() as u16;
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let mut t = Terminal::new(TestBackend::new(100, 10)).unwrap();
        t.draw(|f| render(f, &c, &v)).unwrap();
        assert_eq!(
            on_mouse(&mut v, &mut c, click),
            UiOutcome::Action(Action::Focus(2))
        );
        let miss = MouseEvent {
            column: 99,
            ..click
        };
        assert_eq!(on_mouse(&mut v, &mut c, miss), UiOutcome::None);
    }

    #[test]
    fn active_tab_is_underlined_in_accent() {
        let c = core_with(2);
        let mut t = Terminal::new(TestBackend::new(80, 10)).unwrap();
        t.draw(|f| render(f, &c, &View::new())).unwrap();
        let buf = t.backend().buffer();
        let heavy: Vec<u16> = (0..80).filter(|&x| buf[(x, 1)].symbol() == "━").collect();
        assert!(!heavy.is_empty());
        let row0: String = (0..80).map(|x| buf[(x, 0)].symbol()).collect();
        let label = format!(" {} ", c.focus() + 1);
        let focus_col = row0[..row0.find(&label).unwrap()].width() as u16;
        assert!(heavy.contains(&focus_col), "{heavy:?} {focus_col}");
    }

    #[test]
    fn separator_label_is_colored_and_centered() {
        let l = separator_line("──── reset · 14:02:40 ────", 40);
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.width(), 40);
        assert!(text.contains("reset · 14:02:40"));
        let label = l.spans.iter().find(|s| s.content == "reset").unwrap();
        assert!(label.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn baud_picker_marks_current() {
        let mut c = core_with(1);
        let mut v = View::new();
        on_key(&mut v, &mut c, ch('b'));
        let rows = draw(&c, &v, 80, 24);
        let cur = rows.iter().find(|r| r.contains("current")).unwrap();
        assert!(cur.contains("115200"), "{cur}");
    }

    #[test]
    fn status_shows_mode_and_fits_hints() {
        let mut c = core_with(1);
        c.tabs_mut()[0].state = TabState::Connected;
        let mut v = View::new();
        let wide = draw(&c, &v, 120, 10);
        assert!(wide[9].starts_with(" LOG "), "{}", wide[9]);
        assert!(wide[9].contains("? help") && wide[9].contains("q quit"));
        let narrow = draw(&c, &v, 50, 10);
        assert!(!narrow[9].contains("q quit"), "{}", narrow[9]);
        assert!(narrow[9].contains("connected"));
        v.mode = Mode::Send;
        let send = draw(&c, &v, 120, 10);
        assert!(send[9].starts_with(" SEND ") && send[9].contains("Esc back"));
    }

    #[test]
    fn empty_state_explains_what_to_do() {
        let c = Core::new(HashMap::new(), 115_200, vec![]);
        let rows = draw(&c, &View::new(), 80, 16);
        assert!(rows.iter().any(|r| r.contains("waiting for a board")));
        assert!(rows.iter().any(|r| r.contains("c other ports")));
        assert!(!rows[15].contains("b baud"), "{}", rows[15]);
    }
}
