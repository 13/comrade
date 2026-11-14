//! View-only state: modes, input boxes, toggles. No serial or board logic.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::time::Instant;

use crate::board::BoardKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Log,
    Send,
    /// Every key goes straight to the board; `Ctrl-]` leaves.
    Raw,
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
    /// Where `/` was pressed: (buffer position, was following).
    pub search_origin: Option<(usize, bool)>,
    pub timestamps: bool,
    pub wrap: bool,
    pub picker_idx: usize,
    pub custom_baud: String,
    pub status_msg: Option<(String, Instant)>,
    /// Filled by the app once per second from `rx_bytes` deltas.
    pub rate_kbps: HashMap<BoardKey, f64>,
    /// Height of the log area at the last render (for PgUp/PgDn).
    pub last_log_rows: Cell<u16>,
    /// Columns of row 0 each tab label covered at the last render, for clicks.
    pub tab_hits: RefCell<Vec<(Range<u16>, usize)>>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            mode: Mode::Log,
            input: String::new(),
            history: Vec::new(),
            hist_idx: None,
            search: String::new(),
            search_origin: None,
            timestamps: true,
            wrap: true,
            picker_idx: 0,
            custom_baud: String::new(),
            status_msg: None,
            rate_kbps: HashMap::new(),
            last_log_rows: Cell::new(0),
            tab_hits: RefCell::new(Vec::new()),
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
