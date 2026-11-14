//! Semantic styles. Only the 16 ANSI colors are used, so the terminal's own
//! theme decides the actual shades (light and dark both work). Setting
//! `NO_COLOR` (non-empty) drops every color comrade adds; bold, dim, italic
//! and reverse stay so focus and state remain visible. Firmware colors are
//! content, not decoration, and are rendered either way.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

use crate::core::TabState;

fn colored() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()))
}

fn fg(c: Color) -> Style {
    if colored() {
        Style::new().fg(c)
    } else {
        Style::new()
    }
}

/// Filled badge: dark text on `c`. Without color it is reversed video.
fn badge(c: Color) -> Style {
    let s = Style::new().add_modifier(Modifier::BOLD);
    if colored() {
        s.fg(Color::Black).bg(c)
    } else {
        s.add_modifier(Modifier::REVERSED)
    }
}

pub const ACCENT: Color = Color::Cyan;

pub fn accent() -> Style {
    fg(ACCENT)
}
pub fn ok() -> Style {
    fg(Color::Green)
}
pub fn warn() -> Style {
    fg(Color::Yellow)
}
pub fn err() -> Style {
    fg(Color::Red)
}

/// Secondary text: timestamps, rules, labels. Dim rather than a fixed gray so
/// it stays readable on both light and dark backgrounds.
pub fn muted() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

pub fn brand() -> Style {
    accent().add_modifier(Modifier::BOLD)
}

/// A key name in a hint (`?`, `Enter`).
pub fn key() -> Style {
    accent().add_modifier(Modifier::BOLD)
}

pub fn border() -> Style {
    accent()
}

pub fn selected() -> Style {
    badge(ACCENT)
}

pub fn search_hit() -> Style {
    let s = Style::new();
    if colored() {
        s.fg(Color::Black).bg(Color::Yellow)
    } else {
        s.add_modifier(Modifier::REVERSED | Modifier::UNDERLINED)
    }
}

/// Mode pill at the left of the status line.
pub fn mode_badge(c: Color) -> Style {
    badge(c)
}

/// The floating "scrolled / +N new" pill.
pub fn pill() -> Style {
    badge(Color::Yellow)
}

pub fn state(s: &TabState) -> Style {
    match s {
        TabState::Connected => ok(),
        TabState::Paused(_) => warn(),
        TabState::Waiting | TabState::Connecting => muted(),
        TabState::Error(_) => err().add_modifier(Modifier::BOLD),
    }
}

/// Color of a `──── label · HH:MM:SS ────` separator's label.
pub fn separator_label(label: &str) -> Style {
    let s = match label {
        "reconnected" => ok(),
        "reset" => warn(),
        "upload" => fg(Color::Magenta),
        _ => accent(),
    };
    s.add_modifier(Modifier::BOLD)
}
