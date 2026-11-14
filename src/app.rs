//! App loop: owns `Core` and `View`, runs `Command`s against the I/O workers,
//! and renders. `run` wires up the real threads and the terminal.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};

use crate::board::BoardKey;
use crate::core::Core;
use crate::event::{Command, ConnId, Event, Now};
use crate::io::opener_watch::OpenerWatch;
use crate::io::{PortHandle, spawn_port, udev_watch};
use crate::scrollback::strip_sgr;
use crate::store::{self, BoardPrefs};
use crate::ui::{self, UiOutcome, View};

const DEFAULT_BAUD: u32 = 115_200;
/// Minimum gap between frames (60 fps cap).
const FRAME: Duration = Duration::from_millis(16);
/// How often `rate_kbps` is recomputed.
const RATE_EVERY: Duration = Duration::from_secs(1);
/// Longest blocking wait, so signal flags are seen promptly.
const MAX_WAIT: Duration = Duration::from_millis(250);
/// Longest time spent draining queued events before a tick and a frame.
const DRAIN_BUDGET: Duration = Duration::from_millis(50);

/// Zero-config serial monitor for USB microcontrollers.
#[derive(clap::Parser, Debug)]
#[command(version, about)]
pub struct Args {
    /// Baud rate for boards without a remembered rate (default 115200).
    #[arg(long)]
    pub baud: Option<u32>,
    /// Open this port at startup, even if it is not a known board (repeatable).
    #[arg(long)]
    pub port: Vec<PathBuf>,
}

/// A running port worker and what it was opened with.
struct Port {
    handle: PortHandle,
    devnode: PathBuf,
    conn: ConnId,
}

fn now() -> Now {
    Now {
        mono: Instant::now(),
        wall: chrono::Local::now(),
    }
}

pub struct App {
    core: Core,
    view: View,
    opener: Option<OpenerWatch>,
    tx: Sender<Event>,
    state_path: Option<PathBuf>,
    prefs: HashMap<BoardKey, BoardPrefs>,
    ports: HashMap<BoardKey, Port>,
    /// Directory `s` saves into (the working directory in the real app).
    save_dir: PathBuf,
    dirty: bool,
    quit: bool,
    last_frame: Option<Instant>,
    next_rate: Instant,
    last_rate_at: Option<Instant>,
    last_rx: HashMap<BoardKey, u64>,
}

impl App {
    /// Builds the app and opens the `--port` ports (`Core::startup`).
    pub fn new(
        core: Core,
        opener: Option<OpenerWatch>,
        tx: Sender<Event>,
        state_path: Option<PathBuf>,
    ) -> Self {
        let prefs = core.prefs().clone();
        let t = Instant::now();
        let mut app = Self {
            core,
            view: View::new(),
            opener,
            tx,
            state_path,
            prefs,
            ports: HashMap::new(),
            save_dir: PathBuf::from("."),
            dirty: true,
            quit: false,
            last_frame: None,
            next_rate: t,
            last_rate_at: None,
            last_rx: HashMap::new(),
        };
        let cmds = app.core.startup(now());
        app.exec(cmds);
        app
    }

    pub fn handle_event(&mut self, ev: Event, now: Now) {
        self.dirty = true;
        match ev {
            Event::Input(input) => self.on_input(input, now),
            ev => {
                match &ev {
                    // The worker has exited: no live fd and nothing to close.
                    Event::PortOpenFailed { key, conn, .. } => {
                        if let Some(p) = self.take_port(key, *conn)
                            && let Some(o) = &self.opener
                        {
                            o.cancel_own_open(&p.devnode);
                        }
                    }
                    Event::PortGone { key, conn, .. } => {
                        self.take_port(key, *conn);
                    }
                    _ => {}
                }
                let cmds = self.core.handle(ev, now);
                self.exec(cmds);
            }
        }
    }

    /// Runs `Core::on_timer` when its deadline has passed, and recomputes
    /// the per-tab rates once per second.
    pub fn tick(&mut self, now: Now) {
        if self.core.next_deadline().is_some_and(|d| d <= now.mono) {
            let cmds = self.core.on_timer(now);
            self.exec(cmds);
            self.dirty = true;
        }
        if now.mono >= self.next_rate {
            self.update_rates(now.mono);
            self.next_rate = now.mono + RATE_EVERY;
            // Also refreshes the status line (expiring messages).
            self.dirty = true;
        }
    }

    /// Renders if something changed and the last frame is at least 16 ms old.
    pub fn draw<B: Backend>(&mut self, term: &mut Terminal<B>) -> io::Result<()> {
        let t = Instant::now();
        if !self.dirty || self.last_frame.is_some_and(|f| t < f + FRAME) {
            return Ok(());
        }
        term.draw(|f| ui::render(f, &self.core, &self.view))
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.dirty = false;
        self.last_frame = Some(t);
        Ok(())
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Earliest of: the core's timer, the next allowed frame (if dirty) and
    /// the next rate tick.
    pub fn next_wake(&self) -> Option<Instant> {
        let mut w = self.next_rate;
        if let Some(d) = self.core.next_deadline() {
            w = w.min(d);
        }
        if self.dirty {
            w = w.min(self.last_frame.map_or_else(Instant::now, |f| f + FRAME));
        }
        Some(w)
    }

    /// One loop step: wait for an event (or the next wake-up, at most
    /// 250 ms), drain whatever else is queued so floods coalesce, tick, and
    /// draw if due.
    pub fn pump<B: Backend>(
        &mut self,
        rx: &Receiver<Event>,
        term: &mut Terminal<B>,
    ) -> io::Result<()> {
        let wait = self
            .next_wake()
            .map_or(MAX_WAIT, |w| w.saturating_duration_since(Instant::now()))
            .min(MAX_WAIT);
        if let Ok(ev) = rx.recv_timeout(wait) {
            self.handle_event(ev, now());
            let start = Instant::now();
            while start.elapsed() < DRAIN_BUDGET {
                match rx.try_recv() {
                    Ok(ev) => self.handle_event(ev, now()),
                    Err(_) => break,
                }
            }
        }
        self.tick(now());
        self.draw(term)
    }

    fn on_input(&mut self, input: crossterm::event::Event, now: Now) {
        use crossterm::event::Event as In;
        let outcome = match input {
            In::Key(k) => ui::on_key(&mut self.view, &mut self.core, k),
            In::Mouse(m) => ui::on_mouse(&mut self.view, &mut self.core, m),
            In::Paste(s) => ui::on_paste(&mut self.view, &s),
            _ => UiOutcome::Redraw, // resize, focus
        };
        match outcome {
            UiOutcome::None | UiOutcome::Redraw => {}
            UiOutcome::Quit => self.quit = true,
            UiOutcome::Save => self.save(now),
            UiOutcome::Action(a) => {
                let cmds = self.core.action(a, now);
                self.exec(cmds);
            }
        }
    }

    /// Removes the port entry if it belongs to connection `conn`.
    fn take_port(&mut self, key: &BoardKey, conn: ConnId) -> Option<Port> {
        if self.ports.get(key).is_some_and(|p| p.conn == conn) {
            self.ports.remove(key)
        } else {
            None
        }
    }

    fn close_port(&self, p: Port) {
        if let Some(o) = &self.opener {
            o.note_own_close(&p.devnode);
        }
        p.handle.close();
    }

    fn exec(&mut self, cmds: Vec<Command>) {
        for cmd in order_commands(cmds) {
            match cmd {
                Command::Watch(nodes) => {
                    if let Some(o) = &self.opener {
                        o.set_watched(nodes);
                    }
                }
                Command::Close { key } => {
                    if let Some(p) = self.ports.remove(&key) {
                        self.close_port(p);
                    }
                }
                Command::Open {
                    key,
                    conn,
                    devnode,
                    baud,
                    policy,
                } => {
                    if let Some(old) = self.ports.remove(&key) {
                        self.close_port(old);
                    }
                    if let Some(o) = &self.opener {
                        o.note_own_open(&devnode);
                    }
                    let handle = spawn_port(
                        key.clone(),
                        conn,
                        devnode.clone(),
                        baud,
                        policy,
                        self.tx.clone(),
                    );
                    self.ports.insert(
                        key,
                        Port {
                            handle,
                            devnode,
                            conn,
                        },
                    );
                }
                Command::Write { key, bytes } => {
                    if let Some(p) = self.ports.get(&key) {
                        p.handle.write(bytes);
                    }
                }
                Command::SetBaud { key, baud } => {
                    if let Some(p) = self.ports.get(&key) {
                        p.handle.set_baud(baud);
                    }
                }
                Command::ResetPulse { key, family } => {
                    if let Some(p) = self.ports.get(&key) {
                        p.handle.reset_pulse(family);
                    }
                }
                Command::Persist { key, prefs } => {
                    self.prefs.insert(key, prefs);
                    if let Some(path) = &self.state_path
                        && let Err(e) = store::save(path, &self.prefs)
                    {
                        self.view
                            .set_status(format!("could not save {}: {e}", path.display()));
                    }
                }
            }
        }
    }

    fn update_rates(&mut self, t: Instant) {
        let dt = self
            .last_rate_at
            .map(|p| t.saturating_duration_since(p).as_secs_f64());
        let mut rates = HashMap::new();
        let mut seen = HashMap::new();
        for tab in self.core.tabs() {
            if let (Some(dt), Some(prev)) = (dt, self.last_rx.get(&tab.key))
                && dt > 0.0
            {
                let delta = tab.rx_bytes.saturating_sub(*prev);
                rates.insert(tab.key.clone(), delta as f64 / dt / 1000.0);
            }
            seen.insert(tab.key.clone(), tab.rx_bytes);
        }
        self.view.rate_kbps = rates;
        self.last_rx = seen;
        self.last_rate_at = Some(t);
    }

    /// `s`: focused tab's scrollback as plain text into
    /// `comrade-<board>-<YYYYMMDD-HHMMSS>.log`.
    fn save(&mut self, now: Now) {
        let Some(tab) = self.core.tabs().get(self.core.focus()) else {
            self.view.set_status("nothing to save");
            return;
        };
        let mut out = String::new();
        let lines = (0..tab.scroll.len())
            .filter_map(|i| tab.scroll.get(i))
            .chain(tab.asm.partial());
        for l in lines {
            if self.view.timestamps {
                let _ = write!(out, "{} ", l.at.format("%H:%M:%S%.3f"));
            }
            out.push_str(&strip_sgr(&l.text));
            out.push('\n');
        }
        let name = format!(
            "comrade-{}-{}.log",
            sanitize_name(&tab.name),
            now.wall.format("%Y%m%d-%H%M%S")
        );
        let path = self.save_dir.join(name);
        match std::fs::write(&path, out) {
            Ok(()) => self.view.set_status(format!("saved {}", path.display())),
            Err(e) => self
                .view
                .set_status(format!("save failed: {}: {e}", path.display())),
        }
    }
}

/// Execution order within one batch: `Watch`, then `Close`, then `Open`,
/// then the rest, keeping relative order inside each group. `Watch` goes
/// first because `note_own_open` is ignored for nodes not yet watched;
/// `Close` before `Open` so a reopen never overlaps the old fd.
pub fn order_commands(mut cmds: Vec<Command>) -> Vec<Command> {
    cmds.sort_by_key(|c| match c {
        Command::Watch(_) => 0,
        Command::Close { .. } => 1,
        Command::Open { .. } => 2,
        _ => 3,
    });
    cmds
}

/// File-name-safe board name: anything outside `[A-Za-z0-9._-]` becomes `_`.
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---- process and terminal plumbing ----

static TERMINATE: AtomicBool = AtomicBool::new(false);
static MAIN_THREAD: OnceLock<ThreadId> = OnceLock::new();
/// Last panic message from a non-main thread, shown in the status line.
static WORKER_PANIC: Mutex<Option<String>> = Mutex::new(None);

extern "C" fn on_signal(_: nix::libc::c_int) {
    TERMINATE.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() -> nix::Result<()> {
    let act = SigAction::new(
        SigHandler::Handler(on_signal),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    for sig in [Signal::SIGTERM, Signal::SIGHUP] {
        // SAFETY: the handler only stores to an atomic (async-signal-safe).
        unsafe { sigaction(sig, &act) }?;
    }
    Ok(())
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    );
}

/// Restores the terminal when dropped, on every exit path out of `run`.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Main-thread panics restore the terminal first, then run the previous
/// hook. Other threads (port workers catch their own panics and report
/// `PortGone`) must not tear down the UI: their message goes to the status
/// line instead of being printed over the screen.
fn install_panic_hook() {
    let _ = MAIN_THREAD.set(thread::current().id());
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if MAIN_THREAD.get() == Some(&thread::current().id()) {
            restore_terminal();
            prev(info);
        } else {
            let name = thread::current().name().unwrap_or("worker").to_string();
            let msg = format!("thread '{name}' {info}").replace('\n', " ");
            if let Ok(mut slot) = WORKER_PANIC.lock() {
                *slot = Some(msg);
            }
        }
    }));
}

/// Forwards terminal input. Exits when the app is gone or the terminal
/// keeps failing (e.g. it was closed).
fn spawn_input(tx: Sender<Event>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name("input".into()).spawn(move || {
        let mut errors = 0;
        loop {
            match crossterm::event::read() {
                Ok(ev) => {
                    errors = 0;
                    if tx.send(Event::Input(ev)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    errors += 1;
                    if errors >= 50 {
                        return;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
    })
}

pub fn run(args: Args) -> anyhow::Result<()> {
    // Before the TUI starts, so a corrupt-file warning prints normally.
    let state_path = store::state_path();
    let prefs = state_path.as_deref().map(store::load).unwrap_or_default();
    let core = Core::new(prefs, args.baud.unwrap_or(DEFAULT_BAUD), args.port);
    let (tx, rx) = unbounded();
    let (opener, opener_err) = match OpenerWatch::spawn(tx.clone()) {
        Ok(o) => (Some(o), None),
        Err(e) => (None, Some(e)),
    };
    install_signal_handlers()?;
    install_panic_hook();

    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut app = App::new(core, opener, tx.clone(), state_path);
    if let Some(e) = opener_err {
        app.view
            .set_status(format!("upload detection off (inotify: {e})"));
    }
    // udev_watch sends Event::EnumerationDone after the startup
    // enumeration; Core treats later adds as hotplugs. Without udev there
    // are no adds at all.
    if let Err(e) = udev_watch::spawn(tx.clone()) {
        app.core.startup_done();
        app.view
            .set_status(format!("hotplug detection off (udev: {e})"));
    }
    let input = spawn_input(tx)?;

    while !app.should_quit() && !TERMINATE.load(Ordering::SeqCst) && !input.is_finished() {
        if let Some(msg) = WORKER_PANIC.lock().ok().and_then(|mut m| m.take()) {
            app.view.set_status(msg);
            app.dirty = true;
        }
        app.pump(&rx, &mut term)?;
    }
    // Close every port before the terminal is handed back.
    drop(app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{Family, LinePolicy};
    use crate::core::TabState;
    use crate::store::LineEnding;
    use crossbeam_channel::unbounded;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use nix::fcntl::OFlag;
    use nix::pty::{PtyMaster, grantpt, posix_openpt, ptsname_r, unlockpt};
    use ratatui::backend::TestBackend;
    use std::os::fd::AsFd;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    /// Pty tests run one at a time: a fork in one test briefly holds copies
    /// of the other tests' fds, which the opener watch would see as foreign.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now() -> Now {
        Now {
            mono: Instant::now(),
            wall: chrono::Local::now(),
        }
    }

    struct Pty {
        master: PtyMaster,
        path: PathBuf,
    }

    /// CLOEXEC master; the slave is only ever opened by the port worker.
    fn make_pty() -> Pty {
        let master = posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let path = PathBuf::from(ptsname_r(&master).unwrap());
        Pty { master, path }
    }

    fn write_all(master: &PtyMaster, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match nix::unistd::write(master.as_fd(), bytes) {
                Ok(n) => bytes = &bytes[n..],
                Err(nix::errno::Errno::EINTR | nix::errno::Errno::EAGAIN) => {}
                Err(e) => panic!("master write: {e}"),
            }
        }
    }

    struct Rig {
        app: App,
        rx: Receiver<Event>,
        term: Terminal<TestBackend>,
    }

    fn rig(ports: Vec<PathBuf>, state_path: Option<PathBuf>) -> Rig {
        let (tx, rx) = unbounded();
        let opener = OpenerWatch::spawn(tx.clone()).expect("opener watch");
        let core = Core::new(HashMap::new(), 115_200, ports);
        let app = App::new(core, Some(opener), tx, state_path);
        let term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        Rig { app, rx, term }
    }

    impl Rig {
        /// Runs the real loop step until `cond` holds or `limit` passes.
        fn until(&mut self, limit: Duration, mut cond: impl FnMut(&App) -> bool) -> bool {
            let end = Instant::now() + limit;
            while Instant::now() < end {
                self.app.pump(&self.rx, &mut self.term).unwrap();
                if cond(&self.app) {
                    return true;
                }
            }
            false
        }

        fn state(&self) -> TabState {
            self.app.core.tabs()[0].state.clone()
        }

        fn screen(&mut self) -> String {
            // Force a frame regardless of the 16 ms cap.
            std::thread::sleep(Duration::from_millis(20));
            self.app.dirty = true;
            self.app.draw(&mut self.term).unwrap();
            let buf = self.term.backend().buffer();
            let area = buf.area;
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn texts(&self) -> Vec<String> {
            let t = &self.app.core.tabs()[0];
            (0..t.scroll.len())
                .map(|i| t.scroll.get(i).unwrap().text.clone())
                .collect()
        }

        fn connected(&mut self) {
            assert!(
                self.until(Duration::from_secs(2), |a| a.core.tabs()[0].state
                    == TabState::Connected),
                "never connected: {:?}",
                self.state()
            );
        }
    }

    fn key(c: char) -> Event {
        Event::Input(crossterm::event::Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
    }

    #[test]
    fn app_boot_ok_is_rendered() {
        let _g = serial();
        let pty = make_pty();
        let mut r = rig(vec![pty.path.clone()], None);
        r.connected();
        write_all(&pty.master, b"boot ok\n");
        assert!(
            r.until(Duration::from_secs(2), |a| a.core.tabs()[0].scroll.len()
                == 1)
        );
        let screen = r.screen();
        assert!(screen.contains("boot ok"), "{screen}");
        assert!(screen.contains("●"), "{screen}");
        // Our own open must never have been taken for a foreign one.
        assert_eq!(r.state(), TabState::Connected);
    }

    #[test]
    fn app_throughput_no_loss() {
        let _g = serial();
        let pty = make_pty();
        let mut r = rig(vec![pty.path.clone()], None);
        r.connected();
        let total: usize = 1_000_000usize.div_ceil(9);
        let master = pty.master;
        let writer = std::thread::spawn(move || {
            let mut data = Vec::with_capacity(total * 9);
            for i in 0..total {
                data.extend_from_slice(format!("{i:08}\n").as_bytes());
            }
            write_all(&master, &data);
            master // keep the master open until the reader is done
        });
        let last = format!("{:08}", total - 1);
        let done = r.until(Duration::from_secs(8), |a| {
            let s = &a.core.tabs()[0].scroll;
            !s.is_empty() && s.get(s.len() - 1).unwrap().text == last
        });
        let _master = writer.join().unwrap();
        assert!(done, "last line never arrived");
        let texts = r.texts();
        assert_eq!(texts.len(), 100_000);
        let first = total - texts.len();
        for (j, t) in texts.iter().enumerate() {
            assert_eq!(t, &format!("{:08}", first + j), "gap at index {j}");
        }
        assert_eq!(r.app.core.tabs()[0].rx_bytes, (total * 9) as u64);
    }

    #[test]
    fn app_foreign_open_pauses_and_resumes_manual_port() {
        let _g = serial();
        let pty = make_pty();
        let mut r = rig(vec![pty.path.clone()], None);
        r.connected();
        write_all(&pty.master, b"before\n");
        assert!(
            r.until(Duration::from_secs(2), |a| a.core.tabs()[0].scroll.len()
                == 1)
        );
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exec sleep 0.6 < \"$0\"")
            .arg(&pty.path)
            .spawn()
            .unwrap();
        assert!(
            r.until(Duration::from_secs(2), |a| matches!(
                a.core.tabs()[0].state,
                TabState::Paused(_)
            )),
            "never paused: {:?}",
            r.state()
        );
        assert!(r.screen().contains("sleep"));
        child.wait().unwrap();
        r.connected();
        write_all(&pty.master, b"after\n");
        assert!(r.until(Duration::from_secs(2), |a| {
            let s = &a.core.tabs()[0].scroll;
            s.get(s.len() - 1).is_some_and(|l| l.text == "after")
        }));
        let texts = r.texts();
        let uploads = texts.iter().filter(|t| t.contains("upload")).count();
        assert_eq!(uploads, 1, "{texts:?}");
        assert!(
            !texts.iter().any(|t| t.contains("reconnected")),
            "{texts:?}"
        );
    }

    #[test]
    fn app_port_gone_keeps_running() {
        let _g = serial();
        let pty = make_pty();
        let mut r = rig(vec![pty.path.clone()], None);
        r.connected();
        drop(pty.master);
        assert!(
            r.until(Duration::from_secs(2), |a| a.core.tabs()[0].state
                == TabState::Waiting),
            "{:?}",
            r.state()
        );
        assert!(!r.app.should_quit());
        assert!(r.app.ports.is_empty());
        assert!(r.screen().contains("○"));
    }

    #[test]
    fn app_missing_port_waits_and_schedules_retry() {
        let _g = serial();
        let mut r = rig(vec!["/nonexistent/ttyX".into()], None);
        assert!(r.until(Duration::from_secs(2), |a| a.core.tabs()[0].state
            == TabState::Waiting
            && a.ports.is_empty()));
        assert!(r.app.core.next_deadline().is_some());
        assert!(!r.app.should_quit());
    }

    #[test]
    fn app_open_failure_shows_error() {
        let (tx, _rx) = unbounded();
        let core = Core::new(HashMap::new(), 115_200, vec!["/dev/ttyX".into()]);
        let mut app = App::new(core, None, tx, None);
        let key = app.core.tabs()[0].key.clone();
        app.handle_event(
            Event::PortOpenFailed {
                key,
                conn: 1,
                err: crate::event::OpenError::Other("boom".into()),
            },
            now(),
        );
        assert!(matches!(app.core.tabs()[0].state, TabState::Error(_)));
        assert!(app.ports.is_empty());
        assert!(!app.should_quit());
    }

    #[test]
    fn app_next_wake_includes_retry_deadline() {
        let (tx, _rx) = unbounded();
        let core = Core::new(HashMap::new(), 115_200, vec!["/dev/ttyX".into()]);
        let mut app = App::new(core, None, tx, None);
        let key = app.core.tabs()[0].key.clone();
        let t = now();
        app.handle_event(
            Event::PortGone {
                key,
                conn: 1,
                reason: "x".into(),
            },
            t,
        );
        app.dirty = false;
        app.next_rate = t.mono + Duration::from_secs(10);
        assert_eq!(app.next_wake(), Some(t.mono + Duration::from_secs(1)));
    }

    #[test]
    fn app_save_writes_plain_text_with_timestamps() {
        let _g = serial();
        let pty = make_pty();
        let mut r = rig(vec![pty.path.clone()], None);
        let dir = tempfile::tempdir().unwrap();
        r.app.save_dir = dir.path().to_path_buf();
        r.connected();
        write_all(&pty.master, b"\x1b[31mred\x1b[0m line\nplain\n");
        assert!(
            r.until(Duration::from_secs(2), |a| a.core.tabs()[0].scroll.len()
                == 2)
        );
        r.app.handle_event(key('s'), now());
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
        let tab = sanitize_name(&r.app.core.tabs()[0].name);
        assert!(name.starts_with(&format!("comrade-{tab}-")), "{name}");
        assert!(name.ends_with(".log"), "{name}");
        // comrade-<name>-YYYYMMDD-HHMMSS.log
        let stamp = &name[name.len() - 19..name.len() - 4];
        assert!(
            stamp.chars().enumerate().all(|(i, c)| if i == 8 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }),
            "{stamp}"
        );
        let text = std::fs::read_to_string(&files[0]).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text:?}");
        assert!(!text.contains('\x1b'));
        for (l, want) in lines.iter().zip(["red line", "plain"]) {
            // HH:MM:SS.mmm + space
            assert_eq!(&l[13..], want);
            assert_eq!(l.as_bytes()[2], b':');
            assert_eq!(l.as_bytes()[8], b'.');
            assert_eq!(l.as_bytes()[12], b' ');
        }
        let (msg, _) = r.app.view.status_msg.clone().unwrap();
        assert!(msg.contains(&name), "{msg}");

        // Timestamps off: bare lines.
        r.app.handle_event(key('t'), now());
        std::thread::sleep(Duration::from_millis(1100)); // new file name
        r.app.handle_event(key('s'), now());
        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        assert_eq!(files.len(), 2);
        let text = std::fs::read_to_string(&files[1]).unwrap();
        assert_eq!(text, "red line\nplain\n");
    }

    #[test]
    fn app_persist_saves_prefs_and_reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("comrade/boards.toml");
        let (tx, _rx) = unbounded();
        let core = Core::new(HashMap::new(), 115_200, vec![]);
        let mut app = App::new(core, None, tx.clone(), Some(path.clone()));
        let prefs = BoardPrefs {
            baud: 9600,
            line_ending: LineEnding::CrLf,
        };
        app.exec(vec![Command::Persist {
            key: BoardKey("k".into()),
            prefs: prefs.clone(),
        }]);
        let loaded = crate::store::load(&path);
        assert_eq!(loaded.get(&BoardKey("k".into())), Some(&prefs));
        assert!(app.view.status_msg.is_none());

        // Unwritable location: status message, not a crash.
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let core = Core::new(HashMap::new(), 115_200, vec![]);
        let mut app = App::new(core, None, tx, Some(blocker.join("boards.toml")));
        app.exec(vec![Command::Persist {
            key: BoardKey("k".into()),
            prefs,
        }]);
        assert!(app.view.status_msg.is_some());
        assert!(!app.should_quit());
    }

    #[test]
    fn app_rate_from_rx_bytes() {
        let _g = serial();
        let pty = make_pty();
        let mut r = rig(vec![pty.path.clone()], None);
        r.connected();
        // Baseline exactly at t0; the next rate tick is then t0 + 1 s.
        let t0 = now();
        r.app.next_rate = t0.mono;
        r.app.tick(t0);
        write_all(&pty.master, &[b'x'; 2000]);
        assert!(
            r.until(Duration::from_secs(2), |a| a.core.tabs()[0].rx_bytes
                == 2000)
        );
        let key = r.app.core.tabs()[0].key.clone();
        let t1 = Now {
            mono: t0.mono + Duration::from_secs(1),
            wall: t0.wall,
        };
        r.app.tick(t1);
        let rate = r.app.view.rate_kbps.get(&key).copied().unwrap();
        assert!((rate - 2.0).abs() < 1e-9, "{rate}");
    }

    #[test]
    fn app_quit_key() {
        let (tx, _rx) = unbounded();
        let core = Core::new(HashMap::new(), 115_200, vec![]);
        let mut app = App::new(core, None, tx, None);
        assert!(!app.should_quit());
        app.handle_event(key('q'), now());
        assert!(app.should_quit());
    }

    #[test]
    fn app_next_wake_is_bounded() {
        let (tx, _rx) = unbounded();
        let core = Core::new(HashMap::new(), 115_200, vec![]);
        let app = App::new(core, None, tx, None);
        let w = app.next_wake().expect("rate tick always pending");
        assert!(w <= Instant::now() + Duration::from_secs(1));
    }

    fn k(s: &str) -> BoardKey {
        BoardKey(s.into())
    }

    #[test]
    fn order_watch_close_open_rest_stable() {
        let open = |s: &str| Command::Open {
            key: k(s),
            conn: 1,
            devnode: Path::new("/dev/x").into(),
            baud: 1,
            policy: LinePolicy {
                dtr: false,
                rts: false,
            },
        };
        let cmds = vec![
            Command::Write {
                key: k("w1"),
                bytes: vec![1],
            },
            open("o1"),
            Command::Close { key: k("c1") },
            Command::ResetPulse {
                key: k("r"),
                family: Family::Unknown,
            },
            open("o2"),
            Command::Watch(vec!["/dev/x".into()]),
            Command::Close { key: k("c2") },
            Command::Write {
                key: k("w2"),
                bytes: vec![2],
            },
        ];
        let got = order_commands(cmds);
        let want = vec![
            Command::Watch(vec!["/dev/x".into()]),
            Command::Close { key: k("c1") },
            Command::Close { key: k("c2") },
            open("o1"),
            open("o2"),
            Command::Write {
                key: k("w1"),
                bytes: vec![1],
            },
            Command::ResetPulse {
                key: k("r"),
                family: Family::Unknown,
            },
            Command::Write {
                key: k("w2"),
                bytes: vec![2],
            },
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn sanitize_board_names() {
        assert_eq!(sanitize_name("ESP32-S3/C3"), "ESP32-S3_C3");
        assert_eq!(sanitize_name("Uno"), "Uno");
        assert_eq!(sanitize_name("a b:c.d_e"), "a_b_c.d_e");
        assert_eq!(sanitize_name("ü"), "_");
    }
}
