use std::path::PathBuf;

use crate::board::{BoardKey, DeviceInfo, Family, LinePolicy};

pub type ConnId = u64;

#[derive(Clone, Copy, Debug)]
pub struct Now {
    pub mono: std::time::Instant,
    pub wall: chrono::DateTime<chrono::Local>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Holder {
    Named { pid: u32, comm: String },
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OpenError {
    PermissionDenied {
        group: String,
    },
    /// ENOENT / ENODEV / ENXIO: the node is not there (yet). The tab waits.
    NotPresent,
    Other(String),
}

pub enum Event {
    DeviceAdded(DeviceInfo),
    DeviceRemoved {
        devnode: PathBuf,
    },
    PortOpened {
        key: BoardKey,
        conn: ConnId,
    },
    PortOpenFailed {
        key: BoardKey,
        conn: ConnId,
        err: OpenError,
    },
    PortData {
        key: BoardKey,
        conn: ConnId,
        bytes: Vec<u8>,
    },
    PortGone {
        key: BoardKey,
        conn: ConnId,
        reason: String,
    },
    /// Something went wrong but the port stays open (e.g. a write timed
    /// out and its bytes were dropped). Shown as an info line.
    PortWarning {
        key: BoardKey,
        conn: ConnId,
        msg: String,
    },
    ForeignOpen {
        devnode: PathBuf,
        holder: Holder,
    },
    ForeignReleased {
        devnode: PathBuf,
    },
    /// The startup udev enumeration has been sent; later `DeviceAdded`
    /// events are hotplugs.
    EnumerationDone,
    Input(crossterm::event::Event),
}

#[derive(Debug, PartialEq)]
pub enum Command {
    Open {
        key: BoardKey,
        conn: ConnId,
        devnode: PathBuf,
        baud: u32,
        policy: LinePolicy,
    },
    Close {
        key: BoardKey,
    },
    Write {
        key: BoardKey,
        bytes: Vec<u8>,
    },
    SetBaud {
        key: BoardKey,
        baud: u32,
    },
    ResetPulse {
        key: BoardKey,
        family: Family,
    },
    Watch(Vec<PathBuf>),
    Persist {
        key: BoardKey,
        prefs: crate::store::BoardPrefs,
    },
}
