use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use comrade::board::{BoardKey, Family, LinePolicy};
use comrade::event::{Event, OpenError};
use comrade::io::{PortHandle, spawn_port};
use crossbeam_channel::{Receiver, Sender, unbounded};
use nix::poll::{PollFd, PollFlags, PollTimeout};

const WAIT: Duration = Duration::from_secs(2);

struct Pty {
    master: OwnedFd,
    path: PathBuf,
}

fn make_pty() -> Pty {
    let p = nix::pty::openpty(None, None).unwrap();
    let path = std::fs::read_link(format!("/proc/self/fd/{}", p.slave.as_raw_fd())).unwrap();
    drop(p.slave); // the worker is the only slave opener
    Pty {
        master: p.master,
        path,
    }
}

fn key() -> BoardKey {
    BoardKey("test".into())
}

fn start(path: &std::path::Path) -> (PortHandle, Receiver<Event>) {
    let (tx, rx): (Sender<Event>, _) = unbounded();
    let policy = LinePolicy {
        dtr: false,
        rts: false,
    };
    let h = spawn_port(key(), 7, path.to_path_buf(), 115200, policy, tx);
    (h, rx)
}

fn expect_opened(rx: &Receiver<Event>) {
    match rx.recv_timeout(WAIT).expect("event") {
        Event::PortOpened { conn, .. } => assert_eq!(conn, 7),
        Event::PortOpenFailed { err, .. } => panic!("open failed {err:?}"),
        _ => panic!("expected PortOpened"),
    }
}

fn read_master(master: &OwnedFd, want: usize) -> Vec<u8> {
    let deadline = Instant::now() + WAIT;
    let mut out = Vec::new();
    while out.len() < want && Instant::now() < deadline {
        let mut fds = [PollFd::new(master.as_fd(), PollFlags::POLLIN)];
        let _ = nix::poll::poll(&mut fds, PollTimeout::from(50u16));
        let mut buf = [0u8; 256];
        if let Ok(n) = nix::unistd::read(master, &mut buf) {
            out.extend_from_slice(&buf[..n]);
        }
    }
    out
}

fn fd_open_to(path: &std::path::Path) -> bool {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .any(|e| std::fs::read_link(e.path()).is_ok_and(|l| l == path))
}

#[test]
fn reads_bytes_from_device() {
    let pty = make_pty();
    let (_h, rx) = start(&pty.path);
    expect_opened(&rx);
    nix::unistd::write(&pty.master, b"hello\n").unwrap();
    let mut got = Vec::new();
    while !got.windows(5).any(|w| w == b"hello") {
        match rx.recv_timeout(WAIT).expect("data") {
            Event::PortData { bytes, .. } => got.extend(bytes),
            _ => panic!("unexpected event"),
        }
    }
}

#[test]
fn writes_to_device() {
    let pty = make_pty();
    let (h, rx) = start(&pty.path);
    expect_opened(&rx);
    h.write(b"x\n".to_vec());
    assert_eq!(read_master(&pty.master, 2), b"x\n");
}

#[test]
fn close_releases_fd_fast() {
    let pty = make_pty();
    let (h, rx) = start(&pty.path);
    expect_opened(&rx);
    assert!(fd_open_to(&pty.path));
    h.close();
    let t = Instant::now();
    while fd_open_to(&pty.path) {
        assert!(t.elapsed() < Duration::from_millis(50), "fd still open");
        std::thread::sleep(Duration::from_millis(1));
    }
    // no event after a requested close
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
}

#[test]
fn drop_also_closes() {
    let pty = make_pty();
    let (h, rx) = start(&pty.path);
    expect_opened(&rx);
    drop(h);
    let t = Instant::now();
    while fd_open_to(&pty.path) {
        assert!(t.elapsed() < Duration::from_millis(50), "fd still open");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[allow(deprecated)] // raw flock(2) on purpose
fn not_exclusive() {
    let pty = make_pty();
    let (_h, rx) = start(&pty.path);
    expect_opened(&rx);
    let second = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pty.path);
    let second = second.expect("second open");
    // An exclusive flock by another process (esptool, pyserial) must succeed.
    nix::fcntl::flock(
        second.as_raw_fd(),
        nix::fcntl::FlockArg::LockExclusiveNonblock,
    )
    .expect("flock LOCK_EX|LOCK_NB");
}

#[test]
fn write_after_unplug_reports_gone() {
    let pty = make_pty();
    let (h, rx) = start(&pty.path);
    expect_opened(&rx);
    drop(pty.master);
    match rx.recv_timeout(WAIT).expect("gone") {
        Event::PortGone { conn, .. } => assert_eq!(conn, 7),
        _ => panic!("expected PortGone"),
    }
    h.write(b"late".to_vec());
    h.set_baud(9600);
    h.reset_pulse(Family::UartBridge);
    h.close();
}

#[test]
fn bad_path_reports_open_failed() {
    let (_h, rx) = start(std::path::Path::new("/dev/does-not-exist-comrade"));
    match rx.recv_timeout(WAIT).expect("event") {
        Event::PortOpenFailed { err, .. } => assert!(matches!(err, OpenError::Other(_))),
        _ => panic!("expected PortOpenFailed"),
    }
}

#[test]
fn set_baud_and_pulse_on_pty_do_not_kill_worker() {
    let pty = make_pty();
    let (h, rx) = start(&pty.path);
    expect_opened(&rx);
    h.set_baud(9600);
    h.reset_pulse(Family::NativeCdc);
    nix::unistd::write(&pty.master, b"ok").unwrap();
    match rx.recv_timeout(WAIT).expect("data") {
        Event::PortData { bytes, .. } => assert!(!bytes.is_empty()),
        _ => panic!("expected data"),
    }
}

fn assert_fd_released_within_50ms(path: &std::path::Path) {
    let t = Instant::now();
    while fd_open_to(path) {
        assert!(t.elapsed() < Duration::from_millis(50), "fd still open");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn open_at_74880_succeeds() {
    // Custom bauds go through termios2/BOTHER. A pty may reject it; that is
    // ignored (port stays open at the placeholder rate), so open must succeed.
    let pty = make_pty();
    let (tx, rx) = unbounded();
    let policy = LinePolicy {
        dtr: false,
        rts: false,
    };
    let _h = spawn_port(key(), 7, pty.path.clone(), 74880, policy, tx);
    expect_opened(&rx);
}

#[test]
fn set_baud_74880_keeps_port_alive() {
    let pty = make_pty();
    let (h, rx) = start(&pty.path);
    expect_opened(&rx);
    h.set_baud(74880);
    h.set_baud(12345);
    nix::unistd::write(&pty.master, b"after").unwrap();
    match rx.recv_timeout(WAIT).expect("event") {
        Event::PortData { bytes, .. } => assert!(!bytes.is_empty()),
        Event::PortGone { reason, .. } => panic!("PortGone: {reason}"),
        _ => panic!("unexpected event"),
    }
}

#[test]
fn close_is_fast_with_stuck_writes() {
    let pty = make_pty();
    let (h, rx) = start(&pty.path);
    expect_opened(&rx);
    // master is never read: the pty buffer fills and writes stall
    for _ in 0..64 {
        h.write(vec![b'a'; 64 * 1024]);
    }
    std::thread::sleep(Duration::from_millis(100));
    h.close();
    assert_fd_released_within_50ms(&pty.path);
}

#[test]
fn close_right_after_reset_pulse_is_fast() {
    for fam in [Family::UartBridge, Family::NativeCdc] {
        let pty = make_pty();
        let (h, rx) = start(&pty.path);
        expect_opened(&rx);
        h.reset_pulse(fam);
        std::thread::sleep(Duration::from_millis(10));
        h.close();
        assert_fd_released_within_50ms(&pty.path);
    }
}

#[test]
fn receiver_dropped_stops_worker() {
    let pty = make_pty();
    let (h, rx) = start(&pty.path);
    expect_opened(&rx);
    drop(rx);
    nix::unistd::write(&pty.master, b"data").unwrap();
    assert_fd_released_within_50ms(&pty.path);
    drop(h);
}
