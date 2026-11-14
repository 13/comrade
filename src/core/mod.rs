//! Pure state machine: events and actions in, commands out. No I/O.

mod pause;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use crate::board::{
    BoardKey, Class, DeviceInfo, Family, LinePolicy, board_key, classify, friendly_name,
    line_policy,
};
use crate::event::{Command, ConnId, Event, Holder, Now, OpenError};
use crate::lines::{Line, LineAssembler, LineKind};
use crate::scrollback::{Scrollback, strip_sgr};
use crate::store::{BoardPrefs, LineEnding};
use pause::PauseTracker;

const SCROLLBACK_CAP: usize = 100_000;

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
}

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

    pub fn other_ports(&self) -> Vec<&DeviceInfo> {
        self.other.iter().collect()
    }

    pub fn on_timer(&mut self, _now: Now) -> Vec<Command> {
        Vec::new()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.pause.deadline()
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
                    if t.ever_connected {
                        t.scroll.push(separator(now, "reconnected"));
                    }
                    t.ever_connected = true;
                }
            }
            Event::PortOpenFailed { key, conn, err } => {
                if let Some(i) = self.matching(&key, conn) {
                    let t = &mut self.tabs[i];
                    let msg = match err {
                        OpenError::PermissionDenied { group } => {
                            let dev = t
                                .devnode
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                            format!(
                                "no permission for {dev} — add yourself to group '{group}' and re-login"
                            )
                        }
                        OpenError::Other(m) => m,
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
            Event::PortGone { key, conn, .. } => {
                if let Some(i) = self.matching(&key, conn) {
                    let t = &mut self.tabs[i];
                    Self::flush_partial(t);
                    t.state = TabState::Waiting;
                    t.devnode = None;
                    cmds.push(Command::Close { key });
                }
            }
            Event::ForeignOpen { .. } | Event::ForeignReleased { .. } | Event::Input(_) => {}
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
                    if t.state == TabState::Connected {
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
        self.other.retain(|d| d.devnode != dev.devnode);
        match classify(dev.vid, dev.pid) {
            Class::Other | Class::Bootloader => self.other.push(dev),
            Class::Auto(family) => {
                let key = board_key(&dev);
                self.present_auto.push(dev.clone());
                if self.suppressed.contains(&key) {
                    return;
                }
                let idx = self.find_or_create(key, friendly_name(&dev), family, now);
                self.tabs[idx].devnode = Some(dev.devnode.clone());
                if self.pause.allows_auto_connect(now.mono) {
                    self.open_tab(idx, Some(dev.devnode), cmds);
                }
            }
        }
    }

    fn device_removed(&mut self, devnode: PathBuf, cmds: &mut Vec<Command>) {
        if let Some(d) = self.present_auto.iter().find(|d| d.devnode == devnode) {
            let key = board_key(d);
            self.suppressed.remove(&key);
        }
        self.present_auto.retain(|d| d.devnode != devnode);
        self.other.retain(|d| d.devnode != devnode);
        let Some(t) = self
            .tabs
            .iter_mut()
            .find(|t| t.devnode.as_ref() == Some(&devnode))
        else {
            return;
        };
        match t.state {
            TabState::Connecting | TabState::Connected => {
                Self::flush_partial(t);
                t.state = TabState::Waiting;
                t.devnode = None;
                cmds.push(Command::Close { key: t.key.clone() });
            }
            TabState::Paused(PauseReason::Manual) => t.devnode = None,
            _ => {
                t.devnode = None;
                t.state = TabState::Waiting;
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
        let mut sep_done = !(pushed.rom_banner && t.data_this_conn);
        for l in pushed.complete {
            if !sep_done && is_rom_banner(&l) {
                t.scroll.push(separator(now, "reset"));
                sep_done = true;
            }
            t.scroll.push(l);
        }
        if !bytes.is_empty() {
            t.data_this_conn = true;
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

    fn now(ms: u64) -> Now {
        Now {
            mono: Instant::now() + Duration::from_millis(ms),
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
        // Banner as the first data of a connection: no separator.
        feed(b"ESP-ROM:esp32s3\n");
        feed(b"hello\n");
        feed(b"ESP-ROM:esp32s3\n");
        let t = &c.tabs()[0];
        let v = texts(t);
        assert_eq!(v.len(), 4);
        assert_eq!(v[0], "ESP-ROM:esp32s3");
        assert!(v[2].starts_with("──── reset · "));
        assert_eq!(t.scroll.get(2).unwrap().kind, LineKind::Separator);
        assert_eq!(v[3], "ESP-ROM:esp32s3");
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
}
