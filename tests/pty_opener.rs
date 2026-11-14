//! Opener watch against real pty slaves, with child processes as the
//! "foreign" openers (esptool / avrdude stand-ins).
//!
//! Tests run one at a time (`SERIAL`): a parallel test's fork briefly holds
//! copies of this process's fds until it execs, and a /proc scan that catches
//! that window would see a "foreign" holder.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use comrade::event::{Event, Holder};
use comrade::io::opener_watch::{OpenerWatch, foreign_holders};
use crossbeam_channel::{Receiver, unbounded};
use nix::fcntl::OFlag;
use nix::pty::{PtyMaster, grantpt, posix_openpt, ptsname_r, unlockpt};

/// Deadline for an event the watch must produce.
const FAST: Duration = Duration::from_millis(200);
/// How long we listen to prove that nothing is emitted.
const QUIET: Duration = Duration::from_millis(200);
/// Bound for setup polling.
const SLOW: Duration = Duration::from_secs(1);

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct Pty {
    _master: PtyMaster,
    path: PathBuf,
}

/// Master is CLOEXEC and the slave is never opened here, so spawned children
/// never inherit either end by accident.
fn make_pty() -> Pty {
    let master = posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC).unwrap();
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let path = PathBuf::from(ptsname_r(&master).unwrap());
    Pty {
        _master: master,
        path,
    }
}

/// A foreign process holding the node open. Killed and reaped on drop.
struct Opener(Child);

impl Opener {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn kill(mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Opener {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `cat` opens the node itself after exec, so the open fires IN_OPEN while
/// the process already has its final comm.
fn cat_opener(path: &Path) -> Opener {
    let child = Command::new("cat")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cat");
    Opener(child)
}

/// `sleep 5` with stdin redirected from the node. The open happens in this
/// process, so only call this while the node is not watched.
fn sleep_holder(path: &Path) -> Opener {
    let child = Command::new("sleep")
        .arg("5")
        .stdin(File::open(path).expect("open slave"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    Opener(child)
}

fn watch(path: &Path) -> (OpenerWatch, Receiver<Event>) {
    let (tx, rx) = unbounded();
    let w = OpenerWatch::spawn(tx).expect("spawn opener watch");
    w.set_watched(vec![path.to_path_buf()]);
    (w, rx)
}

fn kind(e: &Event) -> &'static str {
    match e {
        Event::ForeignOpen { .. } => "ForeignOpen",
        Event::ForeignReleased { .. } => "ForeignReleased",
        _ => "other",
    }
}

fn expect_open(rx: &Receiver<Event>) -> (PathBuf, Holder) {
    match rx.recv_timeout(FAST).expect("ForeignOpen within 200 ms") {
        Event::ForeignOpen { devnode, holder } => (devnode, holder),
        e => panic!("expected ForeignOpen, got {}", kind(&e)),
    }
}

fn expect_released(rx: &Receiver<Event>) -> PathBuf {
    match rx
        .recv_timeout(FAST)
        .expect("ForeignReleased within 200 ms")
    {
        Event::ForeignReleased { devnode } => devnode,
        e => panic!("expected ForeignReleased, got {}", kind(&e)),
    }
}

fn expect_silent(rx: &Receiver<Event>) {
    if let Ok(e) = rx.recv_timeout(QUIET) {
        panic!("expected no event, got {}", kind(&e));
    }
}

fn wait_held_by(path: &Path, pid: u32) {
    let t = Instant::now();
    while !foreign_holders(path, std::process::id())
        .iter()
        .any(|h| matches!(h, Holder::Named { pid: p, .. } if *p == pid))
    {
        assert!(t.elapsed() < SLOW, "child never showed up as holder");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn assert_named(holder: &Holder, pid: u32, comm: &str) {
    match holder {
        Holder::Named { pid: p, comm: c } => {
            assert_eq!(*p, pid);
            assert_eq!(c, comm);
        }
        Holder::Unknown => panic!("expected Named holder, got Unknown"),
    }
}

#[test]
fn child_open_emits_foreign_open_with_comm() {
    let _g = serial();
    let pty = make_pty();
    let (_w, rx) = watch(&pty.path);
    let child = cat_opener(&pty.path);
    let (devnode, holder) = expect_open(&rx);
    assert_eq!(devnode, pty.path);
    assert_named(&holder, child.pid(), "cat");
}

#[test]
fn child_exit_emits_released() {
    let _g = serial();
    let pty = make_pty();
    let (_w, rx) = watch(&pty.path);
    let child = cat_opener(&pty.path);
    expect_open(&rx);
    child.kill();
    assert_eq!(expect_released(&rx), pty.path);
    expect_silent(&rx);
}

#[test]
fn own_open_is_silent() {
    let _g = serial();
    let pty = make_pty();
    let (w, rx) = watch(&pty.path);
    w.note_own_open(&pty.path);
    let f = File::options()
        .read(true)
        .write(true)
        .open(&pty.path)
        .unwrap();
    expect_silent(&rx);
    w.note_own_close(&pty.path);
    drop(f);
    expect_silent(&rx);
}

#[test]
fn foreign_holders_excludes_self() {
    let _g = serial();
    let pty = make_pty();
    let _own = File::open(&pty.path).unwrap();
    let me = std::process::id();
    assert!(foreign_holders(&pty.path, me).is_empty());
    // With another own_pid we do see ourselves: the scan itself works.
    assert!(
        foreign_holders(&pty.path, 0)
            .iter()
            .any(|h| matches!(h, Holder::Named { pid, .. } if *pid == me))
    );
    let child = cat_opener(&pty.path);
    wait_held_by(&pty.path, child.pid());
    let holders = foreign_holders(&pty.path, me);
    assert_eq!(holders.len(), 1);
    assert_named(&holders[0], child.pid(), "cat");
}

#[test]
fn initial_scan_detects_preexisting_holder() {
    let _g = serial();
    let pty = make_pty();
    let child = sleep_holder(&pty.path);
    wait_held_by(&pty.path, child.pid());
    let (_w, rx) = watch(&pty.path);
    let (devnode, holder) = expect_open(&rx);
    assert_eq!(devnode, pty.path);
    assert_named(&holder, child.pid(), "sleep");
    expect_silent(&rx);
}

#[test]
fn no_duplicate_foreign_open() {
    let _g = serial();
    let pty = make_pty();
    let (w, rx) = watch(&pty.path);
    let a = cat_opener(&pty.path);
    expect_open(&rx);
    let b = cat_opener(&pty.path);
    wait_held_by(&pty.path, b.pid());
    expect_silent(&rx);
    // comrade's own reopen while a foreign holder sits on the port
    w.note_own_open(&pty.path);
    let own = File::open(&pty.path).unwrap();
    expect_silent(&rx);
    w.note_own_close(&pty.path);
    drop(own);
    expect_silent(&rx);
    a.kill();
    expect_silent(&rx);
    b.kill();
    assert_eq!(expect_released(&rx), pty.path);
    expect_silent(&rx);
}

#[test]
fn unwatch_held_node_emits_released() {
    let _g = serial();
    let pty = make_pty();
    let (w, rx) = watch(&pty.path);
    let _child = cat_opener(&pty.path);
    expect_open(&rx);
    w.set_watched(vec![]);
    assert_eq!(expect_released(&rx), pty.path);
    expect_silent(&rx);
}

#[test]
fn symlink_alias_reports_watched_path() {
    let _g = serial();
    let pty = make_pty();
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("usb-Espressif_board-if00");
    std::os::unix::fs::symlink(&pty.path, &link).unwrap();
    let (w, rx) = watch(&link);
    // own open through the real path is matched to the symlinked watch
    w.note_own_open(&pty.path);
    let own = File::open(&pty.path).unwrap();
    expect_silent(&rx);
    w.note_own_close(&link);
    drop(own);
    expect_silent(&rx);
    let child = cat_opener(&pty.path);
    let (devnode, holder) = expect_open(&rx);
    assert_eq!(devnode, link);
    assert_named(&holder, child.pid(), "cat");
    child.kill();
    assert_eq!(expect_released(&rx), link);
}

/// devpts emits no inotify event when a pty node goes away, so this uses a
/// regular file: unlinking it goes through `vfs_unlink` like a devtmpfs
/// node removed by udev (IN_ATTRIB while held, IN_DELETE_SELF once freed).
#[test]
fn node_removal_emits_released() {
    let _g = serial();
    let dir = tempfile::tempdir().unwrap();
    let node = dir.path().join("ttyACM0");
    File::create(&node).unwrap();
    let child = sleep_holder(&node);
    wait_held_by(&node, child.pid());
    let (_w, rx) = watch(&node);
    let (devnode, holder) = expect_open(&rx);
    assert_eq!(devnode, node);
    assert_named(&holder, child.pid(), "sleep");
    // `sleep` keeps its fd, so the inode lives on: only IN_ATTRIB fires.
    std::fs::remove_file(&node).unwrap();
    assert_eq!(expect_released(&rx), node);
    drop(child);
    expect_silent(&rx);
}
