//! Key, mouse and paste handling. Scroll and search mutate the focused tab's
//! scrollback directly; board-affecting keys come back as `Action`s.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use super::view::{Mode, View};
use crate::core::{Action, Core};

/// Baud picker list (global constraints), plus a final `custom…` entry.
pub const BAUDS: [u32; 16] = [
    300, 1200, 2400, 4800, 9600, 19200, 38400, 57600, 74880, 115_200, 230_400, 460_800, 921_600,
    1_000_000, 1_500_000, 2_000_000,
];

#[derive(Debug, PartialEq)]
pub enum UiOutcome {
    None,
    Action(Action),
    Quit,
    Save,
    /// Copy this text to the clipboard (OSC 52).
    Copy(String),
    /// Turn mouse capture on or off.
    ToggleMouse,
    Redraw,
}

fn scroll_focused(core: &mut Core, f: impl FnOnce(&mut crate::scrollback::Scrollback)) {
    let i = core.focus();
    if let Some(t) = core.tabs_mut().get_mut(i) {
        f(&mut t.scroll);
    }
}

/// Search backward (toward older lines) from `from`, inclusive; wrap from the newest.
fn find_older(
    sb: &crate::scrollback::Scrollback,
    needle: &str,
    from: Option<usize>,
) -> Option<usize> {
    if sb.is_empty() {
        return None;
    }
    let last = sb.len() - 1;
    from.and_then(|f| sb.search(needle, f, false))
        .or_else(|| sb.search(needle, last, false))
}

/// Search forward (toward newer lines) from `from`, inclusive; wrap from the oldest.
fn find_newer(sb: &crate::scrollback::Scrollback, needle: &str, from: usize) -> Option<usize> {
    sb.search(needle, from, true)
        .or_else(|| sb.search(needle, 0, true))
}

/// Re-run the incremental search from the origin saved when `/` was pressed.
/// An empty needle restores the origin view.
fn search_incremental(view: &View, core: &mut Core) {
    let i = core.focus();
    let Some(t) = core.tabs_mut().get_mut(i) else {
        return;
    };
    let origin = view.search_origin;
    if view.search.is_empty() {
        match origin {
            Some((_, true)) => t.scroll.end(),
            Some((pos, false)) => t.scroll.jump_to(pos),
            None => {}
        }
        return;
    }
    let from = origin
        .map(|(p, _)| p)
        .unwrap_or_else(|| t.scroll.position());
    if let Some(h) = find_older(&t.scroll, &view.search, Some(from)) {
        t.scroll.jump_to(h);
    }
}

/// `n` (older) / `N` (newer) relative to the current position, wrapping.
fn search_step(view: &View, core: &mut Core, older: bool) {
    if view.search.is_empty() {
        return;
    }
    let i = core.focus();
    let Some(t) = core.tabs_mut().get_mut(i) else {
        return;
    };
    let pos = t.scroll.position();
    let hit = if older {
        find_older(&t.scroll, &view.search, pos.checked_sub(1))
    } else {
        find_newer(&t.scroll, &view.search, pos + 1)
    };
    if let Some(h) = hit {
        t.scroll.jump_to(h);
    }
}

pub fn on_key(view: &mut View, core: &mut Core, key: KeyEvent) -> UiOutcome {
    if key.kind == KeyEventKind::Release {
        return UiOutcome::None;
    }
    if view.mode == Mode::Raw {
        return raw_key(view, core, key);
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && key.code == KeyCode::Char('c') {
        return UiOutcome::Quit;
    }
    if key.code == KeyCode::Esc {
        if view.mode == Mode::Log {
            view.search.clear();
        }
        if view.mode == Mode::Search && view.search.is_empty() {
            search_incremental(view, core);
        }
        view.search_origin = None;
        view.mode = Mode::Log;
        return UiOutcome::Redraw;
    }
    match view.mode {
        Mode::Log => log_key(view, core, key),
        Mode::Send => send_key(view, key),
        Mode::Raw => UiOutcome::None,
        Mode::Search => search_key(view, core, key),
        Mode::BaudPicker => baud_key(view, key),
        Mode::OtherPorts => ports_key(view, core, key),
        Mode::Help => {
            view.mode = Mode::Log;
            UiOutcome::Redraw
        }
    }
}

fn log_key(view: &mut View, core: &mut Core, key: KeyEvent) -> UiOutcome {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return UiOutcome::None;
    }
    let page = view.last_log_rows.get().max(1) as usize;
    match key.code {
        KeyCode::Char(c @ '1'..='9') => UiOutcome::Action(Action::Focus(c as usize - '1' as usize)),
        KeyCode::Tab => UiOutcome::Action(Action::NextTab),
        KeyCode::BackTab => UiOutcome::Action(Action::PrevTab),
        KeyCode::Up => {
            scroll_focused(core, |s| s.scroll_up(1));
            UiOutcome::Redraw
        }
        KeyCode::Down => {
            scroll_focused(core, |s| s.scroll_down(1));
            UiOutcome::Redraw
        }
        KeyCode::PageUp => {
            scroll_focused(core, |s| s.scroll_up(page));
            UiOutcome::Redraw
        }
        KeyCode::PageDown => {
            scroll_focused(core, |s| s.scroll_down(page));
            UiOutcome::Redraw
        }
        KeyCode::Home => {
            scroll_focused(core, |s| s.home());
            UiOutcome::Redraw
        }
        KeyCode::End => {
            scroll_focused(core, |s| s.end());
            UiOutcome::Redraw
        }
        KeyCode::Enter | KeyCode::Char('i') => {
            view.mode = Mode::Send;
            view.hist_idx = None;
            UiOutcome::Redraw
        }
        KeyCode::Char('I') => {
            view.mode = Mode::Raw;
            UiOutcome::Redraw
        }
        KeyCode::Char('/') => {
            view.mode = Mode::Search;
            view.search.clear();
            let i = core.focus();
            view.search_origin = core
                .tabs()
                .get(i)
                .map(|t| (t.scroll.position(), t.scroll.follow()));
            UiOutcome::Redraw
        }
        KeyCode::Char('n') => {
            search_step(view, core, true);
            UiOutcome::Redraw
        }
        KeyCode::Char('N') => {
            search_step(view, core, false);
            UiOutcome::Redraw
        }
        KeyCode::Char('b') => {
            let baud = core.tabs().get(core.focus()).map(|t| t.baud);
            view.picker_idx = baud
                .and_then(|b| BAUDS.iter().position(|&x| x == b))
                .unwrap_or(BAUDS.len());
            view.custom_baud = match baud {
                Some(b) if !BAUDS.contains(&b) => b.to_string(),
                _ => String::new(),
            };
            view.mode = Mode::BaudPicker;
            UiOutcome::Redraw
        }
        KeyCode::Char('p') => UiOutcome::Action(Action::TogglePause),
        KeyCode::Char('r') => UiOutcome::Action(Action::Reset),
        KeyCode::Char('t') => {
            view.timestamps = !view.timestamps;
            UiOutcome::Redraw
        }
        KeyCode::Char('w') => {
            view.wrap = !view.wrap;
            UiOutcome::Redraw
        }
        KeyCode::Char('c') => {
            view.picker_idx = 0;
            view.mode = Mode::OtherPorts;
            UiOutcome::Redraw
        }
        KeyCode::Char('s') => UiOutcome::Save,
        KeyCode::Char('m') => UiOutcome::ToggleMouse,
        KeyCode::Char('y') => {
            let i = core.focus();
            core.tabs()
                .get(i)
                .and_then(|t| t.scroll.get(t.scroll.position()))
                .map_or(UiOutcome::None, |l| {
                    UiOutcome::Copy(crate::scrollback::strip_sgr(&l.text).into_owned())
                })
        }
        KeyCode::Char('C') => UiOutcome::Action(Action::ClearTab),
        KeyCode::Char('x') => UiOutcome::Action(Action::CloseTab),
        KeyCode::Char('?') => {
            view.mode = Mode::Help;
            UiOutcome::Redraw
        }
        KeyCode::Char('q') => UiOutcome::Quit,
        _ => UiOutcome::None,
    }
}

/// The bytes a key press sends to the board in raw mode, like a terminal
/// would: control letters, arrows and Home/End as ANSI sequences.
fn raw_bytes(core: &Core, key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out = match key.code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => {
            vec![c.to_ascii_lowercase() as u8 - b'a' + 1]
        }
        KeyCode::Char('[') if ctrl => vec![0x1b],
        KeyCode::Char('\\') if ctrl => vec![0x1c],
        KeyCode::Char('^') if ctrl => vec![0x1e],
        KeyCode::Char('_') if ctrl => vec![0x1f],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => core.raw_enter().to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => return Vec::new(),
    };
    if alt && !out.is_empty() && !matches!(key.code, KeyCode::Esc) {
        out.insert(0, 0x1b);
    }
    out
}

/// Raw mode: only `Ctrl-]` is kept for comrade; everything else is sent.
fn raw_key(view: &mut View, core: &Core, key: KeyEvent) -> UiOutcome {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(']') {
        view.mode = Mode::Log;
        return UiOutcome::Redraw;
    }
    let bytes = raw_bytes(core, key);
    if bytes.is_empty() {
        UiOutcome::None
    } else {
        UiOutcome::Action(Action::SendRaw(bytes))
    }
}

fn send_key(view: &mut View, key: KeyEvent) -> UiOutcome {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('e') => UiOutcome::Action(Action::CycleLineEnding),
            _ => UiOutcome::None,
        };
    }
    match key.code {
        KeyCode::Char(c) => {
            view.input.push(c);
            view.hist_idx = None;
            UiOutcome::Redraw
        }
        KeyCode::Backspace => {
            view.input.pop();
            UiOutcome::Redraw
        }
        KeyCode::Enter => {
            // Stays in Send mode so repeated commands are quick; Esc leaves.
            let text = std::mem::take(&mut view.input);
            if !text.is_empty() && view.history.last() != Some(&text) {
                view.history.push(text.clone());
            }
            view.hist_idx = None;
            UiOutcome::Action(Action::Send(text))
        }
        KeyCode::Up => {
            if view.history.is_empty() {
                return UiOutcome::None;
            }
            let idx = match view.hist_idx {
                None => view.history.len() - 1,
                Some(i) => i.saturating_sub(1),
            };
            view.hist_idx = Some(idx);
            view.input = view.history[idx].clone();
            UiOutcome::Redraw
        }
        KeyCode::Down => {
            match view.hist_idx {
                Some(i) if i + 1 < view.history.len() => {
                    view.hist_idx = Some(i + 1);
                    view.input = view.history[i + 1].clone();
                }
                Some(_) => {
                    view.hist_idx = None;
                    view.input.clear();
                }
                None => return UiOutcome::None,
            }
            UiOutcome::Redraw
        }
        _ => UiOutcome::None,
    }
}

fn search_key(view: &mut View, core: &mut Core, key: KeyEvent) -> UiOutcome {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return UiOutcome::None;
    }
    match key.code {
        KeyCode::Char(c) => {
            view.search.push(c);
            search_incremental(view, core);
            UiOutcome::Redraw
        }
        KeyCode::Backspace => {
            view.search.pop();
            search_incremental(view, core);
            UiOutcome::Redraw
        }
        KeyCode::Enter => {
            if view.search.is_empty() {
                search_incremental(view, core);
            }
            view.search_origin = None;
            view.mode = Mode::Log;
            UiOutcome::Redraw
        }
        _ => UiOutcome::None,
    }
}

fn baud_key(view: &mut View, key: KeyEvent) -> UiOutcome {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return UiOutcome::None;
    }
    match key.code {
        KeyCode::Up => {
            view.picker_idx = view.picker_idx.saturating_sub(1);
            UiOutcome::Redraw
        }
        KeyCode::Down => {
            view.picker_idx = (view.picker_idx + 1).min(BAUDS.len());
            UiOutcome::Redraw
        }
        KeyCode::Char(c) if c.is_ascii_digit() => {
            view.picker_idx = BAUDS.len();
            if view.custom_baud.len() < 9 {
                view.custom_baud.push(c);
            }
            UiOutcome::Redraw
        }
        KeyCode::Backspace if view.picker_idx == BAUDS.len() => {
            view.custom_baud.pop();
            UiOutcome::Redraw
        }
        KeyCode::Enter => {
            let baud = match BAUDS.get(view.picker_idx) {
                Some(&b) => Some(b),
                None => view.custom_baud.parse::<u32>().ok().filter(|&b| b > 0),
            };
            match baud {
                Some(b) => {
                    view.mode = Mode::Log;
                    UiOutcome::Action(Action::SetBaud(b))
                }
                None => UiOutcome::None,
            }
        }
        _ => UiOutcome::None,
    }
}

fn ports_key(view: &mut View, core: &Core, key: KeyEvent) -> UiOutcome {
    let n = core.other_ports().len();
    match key.code {
        KeyCode::Up => {
            view.picker_idx = view.picker_idx.saturating_sub(1);
            UiOutcome::Redraw
        }
        KeyCode::Down => {
            view.picker_idx = (view.picker_idx + 1).min(n.saturating_sub(1));
            UiOutcome::Redraw
        }
        KeyCode::Enter => {
            view.mode = Mode::Log;
            match core.other_ports().get(view.picker_idx) {
                Some(d) => UiOutcome::Action(Action::ConnectOther(d.devnode.clone())),
                None => UiOutcome::Redraw,
            }
        }
        _ => UiOutcome::None,
    }
}

pub fn on_mouse(view: &mut View, core: &mut Core, ev: MouseEvent) -> UiOutcome {
    if matches!(view.mode, Mode::Help | Mode::BaudPicker | Mode::OtherPorts) {
        return UiOutcome::None;
    }
    match ev.kind {
        MouseEventKind::ScrollUp => {
            scroll_focused(core, |s| s.scroll_up(3));
            UiOutcome::Redraw
        }
        MouseEventKind::ScrollDown => {
            scroll_focused(core, |s| s.scroll_down(3));
            UiOutcome::Redraw
        }
        MouseEventKind::Down(MouseButton::Left) if ev.row == 0 => view
            .tab_hits
            .borrow()
            .iter()
            .find(|(cols, _)| cols.contains(&ev.column))
            .map_or(UiOutcome::None, |&(_, i)| {
                UiOutcome::Action(Action::Focus(i))
            }),
        _ => UiOutcome::None,
    }
}

pub fn on_paste(view: &mut View, s: &str) -> UiOutcome {
    if view.mode == Mode::Raw {
        let bytes: Vec<u8> = s.replace("\r\n", "\r").replace('\n', "\r").into_bytes();
        return UiOutcome::Action(Action::SendRaw(bytes));
    }
    if view.mode != Mode::Send {
        return UiOutcome::None;
    }
    view.input.extend(s.chars().filter(|c| !c.is_control()));
    UiOutcome::Redraw
}
