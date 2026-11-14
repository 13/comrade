//! View-only state: modes, input boxes, toggles. No serial or board logic.

use std::cell::Cell;
use std::collections::HashMap;
use std::time::Instant;

use crate::board::BoardKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Log,
    Send,
    Search,
    BaudPicker,
    OtherPorts,
    Help,
}

pub struct View {
    pub mode: Mode,
    pub input: String,
    pub history: Vec<String>,
    pub hist_idx: Option<usize>,
    pub search: String,
    pub timestamps: bool,
    pub wrap: bool,
    pub picker_idx: usize,
    pub custom_baud: String,
    pub status_msg: Option<(String, Instant)>,
    /// Filled by the app once per second from `rx_bytes` deltas.
    pub rate_kbps: HashMap<BoardKey, f64>,
    /// Height of the log area at the last render (for PgUp/PgDn).
    pub last_log_rows: Cell<u16>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            mode: Mode::Log,
            input: String::new(),
            history: Vec::new(),
            hist_idx: None,
            search: String::new(),
            timestamps: true,
            wrap: true,
            picker_idx: 0,
            custom_baud: String::new(),
            status_msg: None,
            rate_kbps: HashMap::new(),
            last_log_rows: Cell::new(0),
        }
    }
}

impl View {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = Some((msg.into(), Instant::now()));
    }
}
