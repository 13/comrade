//! Pure state machine: events and actions in, commands out. No I/O.

mod pause;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::board::{
    BoardKey, Class, DeviceInfo, Family, LinePolicy, board_key, board_key_at_path, classify,
    friendly_name, line_policy,
};
use crate::event::{Command, ConnId, Event, Holder, Now, OpenError};
use crate::lines::{Line, LineAssembler, LineKind};
use crate::scrollback::{Scrollback, strip_sgr};
use crate::store::{BoardPrefs, LineEnding};
use pause::PauseTracker;

const SCROLLBACK_CAP: usize = 100_000;
const BANNER_QUIET: u8 = 8;
/// First reconnect retry after a lost connection; doubles per failure.
const RETRY_FIRST: Duration = Duration::from_secs(1);
/// Longest gap between reconnect retries.
const RETRY_MAX: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq)]
pub enum PauseReason {
    Upload(Holder),
    Manual,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TabState {
    Connecting,
    Connected,
    Paused(PauseReason),
    Waiting,
    Error(String),
}

pub struct Tab {
    pub key: BoardKey,
    pub name: String,
    pub family: Family,
    pub devnode: Option<PathBuf>,
    pub state: TabState,
    pub baud: u32,
    pub line_ending: LineEnding,
    pub scroll: Scrollback,
    pub asm: LineAssembler,
    pub conn: ConnId,
    pub highlight: bool,
    pub rx_bytes: u64,
    ever_connected: bool,
    data_this_conn: bool,
    /// Completed non-banner lines still to see before a banner counts as a new reset.
    banner_quiet: u8,
    /// Node a foreign holder had open when this tab entered `Paused(Upload)`.
    /// Kept after the node vanishes so the holder's release can be matched.
    upload_node: Option<PathBuf>,
    /// Set when an upload pause begins: the next `PortOpened` adds no
    /// `reconnected` separator (the `upload` one already marks the gap).
    after_upload: bool,
    /// When `on_timer` retries a lost or not-yet-present port.
    retry_at: Option<Instant>,
    /// Delay before the next retry (1 s doubling, capped at 5 s).
    backoff: Duration,
    /// Waiting only because auto-connect was blocked (upload pause or a
    /// retry that fell inside one): `on_timer` opens it once allowed.
    await_unblock: bool,
    /// The "not present" info line was already shown for this streak.
    missing_noted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Focus(usize),
    NextTab,
    PrevTab,
    Send(String),
    SetBaud(u32),
    TogglePause,
    Reset,
    ClearTab,
    CloseTab,
    ConnectOther(PathBuf),
    CycleLineEnding,
}

pub struct Core {
    tabs: Vec<Tab>,
    focus: usize,
    default_baud: u32,
    prefs: HashMap<BoardKey, BoardPrefs>,
    manual_ports: Vec<PathBuf>,
    /// Present Auto-class devices (including ones without a tab).
    present_auto: Vec<DeviceInfo>,
    /// Key each present Auto device was given (see `auto_key`).
    auto_keys: HashMap<PathBuf, BoardKey>,
    /// Present Other and Bootloader devices.
    other: Vec<DeviceInfo>,
    suppressed: HashSet<BoardKey>,
    next_conn: ConnId,
    last_watch: Vec<PathBuf>,
    pause: PauseTracker,
}

fn line(now: Now, kind: LineKind, text: String) -> Line {
    Line {
        at: now.wall,
        kind,
        text,
    }
}

fn separator(now: Now, label: &str) -> Line {
    line(
        now,
        LineKind::Separator,
        format!("──── {label} · {} ────", now.wall.format("%H:%M:%S")),
    )
}

fn is_rom_banner(l: &Line) -> bool {
    let plain = strip_sgr(&l.text);
    plain.starts_with("ESP-ROM:") || plain.starts_with("rst:0x")
}

impl Core {
    pub fn new(
        prefs: HashMap<BoardKey, BoardPrefs>,
        default_baud: u32,
        manual_ports: Vec<PathBuf>,
    ) -> Self {
        Self {
            tabs: Vec::new(),
            focus: 0,
            default_baud,
            prefs,
            manual_ports,
            present_auto: Vec::new(),
            auto_keys: HashMap::new(),
            other: Vec::new(),
            suppressed: HashSet::new(),
            next_conn: 1,
            last_watch: Vec::new(),
            pause: PauseTracker::new(),
        }
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn tabs_mut(&mut self) -> &mut [Tab] {
        &mut self.tabs
    }

    pub fn focus(&self) -> usize {
        self.focus
    }

    /// Per-board preferences as loaded and updated by `Persist`.
    pub fn prefs(&self) -> &HashMap<BoardKey, BoardPrefs> {
        &self.prefs
    }

    pub fn other_ports(&self) -> Vec<&DeviceInfo> {
        self.other.iter().collect()
    }

    /// Called when `next_deadline` passes. Every reopen here is deliberate:
    /// a tab that waited out an upload block (`await_unblock`) or whose
    /// reconnect retry is due (`retry_at`). A retry is an auto-connect, so
    /// during a global upload block it waits for the block to lift.
    pub fn on_timer(&mut self, now: Now) -> Vec<Command> {
        let mut cmds = Vec::new();
        self.pause.on_timer(now.mono);
        let allowed = self.pause.allows_auto_connect(now.mono);
        for idx in 0..self.tabs.len() {
            let t = &mut self.tabs[idx];
            if t.retry_at.is_some_and(|r| r <= now.mono) {
                t.retry_at = None;
                t.await_unblock = true;
            }
            if !t.await_unblock || !allowed {
                continue;
            }
            if t.state != TabState::Waiting {
                t.await_unblock = false;
                continue;
            }
            let Some(node) = t.devnode.clone() else {
                t.await_unblock = false;
                continue;
            };
            // Held: the holder's release brings a quiet deadline and us back.
            if self.pause.is_held(&node) {
                continue;
            }
            // udev-tracked boards only while their node is present. `--port`
            // and `c` tabs just try; a missing node fails with NotPresent.
            if t.family != Family::Unknown && !self.present_auto.iter().any(|d| d.devnode == node) {
                t.await_unblock = false;
                continue;
            }
            self.open_tab(idx, None, &mut cmds);
        }
        self.finish(cmds)
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.tabs
            .iter()
            .filter_map(|t| t.retry_at)
            .chain(self.pause.deadline())
            .min()
    }

    /// Open the `--port` ports as tabs.
    pub fn startup(&mut self, now: Now) -> Vec<Command> {
        let mut cmds = Vec::new();
        for path in self.manual_ports.clone() {
            let key = BoardKey(format!("manual:{}", path.display()));
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let idx = self.find_or_create(key, name, Family::Unknown, now);
            self.open_tab(idx, Some(path), &mut cmds);
        }
        self.finish(cmds)
    }

    pub fn handle(&mut self, ev: Event, now: Now) -> Vec<Command> {
        let mut cmds = Vec::new();
        match ev {
            Event::DeviceAdded(dev) => self.device_added(dev, now, &mut cmds),
            Event::DeviceRemoved { devnode } => self.device_removed(devnode, &mut cmds),
            Event::PortOpened { key, conn } => {
                if let Some(i) = self.matching(&key, conn) {
                    let t = &mut self.tabs[i];
                    t.state = TabState::Connected;
                    if t.after_upload {
                        t.after_upload = false;
                        t.banner_quiet = BANNER_QUIET;
                    } else if t.ever_connected {
                        t.scroll.push(separator(now, "reconnected"));
                        t.banner_quiet = BANNER_QUIET;
                    }
                    t.ever_connected = true;
                    t.missing_noted = false;
                }
            }
            Event::PortOpenFailed { key, conn, err } => {
                if let Some(i) = self.matching(&key, conn) {
                    let t = &mut self.tabs[i];
                    let dev = t
                        .devnode
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    let msg = match err {
                        OpenError::PermissionDenied { group } => format!(
                            "no permission for {dev} — add yourself to group '{group}' and re-login"
                        ),
                        OpenError::Other(m) => m,
                        OpenError::NotPresent => {
                            // Not an error: the node is missing (a --port
                            // path not created yet, or gone meanwhile).
                            if !t.missing_noted {
                                t.missing_noted = true;
                                t.scroll.push(line(
                                    now,
                                    LineKind::Info,
                                    format!("{dev} not present, waiting"),
                                ));
                            }
                            t.state = TabState::Waiting;
                            t.conn = 0;
                            Self::schedule_retry(t, now);
                            return self.finish(cmds);
                        }
                    };
                    t.scroll.push(line(now, LineKind::Error, msg.clone()));
                    t.state = TabState::Error(msg);
                }
            }
            Event::PortData { key, conn, bytes } => {
                if let Some(i) = self.matching(&key, conn) {
                    self.port_data(i, &bytes, now);
                }
            }
            Event::PortGone { key, conn, reason } => {
                if let Some(i) = self.matching(&key, conn) {
                    let t = &mut self.tabs[i];
                    Self::flush_partial(t);
                    t.scroll.push(line(
                        now,
                        LineKind::Info,
                        format!("connection lost: {reason}"),
                    ));
                    // devnode kept: a board that is still present (I/O
                    // error, dead worker) is retried; an unplug clears it.
                    t.state = TabState::Waiting;
                    t.conn = 0;
                    Self::schedule_retry(t, now);
                    cmds.push(Command::Close { key });
                }
            }
            Event::PortWarning { key, conn, msg } => {
                if let Some(i) = self.matching(&key, conn) {
                    self.tabs[i].scroll.push(line(now, LineKind::Info, msg));
                }
            }
            Event::ForeignOpen { devnode, holder } => {
                self.foreign_open(devnode, holder, now, &mut cmds)
            }
            Event::ForeignReleased { devnode } => self.foreign_released(devnode, now, &mut cmds),
            Event::Input(_) => {}
        }
        self.finish(cmds)
    }

    pub fn action(&mut self, a: Action, now: Now) -> Vec<Command> {
        let mut cmds = Vec::new();
        match a {
            Action::Focus(i) => self.set_focus(i),
            Action::NextTab => {
                if !self.tabs.is_empty() {
                    self.set_focus((self.focus + 1) % self.tabs.len());
                }
            }
            Action::PrevTab => {
                if !self.tabs.is_empty() {
                    self.set_focus((self.focus + self.tabs.len() - 1) % self.tabs.len());
                }
            }
            Action::Send(s) => self.send(s, now, &mut cmds),
            Action::SetBaud(baud) => {
                if let Some(t) = self.tabs.get_mut(self.focus) {
                    t.baud = baud;
                    if t.state == TabState::Connected {
                        cmds.push(Command::SetBaud {
                            key: t.key.clone(),
                            baud,
                        });
                    }
                    self.persist(&mut cmds);
                }
            }
            Action::CycleLineEnding => {
                if let Some(t) = self.tabs.get_mut(self.focus) {
                    t.line_ending = match t.line_ending {
                        LineEnding::Lf => LineEnding::CrLf,
                        LineEnding::CrLf => LineEnding::None,
                        LineEnding::None => LineEnding::Lf,
                    };
                    self.persist(&mut cmds);
                }
            }
            Action::TogglePause => self.toggle_pause(&mut cmds),
            Action::Reset => {
                if let Some(t) = self.tabs.get_mut(self.focus) {
                    if t.family == Family::NativeCdc {
                        // DTR/RTS do not reset these boards, and dropping
                        // DTR would only pause `while (!Serial)` firmware.
                        t.scroll.push(line(
                            now,
                            LineKind::Info,
                            "reset not supported on native USB boards".into(),
                        ));
                    } else if t.state == TabState::Connected {
                        Self::flush_partial(t);
                        t.banner_quiet = BANNER_QUIET;
                        cmds.push(Command::ResetPulse {
                            key: t.key.clone(),
                            family: t.family,
                        });
                        t.scroll.push(separator(now, "reset"));
                        t.data_this_conn = false;
                    } else {
                        t.scroll
                            .push(line(now, LineKind::Info, "not connected".into()));
                    }
                }
            }
            Action::ClearTab => {
                if let Some(t) = self.tabs.get_mut(self.focus) {
                    t.scroll.clear();
                }
            }
            Action::CloseTab => self.close_tab(&mut cmds),
            Action::ConnectOther(path) => self.connect_other(path, now, &mut cmds),
        }
        self.finish(cmds)
    }

    // ---- internals ----

    fn finish(&mut self, mut cmds: Vec<Command>) -> Vec<Command> {
        let mut watch: Vec<PathBuf> = self
            .present_auto
            .iter()
            .map(|d| d.devnode.clone())
            .chain(
                self.other
                    .iter()
                    .filter(|d| matches!(classify(d.vid, d.pid), Class::Bootloader))
                    .map(|d| d.devnode.clone()),
            )
            .chain(
                self.tabs
                    .iter()
                    .filter(|t| t.family == Family::Unknown)
                    .filter_map(|t| t.devnode.clone()),
            )
            .collect();
        watch.sort();
        watch.dedup();
        if watch != self.last_watch {
            self.last_watch = watch.clone();
            cmds.push(Command::Watch(watch));
        }
        cmds
    }

    fn matching(&self, key: &BoardKey, conn: ConnId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| &t.key == key && t.conn == conn)
    }

    fn set_focus(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.focus = i;
            self.tabs[i].highlight = false;
        }
    }

    fn flush_partial(t: &mut Tab) {
        if let Some(l) = t.asm.flush() {
            t.scroll.push(l);
        }
    }

    fn schedule_retry(t: &mut Tab, now: Now) {
        t.retry_at = Some(now.mono + t.backoff);
        t.backoff = (t.backoff * 2).min(RETRY_MAX);
    }

    fn find_or_create(&mut self, key: BoardKey, name: String, family: Family, _now: Now) -> usize {
        if let Some(i) = self.tabs.iter().position(|t| t.key == key) {
            return i;
        }
        let (baud, line_ending) = match self.prefs.get(&key) {
            Some(p) => (p.baud, p.line_ending),
            None => (self.default_baud, LineEnding::Lf),
        };
        let focused_connected = self
            .tabs
            .get(self.focus)
            .is_some_and(|t| t.state == TabState::Connected);
        self.tabs.push(Tab {
            key,
            name,
            family,
            devnode: None,
            state: TabState::Waiting,
            baud,
            line_ending,
            scroll: Scrollback::new(SCROLLBACK_CAP),
            asm: LineAssembler::new(),
            conn: 0,
            highlight: false,
            rx_bytes: 0,
            ever_connected: false,
            data_this_conn: false,
            banner_quiet: 0,
            upload_node: None,
            after_upload: false,
            retry_at: None,
            backoff: RETRY_FIRST,
            await_unblock: false,
            missing_noted: false,
        });
        let idx = self.tabs.len() - 1;
        if self.tabs.len() > 1 && focused_connected {
            self.tabs[idx].highlight = true;
        } else {
            self.set_focus(idx);
        }
        idx
    }

    fn open_tab(&mut self, idx: usize, devnode: Option<PathBuf>, cmds: &mut Vec<Command>) {
        let conn = self.next_conn;
        self.next_conn += 1;
        let t = &mut self.tabs[idx];
        if devnode.is_some() {
            t.devnode = devnode;
        }
        let Some(path) = t.devnode.clone() else {
            return;
        };
        t.conn = conn;
        t.state = TabState::Connecting;
        t.data_this_conn = false;
        t.retry_at = None;
        t.await_unblock = false;
        let policy: LinePolicy = line_policy(t.family);
        cmds.push(Command::Open {
            key: t.key.clone(),
            conn,
            devnode: path,
            baud: t.baud,
            policy,
        });
    }

    fn device_added(&mut self, dev: DeviceInfo, now: Now, cmds: &mut Vec<Command>) {
        self.present_auto.retain(|d| d.devnode != dev.devnode);
        self.auto_keys.remove(&dev.devnode);
        self.other.retain(|d| d.devnode != dev.devnode);
        match classify(dev.vid, dev.pid) {
            Class::Bootloader => self.other.push(dev),
            Class::Other => {
                let key = board_key(&dev);
                self.other.push(dev.clone());
                // A port connected with `c` behaves like any board (spec
                // §6.1): its replug reattaches to the same tab.
                if self.suppressed.contains(&key) || self.owned_by_other(&key, &dev.devnode) {
                    return;
                }
                if let Some(idx) = self.tabs.iter().position(|t| t.key == key) {
                    self.attach(idx, dev.devnode, now, cmds);
                }
            }
            Class::Auto(family) => {
                let key = self.auto_key(&dev);
                self.present_auto.push(dev.clone());
                self.auto_keys.insert(dev.devnode.clone(), key.clone());
                if self.suppressed.contains(&key) {
                    return;
                }
                // A tab (e.g. a --port one) already owns this node.
                if self.owned_by_other(&key, &dev.devnode) {
                    return;
                }
                let idx = self.find_or_create(key, friendly_name(&dev), family, now);
                self.attach(idx, dev.devnode, now, cmds);
            }
        }
    }

    /// Points tab `idx` at the (re)appeared `devnode` and opens it when the
    /// pause rules allow; otherwise it waits for `on_timer`.
    fn attach(&mut self, idx: usize, devnode: PathBuf, now: Now, cmds: &mut Vec<Command>) {
        let held = self.pause.is_held(&devnode);
        let can_open = !held && self.pause.allows_auto_connect(now.mono);
        let t = &mut self.tabs[idx];
        // The device (re)appeared: start the retry backoff afresh.
        t.backoff = RETRY_FIRST;
        t.retry_at = None;
        let same_node = t.devnode.as_ref() == Some(&devnode);
        match t.state {
            TabState::Connecting | TabState::Connected if same_node => {}
            TabState::Paused(PauseReason::Manual) => t.devnode = Some(devnode),
            TabState::Paused(PauseReason::Upload(_)) if held => t.devnode = Some(devnode),
            _ if can_open => self.open_tab(idx, Some(devnode), cmds),
            // Live on another node; leave it alone while blocked.
            TabState::Connecting | TabState::Connected => {}
            // Blocked: wait for `on_timer` to open it.
            _ => {
                t.devnode = Some(devnode);
                t.state = TabState::Waiting;
                t.await_unblock = true;
            }
        }
    }

    /// Key for a new Auto device. Normally `board_key`; but when that key's
    /// tab is live on a different node that is still present, this is a
    /// second board with the same USB serial (many CP2102 clones report
    /// `0001`), so it gets its own key with the port path appended.
    fn auto_key(&self, dev: &DeviceInfo) -> BoardKey {
        let key = board_key(dev);
        let taken = self.tabs.iter().any(|t| {
            t.key == key
                && matches!(
                    t.state,
                    TabState::Connecting
                        | TabState::Connected
                        | TabState::Paused(PauseReason::Upload(_))
                )
                && t.devnode.as_ref().is_some_and(|n| {
                    n != &dev.devnode && self.present_auto.iter().any(|d| &d.devnode == n)
                })
        });
        if taken { board_key_at_path(dev) } else { key }
    }

    /// Key a present device is known under.
    fn key_of(&self, dev: &DeviceInfo) -> BoardKey {
        self.auto_keys
            .get(&dev.devnode)
            .cloned()
            .unwrap_or_else(|| board_key(dev))
    }

    fn device_removed(&mut self, devnode: PathBuf, cmds: &mut Vec<Command>) {
        if let Some(d) = self
            .present_auto
            .iter()
            .chain(self.other.iter())
            .find(|d| d.devnode == devnode)
        {
            let key = self.key_of(d);
            self.suppressed.remove(&key);
        }
        self.present_auto.retain(|d| d.devnode != devnode);
        self.auto_keys.remove(&devnode);
        self.other.retain(|d| d.devnode != devnode);
        let Some(t) = self
            .tabs
            .iter_mut()
            .find(|t| t.devnode.as_ref() == Some(&devnode))
        else {
            return;
        };
        t.retry_at = None;
        t.await_unblock = false;
        match t.state {
            TabState::Connecting | TabState::Connected => {
                Self::flush_partial(t);
                t.state = TabState::Waiting;
                t.devnode = None;
                t.conn = 0;
                cmds.push(Command::Close { key: t.key.clone() });
            }
            // Upload pauses end when the holder releases, not on unplug.
            TabState::Paused(_) => t.devnode = None,
            _ => {
                t.devnode = None;
                t.state = TabState::Waiting;
            }
        }
    }

    /// Whether a tab with a different key already holds `devnode`.
    fn owned_by_other(&self, key: &BoardKey, devnode: &PathBuf) -> bool {
        self.tabs
            .iter()
            .any(|t| &t.key != key && t.devnode.as_ref() == Some(devnode))
    }

    fn foreign_open(
        &mut self,
        devnode: PathBuf,
        holder: Holder,
        now: Now,
        cmds: &mut Vec<Command>,
    ) {
        self.pause.foreign_open(&devnode, holder.clone(), now.mono);
        let Some(t) = self
            .tabs
            .iter_mut()
            .find(|t| t.devnode.as_ref() == Some(&devnode))
        else {
            return;
        };
        match t.state {
            TabState::Paused(PauseReason::Manual) => return,
            TabState::Connecting | TabState::Connected => {
                Self::flush_partial(t);
                t.conn = 0;
                cmds.push(Command::Close { key: t.key.clone() });
                t.scroll.push(separator(now, "upload"));
                t.after_upload = true;
            }
            _ => {}
        }
        t.state = TabState::Paused(PauseReason::Upload(holder));
        t.upload_node = Some(devnode);
        t.retry_at = None;
        t.await_unblock = false;
    }

    fn foreign_released(&mut self, devnode: PathBuf, now: Now, cmds: &mut Vec<Command>) {
        self.pause.foreign_released(&devnode, now.mono);
        if self.pause.is_held(&devnode) {
            return;
        }
        for idx in 0..self.tabs.len() {
            let t = &mut self.tabs[idx];
            if !matches!(t.state, TabState::Paused(PauseReason::Upload(_)))
                || t.upload_node.as_ref() != Some(&devnode)
            {
                continue;
            }
            t.upload_node = None;
            let instant = matches!(t.family, Family::UartBridge | Family::EspUsbJtag);
            if instant && t.devnode.as_ref() == Some(&devnode) {
                // Same node still present: reopen at once to catch the boot log.
                self.open_tab(idx, None, cmds);
            } else {
                // NativeCdc/Unknown: an immediate reopen (DTR high, new line
                // coding) would cancel a 1200-baud bootloader touch, so these
                // wait for the quiet period and `on_timer` (devnode kept).
                // Also: node vanished (re-enumeration, failed upload, unplug).
                t.state = TabState::Waiting;
                t.await_unblock = true;
            }
        }
    }

    fn port_data(&mut self, i: usize, bytes: &[u8], now: Now) {
        let t = &mut self.tabs[i];
        if !matches!(t.state, TabState::Connecting | TabState::Connected) {
            return;
        }
        t.rx_bytes += bytes.len() as u64;
        let pushed = t.asm.push(bytes, now.wall);
        let had_data = t.data_this_conn;
        for l in pushed.complete {
            if is_rom_banner(&l) {
                if had_data && t.banner_quiet == 0 {
                    t.scroll.push(separator(now, "reset"));
                }
                t.banner_quiet = BANNER_QUIET;
            } else {
                t.banner_quiet = t.banner_quiet.saturating_sub(1);
            }
            t.scroll.push(l);
        }
        if !bytes.is_empty() {
            t.data_this_conn = true;
            // A connection that delivers data counts as healthy.
            t.backoff = RETRY_FIRST;
        }
    }

    fn send(&mut self, s: String, now: Now, cmds: &mut Vec<Command>) {
        let Some(t) = self.tabs.get_mut(self.focus) else {
            return;
        };
        if t.state != TabState::Connected {
            t.scroll
                .push(line(now, LineKind::Info, "not connected".into()));
            return;
        }
        let mut bytes = s.into_bytes();
        match t.line_ending {
            LineEnding::Lf => bytes.push(b'\n'),
            LineEnding::CrLf => bytes.extend_from_slice(b"\r\n"),
            LineEnding::None => {}
        }
        cmds.push(Command::Write {
            key: t.key.clone(),
            bytes,
        });
    }

    fn persist(&mut self, cmds: &mut Vec<Command>) {
        let Some(t) = self.tabs.get(self.focus) else {
            return;
        };
        let prefs = BoardPrefs {
            baud: t.baud,
            line_ending: t.line_ending,
        };
        self.prefs.insert(t.key.clone(), prefs.clone());
        cmds.push(Command::Persist {
            key: t.key.clone(),
            prefs,
        });
    }

    fn toggle_pause(&mut self, cmds: &mut Vec<Command>) {
        let idx = self.focus;
        let Some(t) = self.tabs.get_mut(idx) else {
            return;
        };
        match t.state {
            TabState::Connecting | TabState::Connected => {
                Self::flush_partial(t);
                t.state = TabState::Paused(PauseReason::Manual);
                t.conn = 0;
                cmds.push(Command::Close { key: t.key.clone() });
            }
            _ => {
                if t.devnode.is_some() {
                    self.open_tab(idx, None, cmds);
                }
            }
        }
    }

    fn close_tab(&mut self, cmds: &mut Vec<Command>) {
        let idx = self.focus;
        let Some(t) = self.tabs.get(idx) else {
            return;
        };
        if t.devnode.is_some() {
            cmds.push(Command::Close { key: t.key.clone() });
            self.suppressed.insert(t.key.clone());
        }
        self.tabs.remove(idx);
        self.focus = self.focus.min(self.tabs.len().saturating_sub(1));
    }

    fn connect_other(&mut self, path: PathBuf, now: Now, cmds: &mut Vec<Command>) {
        let Some(dev) = self.other.iter().find(|d| d.devnode == path).cloned() else {
            return;
        };
        let key = board_key(&dev);
        let idx = self.find_or_create(key, friendly_name(&dev), Family::Unknown, now);
        self.set_focus(idx);
        self.open_tab(idx, Some(path), cmds);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn dev(vid: u16, pid: u16, serial: &str, node: &str) -> DeviceInfo {
        DeviceInfo {
            devnode: node.into(),
            vid,
            pid,
            serial: Some(serial.into()),
            id_path: None,
            product: None,
            gid: 0,
        }
    }

    /// Fixed monotonic base so `now(a)` and `now(b)` differ by exactly `b - a` ms.
    fn base() -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now)
    }

    fn now(ms: u64) -> Now {
        Now {
            mono: base() + Duration::from_millis(ms),
            wall: chrono::Local::now(),
        }
    }

    fn core() -> Core {
        Core::new(HashMap::new(), 115_200, vec![])
    }

    fn opens(cmds: &[Command]) -> Vec<(ConnId, PathBuf)> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::Open { conn, devnode, .. } => Some((*conn, devnode.clone())),
                _ => None,
            })
            .collect()
    }

    /// Add a device and bring it to Connected; returns (key, conn).
    fn connect(c: &mut Core, d: DeviceInfo) -> (BoardKey, ConnId) {
        let key = board_key(&d);
        let cmds = c.handle(Event::DeviceAdded(d), now(0));
        let conn = opens(&cmds)[0].0;
        c.handle(
            Event::PortOpened {
                key: key.clone(),
                conn,
            },
            now(1),
        );
        (key, conn)
    }

    fn texts(t: &Tab) -> Vec<String> {
        (0..t.scroll.len())
            .map(|i| t.scroll.get(i).unwrap().text.clone())
            .collect()
    }

    #[test]
    fn auto_device_opens_with_policy() {
        let mut c = core();
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0")),
            now(0),
        );
        let open = cmds
            .iter()
            .find(|c| matches!(c, Command::Open { .. }))
            .unwrap();
        match open {
            Command::Open {
                baud,
                policy,
                devnode,
                conn,
                ..
            } => {
                assert_eq!(*baud, 115_200);
                assert_eq!(
                    *policy,
                    LinePolicy {
                        dtr: false,
                        rts: false
                    }
                );
                assert_eq!(devnode, &PathBuf::from("/dev/ttyUSB0"));
                assert_eq!(*conn, 1);
            }
            _ => unreachable!(),
        }
        assert_eq!(c.tabs()[0].state, TabState::Connecting);
        assert!(cmds.contains(&Command::Watch(vec!["/dev/ttyUSB0".into()])));
    }

    #[test]
    fn phone_never_opened() {
        let mut c = core();
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x04e8, 0x6860, "P", "/dev/ttyACM0")),
            now(0),
        );
        assert!(opens(&cmds).is_empty());
        assert!(c.tabs().is_empty());
        assert_eq!(c.other_ports().len(), 1);
    }

    #[test]
    fn bootloader_never_opened() {
        let mut c = core();
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x2341, 0x0036, "B", "/dev/ttyACM0")),
            now(0),
        );
        assert!(opens(&cmds).is_empty());
        assert!(c.tabs().is_empty());
        assert!(cmds.contains(&Command::Watch(vec!["/dev/ttyACM0".into()])));
    }

    #[test]
    fn replug_same_board_reuses_tab_new_path() {
        let mut c = core();
        connect(&mut c, dev(0x303a, 0x1001, "S", "/dev/ttyACM0"));
        let cmds = c.handle(
            Event::DeviceRemoved {
                devnode: "/dev/ttyACM0".into(),
            },
            now(2),
        );
        assert!(cmds.iter().any(|c| matches!(c, Command::Close { .. })));
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
        assert_eq!(c.tabs()[0].devnode, None);
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x303a, 0x1001, "S", "/dev/ttyACM1")),
            now(3),
        );
        assert_eq!(c.tabs().len(), 1);
        let o = opens(&cmds);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].1, PathBuf::from("/dev/ttyACM1"));
        assert_eq!(o[0].0, 2);
        let key = c.tabs()[0].key.clone();
        c.handle(Event::PortOpened { key, conn: 2 }, now(4));
        let t = &c.tabs()[0];
        assert!(
            t.scroll
                .get(t.scroll.len() - 1)
                .unwrap()
                .text
                .contains("reconnected")
        );
    }

    #[test]
    fn new_board_does_not_steal_focus_when_connected() {
        let mut c = core();
        connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        c.handle(
            Event::DeviceAdded(dev(0x1a86, 0x7523, "B", "/dev/ttyUSB1")),
            now(2),
        );
        assert_eq!(c.tabs().len(), 2);
        assert_eq!(c.focus(), 0);
        assert!(c.tabs()[1].highlight);
        c.action(Action::Focus(1), now(3));
        assert!(!c.tabs()[1].highlight);
    }

    #[test]
    fn new_board_takes_focus_when_current_waiting() {
        let mut c = core();
        c.handle(
            Event::DeviceAdded(dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0")),
            now(0),
        );
        c.handle(
            Event::DeviceAdded(dev(0x1a86, 0x7523, "B", "/dev/ttyUSB1")),
            now(1),
        );
        assert_eq!(c.focus(), 1);
        assert!(!c.tabs()[1].highlight);
    }

    #[test]
    fn stale_conn_events_ignored() {
        let mut c = core();
        let d = dev(0x303a, 0x1001, "S", "/dev/ttyACM0");
        let (key, old) = connect(&mut c, d.clone());
        c.handle(
            Event::DeviceRemoved {
                devnode: "/dev/ttyACM0".into(),
            },
            now(2),
        );
        c.handle(Event::DeviceAdded(d), now(3));
        c.handle(
            Event::PortData {
                key: key.clone(),
                conn: old,
                bytes: b"stale\n".to_vec(),
            },
            now(4),
        );
        c.handle(
            Event::PortGone {
                key: key.clone(),
                conn: old,
                reason: "x".into(),
            },
            now(5),
        );
        assert_eq!(c.tabs()[0].scroll.len(), 0);
        assert_eq!(c.tabs()[0].rx_bytes, 0);
        assert_eq!(c.tabs()[0].state, TabState::Connecting);
    }

    #[test]
    fn permission_denied_message() {
        let mut c = core();
        let d = dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0");
        let key = board_key(&d);
        c.handle(Event::DeviceAdded(d), now(0));
        c.handle(
            Event::PortOpenFailed {
                key,
                conn: 1,
                err: OpenError::PermissionDenied {
                    group: "uucp".into(),
                },
            },
            now(1),
        );
        let msg = "no permission for /dev/ttyUSB0 — add yourself to group 'uucp' and re-login";
        assert_eq!(c.tabs()[0].state, TabState::Error(msg.into()));
        let t = &c.tabs()[0];
        let l = t.scroll.get(0).unwrap();
        assert_eq!(l.text, msg);
        assert_eq!(l.kind, LineKind::Error);
    }

    #[test]
    fn rom_banner_mid_stream_inserts_reset_separator() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        let mut feed = |b: &[u8]| {
            c.handle(
                Event::PortData {
                    key: key.clone(),
                    conn,
                    bytes: b.to_vec(),
                },
                now(5),
            );
        };
        feed(b"ESP-ROM:esp32s3-20210327\r\nBuild:Mar 27 2021\r\nrst:0x1 (POWERON),boot:0x8 (SPI_FAST_FLASH_BOOT)\r\n");
        for i in 0..10 {
            feed(format!("log {i}\r\n").as_bytes());
        }
        feed(b"ESP-ROM:esp32s3-20210327\r\nBuild:Mar 27 2021\r\nrst:0x1 (POWERON),boot:0x8\r\n");
        let v = texts(&c.tabs()[0]);
        assert_eq!(v.iter().filter(|l| l.contains("reset")).count(), 1);
        assert!(v[13].starts_with("──── reset · "), "{v:?}");
        assert_eq!(v[14], "ESP-ROM:esp32s3-20210327");
    }

    #[test]
    fn send_appends_line_ending() {
        let mut c = core();
        let (key, _) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        let cmds = c.action(Action::Send("hi".into()), now(2));
        assert_eq!(
            cmds,
            vec![Command::Write {
                key: key.clone(),
                bytes: b"hi\n".to_vec()
            }]
        );
        c.action(Action::CycleLineEnding, now(3));
        let cmds = c.action(Action::Send("hi".into()), now(4));
        assert_eq!(
            cmds,
            vec![Command::Write {
                key,
                bytes: b"hi\r\n".to_vec()
            }]
        );
    }

    #[test]
    fn send_when_not_connected_notes_it() {
        let mut c = core();
        c.handle(
            Event::DeviceAdded(dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0")),
            now(0),
        );
        let cmds = c.action(Action::Send("hi".into()), now(1));
        assert!(cmds.is_empty());
        let t = &c.tabs()[0];
        assert_eq!(t.scroll.get(0).unwrap().text, "not connected");
        assert_eq!(t.scroll.get(0).unwrap().kind, LineKind::Info);
    }

    #[test]
    fn prefs_baud_used_on_open() {
        let d = dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0");
        let mut prefs = HashMap::new();
        prefs.insert(
            board_key(&d),
            BoardPrefs {
                baud: 9600,
                line_ending: LineEnding::CrLf,
            },
        );
        let mut c = Core::new(prefs, 115_200, vec![]);
        let cmds = c.handle(Event::DeviceAdded(d), now(0));
        assert!(matches!(&cmds[0], Command::Open { baud: 9600, .. }));
        assert_eq!(c.tabs()[0].line_ending, LineEnding::CrLf);
    }

    #[test]
    fn manual_port_opened_at_start() {
        let mut c = Core::new(HashMap::new(), 115_200, vec!["/dev/ttyS3".into()]);
        let cmds = c.startup(now(0));
        let t = &c.tabs()[0];
        assert_eq!(t.key, BoardKey("manual:/dev/ttyS3".into()));
        assert_eq!(t.name, "ttyS3");
        assert_eq!(t.family, Family::Unknown);
        assert!(matches!(
            &cmds[0],
            Command::Open {
                policy: LinePolicy {
                    dtr: true,
                    rts: false
                },
                ..
            }
        ));
        assert!(cmds.contains(&Command::Watch(vec!["/dev/ttyS3".into()])));
    }

    #[test]
    fn set_baud_persists_and_updates_prefs() {
        let mut c = core();
        let (key, _) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        let cmds = c.action(Action::SetBaud(9600), now(2));
        assert_eq!(cmds.len(), 2);
        assert_eq!(
            cmds[0],
            Command::SetBaud {
                key: key.clone(),
                baud: 9600
            }
        );
        assert!(matches!(&cmds[1], Command::Persist { prefs, .. } if prefs.baud == 9600));
    }

    #[test]
    fn close_connected_tab_suppresses_until_unplug() {
        let mut c = core();
        let d = dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0");
        let (key, _) = connect(&mut c, d.clone());
        let cmds = c.action(Action::CloseTab, now(2));
        assert!(cmds.contains(&Command::Close { key }));
        assert!(c.tabs().is_empty());
        let cmds = c.handle(Event::DeviceAdded(d.clone()), now(3));
        assert!(opens(&cmds).is_empty());
        assert!(c.tabs().is_empty());
        c.handle(
            Event::DeviceRemoved {
                devnode: d.devnode.clone(),
            },
            now(4),
        );
        let cmds = c.handle(Event::DeviceAdded(d), now(5));
        assert_eq!(opens(&cmds).len(), 1);
        assert_eq!(c.tabs().len(), 1);
    }

    #[test]
    fn close_unplugged_tab_removes_it() {
        let mut c = core();
        let d = dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0");
        connect(&mut c, d.clone());
        c.handle(Event::DeviceRemoved { devnode: d.devnode }, now(2));
        let cmds = c.action(Action::CloseTab, now(3));
        assert!(cmds.iter().all(|c| !matches!(c, Command::Close { .. })));
        assert!(c.tabs().is_empty());
        assert_eq!(c.focus(), 0);
    }

    #[test]
    fn toggle_pause_closes_then_reopens_with_new_conn() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        let cmds = c.action(Action::TogglePause, now(2));
        assert_eq!(cmds, vec![Command::Close { key }]);
        assert_eq!(c.tabs()[0].state, TabState::Paused(PauseReason::Manual));
        let cmds = c.action(Action::TogglePause, now(3));
        let o = opens(&cmds);
        assert_eq!(o.len(), 1);
        assert!(o[0].0 > conn);
    }

    #[test]
    fn reset_pulses_and_adds_separator() {
        let mut c = core();
        let (key, _) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        let cmds = c.action(Action::Reset, now(2));
        assert_eq!(
            cmds,
            vec![Command::ResetPulse {
                key,
                family: Family::UartBridge
            }]
        );
        assert!(texts(&c.tabs()[0])[0].starts_with("──── reset · "));
    }

    #[test]
    fn reset_on_native_usb_is_not_supported() {
        let mut c = core();
        connect(&mut c, dev(0x2341, 0x8036, "L", "/dev/ttyACM0"));
        let cmds = c.action(Action::Reset, now(2));
        assert!(cmds.is_empty(), "{cmds:?}");
        let v = texts(&c.tabs()[0]);
        assert_eq!(
            v,
            vec!["reset not supported on native USB boards".to_string()]
        );
        assert_eq!(c.tabs()[0].scroll.get(0).unwrap().kind, LineKind::Info);
    }

    #[test]
    fn connect_other_opens_unknown_tab() {
        let mut c = core();
        c.handle(
            Event::DeviceAdded(dev(0x04e8, 0x6860, "P", "/dev/ttyACM0")),
            now(0),
        );
        let cmds = c.action(Action::ConnectOther("/dev/ttyACM0".into()), now(1));
        assert_eq!(c.tabs()[0].family, Family::Unknown);
        assert!(matches!(
            &cmds[0],
            Command::Open {
                policy: LinePolicy {
                    dtr: true,
                    rts: false
                },
                ..
            }
        ));
        assert!(cmds.contains(&Command::Watch(vec!["/dev/ttyACM0".into()])));
    }

    #[test]
    fn watch_emitted_only_on_change() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        let cmds = c.handle(
            Event::PortData {
                key,
                conn,
                bytes: b"x\n".to_vec(),
            },
            now(2),
        );
        assert!(cmds.is_empty());
    }

    const S3: &[u8] = b"ESP-ROM:esp32s3-20210327\r\nBuild:Mar 27 2021\r\nrst:0x1 (POWERON),boot:0x8 (SPI_FAST_FLASH_BOOT)\r\n";

    fn data(c: &mut Core, key: &BoardKey, conn: ConnId, b: &[u8]) {
        c.handle(
            Event::PortData {
                key: key.clone(),
                conn,
                bytes: b.to_vec(),
            },
            now(5),
        );
    }

    fn seps(c: &Core, word: &str) -> usize {
        texts(&c.tabs()[0])
            .iter()
            .filter(|l| l.starts_with("────") && l.contains(word))
            .count()
    }

    #[test]
    fn late_port_opened_after_pause_ignored() {
        let mut c = core();
        let d = dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0");
        let key = board_key(&d);
        let cmds = c.handle(Event::DeviceAdded(d), now(0));
        let conn = opens(&cmds)[0].0;
        c.action(Action::TogglePause, now(1));
        let cmds = c.handle(Event::PortOpened { key, conn }, now(2));
        assert!(cmds.is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Paused(PauseReason::Manual));
    }

    #[test]
    fn late_open_failed_after_pause_ignored() {
        let mut c = core();
        let d = dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0");
        let key = board_key(&d);
        let cmds = c.handle(Event::DeviceAdded(d), now(0));
        let conn = opens(&cmds)[0].0;
        c.action(Action::TogglePause, now(1));
        let cmds = c.handle(
            Event::PortOpenFailed {
                key,
                conn,
                err: OpenError::Other("boom".into()),
            },
            now(2),
        );
        assert!(cmds.is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Paused(PauseReason::Manual));
        assert_eq!(c.tabs()[0].scroll.len(), 0);
    }

    #[test]
    fn late_port_opened_after_unplug_ignored() {
        let mut c = core();
        let d = dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0");
        let key = board_key(&d);
        let cmds = c.handle(Event::DeviceAdded(d.clone()), now(0));
        let conn = opens(&cmds)[0].0;
        c.handle(Event::DeviceRemoved { devnode: d.devnode }, now(1));
        c.handle(Event::PortOpened { key, conn }, now(2));
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
        assert_eq!(c.tabs()[0].devnode, None);
    }

    #[test]
    fn s3_banner_split_across_pushes_one_separator() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x303a, 0x1001, "S", "/dev/ttyACM0"));
        data(&mut c, &key, conn, b"hello\r\n");
        data(&mut c, &key, conn, &S3[..30]);
        data(&mut c, &key, conn, &S3[30..60]);
        data(&mut c, &key, conn, &S3[60..]);
        assert_eq!(seps(&c, "reset"), 1);
    }

    #[test]
    fn banner_after_action_reset_only_reset_separator() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x303a, 0x1001, "S", "/dev/ttyACM0"));
        data(&mut c, &key, conn, b"hello\r\nprompt> ");
        c.action(Action::Reset, now(2));
        data(&mut c, &key, conn, &S3[..40]);
        data(&mut c, &key, conn, &S3[40..]);
        assert_eq!(seps(&c, "reset"), 1);
        let v = texts(&c.tabs()[0]);
        assert_eq!(v[1], "prompt> ");
        assert!(v[2].starts_with("──── reset"));
        assert_eq!(v[3], "ESP-ROM:esp32s3-20210327");
    }

    #[test]
    fn banner_after_reconnect_only_reconnected_separator() {
        let mut c = core();
        let d = dev(0x303a, 0x1001, "S", "/dev/ttyACM0");
        let (key, conn) = connect(&mut c, d.clone());
        data(&mut c, &key, conn, b"hello\r\n");
        c.handle(
            Event::DeviceRemoved {
                devnode: d.devnode.clone(),
            },
            now(2),
        );
        let cmds = c.handle(Event::DeviceAdded(d), now(3));
        let conn = opens(&cmds)[0].0;
        c.handle(
            Event::PortOpened {
                key: key.clone(),
                conn,
            },
            now(4),
        );
        data(&mut c, &key, conn, &S3[..50]);
        data(&mut c, &key, conn, &S3[50..]);
        assert_eq!(seps(&c, "reconnected"), 1);
        assert_eq!(seps(&c, "reset"), 0);
    }

    #[test]
    fn port_flag_on_auto_board_makes_one_tab() {
        let mut c = Core::new(HashMap::new(), 115_200, vec!["/dev/ttyUSB0".into()]);
        let cmds = c.startup(now(0));
        assert_eq!(opens(&cmds).len(), 1);
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0")),
            now(1),
        );
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs().len(), 1);
    }

    #[test]
    fn replug_of_manually_paused_tab_does_not_reopen() {
        let mut c = core();
        let d = dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0");
        connect(&mut c, d.clone());
        c.action(Action::TogglePause, now(2));
        c.handle(
            Event::DeviceRemoved {
                devnode: d.devnode.clone(),
            },
            now(3),
        );
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x10c4, 0xea60, "A", "/dev/ttyUSB1")),
            now(4),
        );
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Paused(PauseReason::Manual));
        assert_eq!(c.tabs()[0].devnode, Some("/dev/ttyUSB1".into()));
    }

    #[test]
    fn duplicate_device_added_does_not_reopen() {
        let mut c = core();
        let d = dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0");
        c.handle(Event::DeviceAdded(d.clone()), now(0));
        let cmds = c.handle(Event::DeviceAdded(d.clone()), now(1));
        assert!(opens(&cmds).is_empty());
        let key = board_key(&d);
        c.handle(Event::PortOpened { key, conn: 1 }, now(2));
        let cmds = c.handle(Event::DeviceAdded(d), now(3));
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Connected);
    }

    // ---- upload pause (Task 5) ----

    fn esptool() -> Holder {
        Holder::Named {
            pid: 4121,
            comm: "esptool".into(),
        }
    }

    fn foreign_open(c: &mut Core, node: &str, holder: Holder, ms: u64) -> Vec<Command> {
        c.handle(
            Event::ForeignOpen {
                devnode: node.into(),
                holder,
            },
            now(ms),
        )
    }

    fn foreign_released(c: &mut Core, node: &str, ms: u64) -> Vec<Command> {
        c.handle(
            Event::ForeignReleased {
                devnode: node.into(),
            },
            now(ms),
        )
    }

    fn removed(c: &mut Core, node: &str, ms: u64) -> Vec<Command> {
        c.handle(
            Event::DeviceRemoved {
                devnode: node.into(),
            },
            now(ms),
        )
    }

    #[test]
    fn same_node_reopens_immediately() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        let cmds = foreign_open(&mut c, "/dev/ttyUSB0", esptool(), 2);
        assert!(cmds.contains(&Command::Close { key: key.clone() }));
        assert!(opens(&cmds).is_empty());
        assert_eq!(
            c.tabs()[0].state,
            TabState::Paused(PauseReason::Upload(esptool()))
        );
        assert_eq!(c.tabs()[0].conn, 0);
        let v = texts(&c.tabs()[0]);
        assert!(v.last().unwrap().starts_with("──── upload · "), "{v:?}");
        assert!(v.last().unwrap().ends_with(" ────"));
        let cmds = foreign_released(&mut c, "/dev/ttyUSB0", 7);
        let o = opens(&cmds);
        assert_eq!(o.len(), 1);
        assert!(o[0].0 > conn);
        assert_eq!(o[0].1, PathBuf::from("/dev/ttyUSB0"));
        assert_eq!(c.tabs()[0].state, TabState::Connecting);
        // The new conn is live; the old one stays stale.
        c.handle(
            Event::PortOpened {
                key: key.clone(),
                conn: o[0].0,
            },
            now(8),
        );
        assert_eq!(c.tabs()[0].state, TabState::Connected);
    }

    #[test]
    fn repeated_foreign_open_one_separator_one_close() {
        let mut c = core();
        let (key, _) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        foreign_open(&mut c, "/dev/ttyUSB0", esptool(), 2);
        let cmds = foreign_open(&mut c, "/dev/ttyUSB0", esptool(), 3);
        assert!(!cmds.contains(&Command::Close { key }));
        assert_eq!(seps(&c, "upload"), 1);
    }

    #[test]
    fn foreign_open_while_connecting_closes_and_invalidates_conn() {
        let mut c = core();
        let d = dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0");
        let key = board_key(&d);
        let cmds = c.handle(Event::DeviceAdded(d), now(0));
        let conn = opens(&cmds)[0].0;
        let cmds = foreign_open(&mut c, "/dev/ttyUSB0", esptool(), 1);
        assert!(cmds.contains(&Command::Close { key: key.clone() }));
        c.handle(Event::PortOpened { key, conn }, now(2));
        assert_eq!(
            c.tabs()[0].state,
            TabState::Paused(PauseReason::Upload(esptool()))
        );
    }

    #[test]
    fn reenumeration_waits_quiet_period() {
        let mut c = core();
        connect(&mut c, dev(0x2341, 0x8036, "L", "/dev/ttyACM0"));
        foreign_open(&mut c, "/dev/ttyACM0", esptool(), 10);
        let cmds = removed(&mut c, "/dev/ttyACM0", 20);
        assert!(opens(&cmds).is_empty());
        assert_eq!(
            c.tabs()[0].state,
            TabState::Paused(PauseReason::Upload(esptool()))
        );
        assert_eq!(c.tabs()[0].devnode, None);
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x2341, 0x0036, "L", "/dev/ttyACM1")),
            now(30),
        );
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs().len(), 1);
        removed(&mut c, "/dev/ttyACM1", 900);
        let cmds = foreign_released(&mut c, "/dev/ttyACM0", 1000);
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x2341, 0x8036, "L", "/dev/ttyACM0")),
            now(1100),
        );
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
        assert_eq!(c.tabs()[0].devnode, Some("/dev/ttyACM0".into()));
        assert_eq!(c.next_deadline(), Some(now(1500).mono));
        let cmds = c.on_timer(now(1499));
        assert!(opens(&cmds).is_empty());
        let cmds = c.on_timer(now(1500));
        let o = opens(&cmds);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].1, PathBuf::from("/dev/ttyACM0"));
        assert_eq!(c.tabs()[0].state, TabState::Connecting);
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn failed_upload_unblocks() {
        let mut c = core();
        connect(&mut c, dev(0x2341, 0x8036, "L", "/dev/ttyACM0"));
        foreign_open(&mut c, "/dev/ttyACM0", esptool(), 10);
        removed(&mut c, "/dev/ttyACM0", 20);
        foreign_released(&mut c, "/dev/ttyACM0", 1000);
        let cmds = c.on_timer(now(1500));
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
        assert!(c.pause.allows_auto_connect(now(1500).mono));
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn stuck_holder_safety_net() {
        let mut c = core();
        connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        foreign_open(&mut c, "/dev/ttyUSB0", esptool(), 0);
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x1a86, 0x7523, "B", "/dev/ttyUSB1")),
            now(1000),
        );
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[1].state, TabState::Waiting);
        assert_eq!(c.next_deadline(), Some(now(60_000).mono));
        let cmds = c.on_timer(now(60_000));
        let o = opens(&cmds);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].1, PathBuf::from("/dev/ttyUSB1"));
        assert_eq!(
            c.tabs()[0].state,
            TabState::Paused(PauseReason::Upload(esptool()))
        );
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn unknown_holder_label() {
        let mut c = core();
        connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        foreign_open(&mut c, "/dev/ttyUSB0", Holder::Unknown, 2);
        assert_eq!(
            c.tabs()[0].state,
            TabState::Paused(PauseReason::Upload(Holder::Unknown))
        );
    }

    #[test]
    fn manual_pause_not_auto_resumed() {
        let mut c = core();
        connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        c.action(Action::TogglePause, now(2));
        let cmds = foreign_open(&mut c, "/dev/ttyUSB0", esptool(), 3);
        assert!(cmds.is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Paused(PauseReason::Manual));
        assert_eq!(seps(&c, "upload"), 0);
        let cmds = foreign_released(&mut c, "/dev/ttyUSB0", 10);
        assert!(opens(&cmds).is_empty());
        let cmds = c.on_timer(now(510));
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Paused(PauseReason::Manual));
    }

    #[test]
    fn manual_port_resumes_after_upload_quiet_period() {
        let mut c = Core::new(HashMap::new(), 115_200, vec!["/tmp/cmrd".into()]);
        let conn = opens(&c.startup(now(0)))[0].0;
        let key = c.tabs()[0].key.clone();
        c.handle(Event::PortOpened { key, conn }, now(1));
        foreign_open(&mut c, "/tmp/cmrd", esptool(), 2);
        assert!(matches!(c.tabs()[0].state, TabState::Paused(_)));
        let cmds = foreign_released(&mut c, "/tmp/cmrd", 10);
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
        assert!(opens(&c.on_timer(now(100))).is_empty());
        assert_eq!(c.next_deadline(), Some(now(510).mono));
        let cmds = c.on_timer(now(510));
        assert_eq!(opens(&cmds).len(), 1);
        assert_eq!(opens(&cmds)[0].1, PathBuf::from("/tmp/cmrd"));
        assert_eq!(c.tabs()[0].state, TabState::Connecting);
        assert_eq!(seps(&c, "upload"), 1);
    }

    #[test]
    fn manual_port_not_resumed_while_held() {
        let mut c = Core::new(HashMap::new(), 115_200, vec!["/tmp/cmrd".into()]);
        let conn = opens(&c.startup(now(0)))[0].0;
        let key = c.tabs()[0].key.clone();
        c.handle(Event::PortOpened { key, conn }, now(1));
        foreign_open(&mut c, "/tmp/cmrd", esptool(), 2);
        // Safety net lifts the global block, but this node is still held.
        let cmds = c.on_timer(now(61_000));
        assert!(opens(&cmds).is_empty());
        assert!(matches!(c.tabs()[0].state, TabState::Paused(_)));
    }

    #[test]
    fn foreign_open_on_unowned_node_still_blocks() {
        let mut c = core();
        foreign_open(&mut c, "/dev/ttyACM5", Holder::Unknown, 0);
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0")),
            now(1),
        );
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
    }

    #[test]
    fn release_reopens_same_node_even_if_other_node_held() {
        let mut c = core();
        connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        foreign_open(&mut c, "/dev/ttyUSB0", esptool(), 2);
        foreign_open(&mut c, "/dev/ttyACM5", Holder::Unknown, 3);
        let cmds = foreign_released(&mut c, "/dev/ttyUSB0", 4);
        assert_eq!(opens(&cmds).len(), 1);
    }

    #[test]
    fn reappearing_while_holder_alive_waits_for_release() {
        let mut c = core();
        connect(&mut c, dev(0x2341, 0x8036, "L", "/dev/ttyACM0"));
        foreign_open(&mut c, "/dev/ttyACM0", esptool(), 10);
        removed(&mut c, "/dev/ttyACM0", 20);
        // Holder still recorded on the vanished node; board returns on a new node.
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x2341, 0x8036, "L", "/dev/ttyACM1")),
            now(30),
        );
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[0].devnode, Some("/dev/ttyACM1".into()));
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
        foreign_released(&mut c, "/dev/ttyACM0", 1000);
        let cmds = c.on_timer(now(1500));
        assert_eq!(opens(&cmds).len(), 1);
    }

    #[test]
    fn bootloader_devices_never_opened_during_or_after_pause() {
        let mut c = core();
        foreign_open(&mut c, "/dev/ttyACM9", Holder::Unknown, 0);
        // During the pause: Adafruit SAMD UF2 bootloader.
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x239a, 0x000b, "B1", "/dev/ttyACM0")),
            now(1),
        );
        assert!(opens(&cmds).is_empty());
        foreign_released(&mut c, "/dev/ttyACM9", 2);
        assert!(opens(&c.on_timer(now(502))).is_empty());
        // After the pause: Seeed and Arduino SAMD bootloaders.
        for (vid, pid, node) in [
            (0x2886, 0x002f, "/dev/ttyACM1"),
            (0x2341, 0x004d, "/dev/ttyACM2"),
        ] {
            let cmds = c.handle(Event::DeviceAdded(dev(vid, pid, "B2", node)), now(600));
            assert!(opens(&cmds).is_empty());
        }
        assert!(opens(&c.on_timer(now(2000))).is_empty());
        assert!(c.tabs().is_empty());
        assert_eq!(c.other_ports().len(), 3);
    }

    // ---- Task 5 review fixes ----

    #[test]
    fn native_cdc_1200_touch_not_cancelled() {
        let mut c = core();
        connect(&mut c, dev(0x2341, 0x8036, "L", "/dev/ttyACM0"));
        // 1200-baud touch: open + close while the node is still present.
        foreign_open(&mut c, "/dev/ttyACM0", esptool(), 10);
        let cmds = foreign_released(&mut c, "/dev/ttyACM0", 15);
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
        assert_eq!(c.tabs()[0].devnode, Some("/dev/ttyACM0".into()));
        // Board resets into the bootloader.
        assert!(opens(&removed(&mut c, "/dev/ttyACM0", 200)).is_empty());
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x2341, 0x0036, "L", "/dev/ttyACM1")),
            now(300),
        );
        assert!(opens(&cmds).is_empty());
        // avrdude flashes the bootloader node.
        assert!(opens(&foreign_open(&mut c, "/dev/ttyACM1", esptool(), 400)).is_empty());
        assert!(opens(&foreign_released(&mut c, "/dev/ttyACM1", 3000)).is_empty());
        assert!(opens(&removed(&mut c, "/dev/ttyACM1", 3100)).is_empty());
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x2341, 0x8036, "L", "/dev/ttyACM0")),
            now(3200),
        );
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.next_deadline(), Some(now(3500).mono));
        assert!(opens(&c.on_timer(now(3499))).is_empty());
        let o = opens(&c.on_timer(now(3500)));
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].1, PathBuf::from("/dev/ttyACM0"));
        assert_eq!(c.tabs().len(), 1);
    }

    #[test]
    fn native_cdc_release_reopens_after_quiet_if_node_stays() {
        let mut c = core();
        connect(&mut c, dev(0x2341, 0x8036, "L", "/dev/ttyACM0"));
        foreign_open(&mut c, "/dev/ttyACM0", esptool(), 10);
        foreign_released(&mut c, "/dev/ttyACM0", 20);
        let o = opens(&c.on_timer(now(520)));
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].1, PathBuf::from("/dev/ttyACM0"));
    }

    #[test]
    fn uart_bridge_same_flow_reopens_immediately() {
        let mut c = core();
        connect(&mut c, dev(0x1a86, 0x7523, "U", "/dev/ttyUSB0"));
        foreign_open(&mut c, "/dev/ttyUSB0", esptool(), 10);
        let o = opens(&foreign_released(&mut c, "/dev/ttyUSB0", 15));
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].1, PathBuf::from("/dev/ttyUSB0"));
    }

    #[test]
    fn esp_usb_jtag_reopens_immediately() {
        let mut c = core();
        connect(&mut c, dev(0x303a, 0x1001, "S", "/dev/ttyACM0"));
        foreign_open(&mut c, "/dev/ttyACM0", esptool(), 10);
        assert_eq!(
            opens(&foreign_released(&mut c, "/dev/ttyACM0", 15)).len(),
            1
        );
    }

    #[test]
    fn upload_flow_has_single_upload_separator() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        data(&mut c, &key, conn, b"app log\r\n");
        foreign_open(&mut c, "/dev/ttyUSB0", esptool(), 10);
        let conn = opens(&foreign_released(&mut c, "/dev/ttyUSB0", 15))[0].0;
        c.handle(
            Event::PortOpened {
                key: key.clone(),
                conn,
            },
            now(16),
        );
        data(&mut c, &key, conn, b"x\r\n");
        data(&mut c, &key, conn, S3);
        assert_eq!(seps(&c, "upload"), 1);
        assert_eq!(seps(&c, "reconnected"), 0);
        assert_eq!(seps(&c, "reset"), 0);
        // A later ordinary reconnect still gets its separator.
        removed(&mut c, "/dev/ttyUSB0", 100);
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0")),
            now(1000),
        );
        let conn = opens(&cmds)[0].0;
        c.handle(Event::PortOpened { key, conn }, now(1001));
        assert_eq!(seps(&c, "reconnected"), 1);
    }

    #[test]
    fn held_node_never_opened_by_device_added_or_timer() {
        let mut c = core();
        // Safety net lifts the global block, but the held node stays untouched.
        foreign_open(&mut c, "/dev/ttyUSB0", Holder::Unknown, 0);
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0")),
            now(60_000),
        );
        assert!(opens(&cmds).is_empty());
        assert!(opens(&c.on_timer(now(60_001))).is_empty());
        assert_eq!(c.tabs()[0].devnode, Some("/dev/ttyUSB0".into()));
    }

    // ---- final fix wave ----

    #[test]
    fn connect_other_tab_reattaches_on_replug() {
        let mut c = core();
        let phone = dev(0x04e8, 0x6860, "P", "/dev/ttyACM0");
        c.handle(Event::DeviceAdded(phone.clone()), now(0));
        let o = opens(&c.action(Action::ConnectOther("/dev/ttyACM0".into()), now(1)));
        let key = c.tabs()[0].key.clone();
        c.handle(
            Event::PortOpened {
                key: key.clone(),
                conn: o[0].0,
            },
            now(2),
        );
        let cmds = removed(&mut c, "/dev/ttyACM0", 3);
        assert!(cmds.contains(&Command::Close { key: key.clone() }));
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x04e8, 0x6860, "P", "/dev/ttyACM1")),
            now(4),
        );
        let o = opens(&cmds);
        assert_eq!(o.len(), 1, "{cmds:?}");
        assert_eq!(o[0].1, PathBuf::from("/dev/ttyACM1"));
        assert_eq!(c.tabs().len(), 1);
        assert_eq!(c.tabs()[0].key, key);
        assert_eq!(c.tabs()[0].family, Family::Unknown);
        assert_eq!(c.tabs()[0].state, TabState::Connecting);
    }

    #[test]
    fn other_device_without_tab_is_not_opened() {
        let mut c = core();
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x04e8, 0x6860, "P", "/dev/ttyACM0")),
            now(0),
        );
        assert!(opens(&cmds).is_empty());
        assert!(c.tabs().is_empty());
    }

    fn cp2102(serial: &str, id_path: &str, node: &str) -> DeviceInfo {
        DeviceInfo {
            id_path: Some(id_path.into()),
            ..dev(0x10c4, 0xea60, serial, node)
        }
    }

    #[test]
    fn identical_serials_get_separate_tabs() {
        let mut c = core();
        let a = cp2102("0001", "pci-0000:00:14.0-usb-0:1:1.0", "/dev/ttyUSB0");
        let b = cp2102("0001", "pci-0000:00:14.0-usb-0:2:1.0", "/dev/ttyUSB1");
        let (ka, _) = connect(&mut c, a);
        let cmds = c.handle(Event::DeviceAdded(b.clone()), now(2));
        assert!(
            !cmds.iter().any(|c| matches!(c, Command::Close { .. })),
            "{cmds:?}"
        );
        let o = opens(&cmds);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].1, PathBuf::from("/dev/ttyUSB1"));
        assert_eq!(c.tabs().len(), 2);
        assert_eq!(c.tabs()[0].key, ka);
        assert_eq!(c.tabs()[0].state, TabState::Connected);
        assert_eq!(c.tabs()[0].devnode, Some("/dev/ttyUSB0".into()));
        assert_eq!(
            c.tabs()[1].key,
            BoardKey("10c4:ea60:0001@pci-0000:00:14.0-usb-0:2:1.0".into())
        );
        assert_eq!(c.tabs()[1].state, TabState::Connecting);
        // The second board's replug comes back to its own tab.
        removed(&mut c, "/dev/ttyUSB1", 3);
        let o = opens(&c.handle(
            Event::DeviceAdded(cp2102(
                "0001",
                "pci-0000:00:14.0-usb-0:2:1.0",
                "/dev/ttyUSB2",
            )),
            now(4),
        ));
        assert_eq!(o.len(), 1);
        assert_eq!(c.tabs().len(), 2);
        assert_eq!(c.tabs()[1].devnode, Some("/dev/ttyUSB2".into()));
        assert_eq!(c.tabs()[0].state, TabState::Connected);
    }

    fn gone(c: &mut Core, key: &BoardKey, conn: ConnId, ms: u64) -> Vec<Command> {
        c.handle(
            Event::PortGone {
                key: key.clone(),
                conn,
                reason: "read: EBADF".into(),
            },
            now(ms),
        )
    }

    /// The next retry: nothing before `at`, one Open at `at`.
    fn retry_at(c: &mut Core, at: u64) -> ConnId {
        assert_eq!(c.next_deadline(), Some(now(at).mono));
        assert!(opens(&c.on_timer(now(at - 1))).is_empty());
        let o = opens(&c.on_timer(now(at)));
        assert_eq!(o.len(), 1);
        o[0].0
    }

    #[test]
    fn port_gone_on_present_board_retries_with_backoff() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        let cmds = gone(&mut c, &key, conn, 10);
        assert!(cmds.contains(&Command::Close { key: key.clone() }));
        let t = &c.tabs()[0];
        assert_eq!(t.state, TabState::Waiting);
        assert_eq!(t.devnode, Some("/dev/ttyUSB0".into()));
        let last = t.scroll.get(t.scroll.len() - 1).unwrap();
        assert_eq!(last.text, "connection lost: read: EBADF");
        assert_eq!(last.kind, LineKind::Info);
        // 1 s, 2 s, 4 s, then capped at 5 s.
        let mut at = 10;
        for delay in [1000, 2000, 4000, 5000, 5000] {
            at += delay;
            let conn = retry_at(&mut c, at);
            assert_eq!(c.tabs()[0].state, TabState::Connecting);
            at += 5;
            gone(&mut c, &key, conn, at);
        }
        // A connection that delivers data resets the backoff.
        at += 5000;
        let conn = retry_at(&mut c, at);
        c.handle(
            Event::PortOpened {
                key: key.clone(),
                conn,
            },
            now(at + 1),
        );
        data(&mut c, &key, conn, b"hi\r\n");
        gone(&mut c, &key, conn, at + 10);
        retry_at(&mut c, at + 1010);
    }

    #[test]
    fn device_added_resets_backoff() {
        let mut c = core();
        let d = dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0");
        let (key, conn) = connect(&mut c, d.clone());
        gone(&mut c, &key, conn, 10);
        let conn = retry_at(&mut c, 1010);
        gone(&mut c, &key, conn, 1020);
        removed(&mut c, "/dev/ttyUSB0", 1030);
        assert_eq!(c.next_deadline(), None);
        let conn = opens(&c.handle(Event::DeviceAdded(d), now(1040)))[0].0;
        gone(&mut c, &key, conn, 1050);
        retry_at(&mut c, 2050);
    }

    #[test]
    fn no_retry_once_board_is_unplugged() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        gone(&mut c, &key, conn, 10);
        removed(&mut c, "/dev/ttyUSB0", 20);
        assert!(opens(&c.on_timer(now(1010))).is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn retry_waits_for_global_upload_block() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        gone(&mut c, &key, conn, 0);
        foreign_open(&mut c, "/dev/ttyACM5", esptool(), 500);
        assert!(opens(&c.on_timer(now(1000))).is_empty());
        foreign_released(&mut c, "/dev/ttyACM5", 3000);
        assert_eq!(c.next_deadline(), Some(now(3500).mono));
        assert_eq!(opens(&c.on_timer(now(3500))).len(), 1);
    }

    #[test]
    fn retry_skips_held_node() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        gone(&mut c, &key, conn, 0);
        // A holder appears on the node while the tab waits for its retry.
        foreign_open(&mut c, "/dev/ttyUSB0", esptool(), 500);
        assert!(opens(&c.on_timer(now(1000))).is_empty());
        assert!(opens(&c.on_timer(now(60_000))).is_empty());
    }

    #[test]
    fn manual_pause_is_not_retried() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        gone(&mut c, &key, conn, 0);
        c.action(Action::TogglePause, now(10)); // resume: reopens now
        c.action(Action::TogglePause, now(20)); // pause again
        assert!(opens(&c.on_timer(now(1000))).is_empty());
        assert_eq!(c.tabs()[0].state, TabState::Paused(PauseReason::Manual));
    }

    #[test]
    fn on_timer_does_not_reopen_unrelated_waiting_tab() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        gone(&mut c, &key, conn, 0);
        // Another node's upload ends with a quiet deadline before A's retry.
        foreign_open(&mut c, "/dev/ttyACM5", esptool(), 10);
        foreign_released(&mut c, "/dev/ttyACM5", 20);
        assert_eq!(c.next_deadline(), Some(now(520).mono));
        assert!(opens(&c.on_timer(now(520))).is_empty());
        assert_eq!(opens(&c.on_timer(now(1000))).len(), 1);
    }

    #[test]
    fn missing_manual_port_waits_and_retries() {
        let mut c = Core::new(HashMap::new(), 115_200, vec!["/tmp/cmrd-missing".into()]);
        let conn = opens(&c.startup(now(0)))[0].0;
        let key = c.tabs()[0].key.clone();
        c.handle(
            Event::PortOpenFailed {
                key: key.clone(),
                conn,
                err: OpenError::NotPresent,
            },
            now(1),
        );
        assert_eq!(c.tabs()[0].state, TabState::Waiting);
        assert_eq!(c.tabs()[0].devnode, Some("/tmp/cmrd-missing".into()));
        let o = opens(&c.on_timer(now(1001)));
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].1, PathBuf::from("/tmp/cmrd-missing"));
        // Still missing: the next retry backs off; one info line, not one per try.
        c.handle(
            Event::PortOpenFailed {
                key,
                conn: o[0].0,
                err: OpenError::NotPresent,
            },
            now(1002),
        );
        assert_eq!(c.next_deadline(), Some(now(3002).mono));
        assert_eq!(c.tabs()[0].scroll.len(), 1);
    }

    #[test]
    fn port_warning_is_an_info_line() {
        let mut c = core();
        let (key, conn) = connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        let cmds = c.handle(
            Event::PortWarning {
                key,
                conn,
                msg: "write timed out, 12 bytes dropped".into(),
            },
            now(5),
        );
        assert!(cmds.is_empty());
        let t = &c.tabs()[0];
        assert_eq!(t.state, TabState::Connected);
        let l = t.scroll.get(0).unwrap();
        assert_eq!(l.text, "write timed out, 12 bytes dropped");
        assert_eq!(l.kind, LineKind::Info);
    }

    #[test]
    fn blocked_device_added_keeps_connected_devnode() {
        let mut c = core();
        connect(&mut c, dev(0x10c4, 0xea60, "A", "/dev/ttyUSB0"));
        foreign_open(&mut c, "/dev/ttyACM5", Holder::Unknown, 2);
        let cmds = c.handle(
            Event::DeviceAdded(dev(0x10c4, 0xea60, "A", "/dev/ttyUSB1")),
            now(3),
        );
        assert!(opens(&cmds).is_empty());
        assert_eq!(c.tabs()[0].devnode, Some("/dev/ttyUSB0".into()));
        assert_eq!(c.tabs()[0].state, TabState::Connected);
    }
}
