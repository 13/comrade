//! Serial I/O worker: one thread per open port.
//!
//! Opens the device node directly with termios instead of `serialport`'s
//! `open_native`: that call takes `TIOCEXCL` and `flock` (even in "shared"
//! mode), which would make uploaders such as esptool fail. We never lock.

use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::stat::Mode;
use nix::sys::termios::{
    self, BaudRate, ControlFlags, InputFlags, SetArg, SpecialCharacterIndices, Termios, cfmakeraw,
};
use nix::unistd::{AccessFlags, Gid, Group, access, read, write};

use crate::board::{BoardKey, Family, LinePolicy};
use crate::event::{ConnId, Event, OpenError};

const READ_BUF: usize = 8 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

enum PortCmd {
    Write(Vec<u8>),
    SetBaud(u32),
    ResetPulse(Family),
    Close,
}

/// Handle to a running port worker. Dropping it closes the port.
pub struct PortHandle {
    tx: Sender<PortCmd>,
    wake: Option<Arc<EventFd>>,
}

impl PortHandle {
    fn send(&self, cmd: PortCmd) {
        // Worker may already be gone; that is fine.
        let _ = self.tx.send(cmd);
        if let Some(w) = &self.wake {
            let _ = w.write(1);
        }
    }

    pub fn write(&self, bytes: Vec<u8>) {
        self.send(PortCmd::Write(bytes));
    }

    pub fn set_baud(&self, baud: u32) {
        self.send(PortCmd::SetBaud(baud));
    }

    pub fn reset_pulse(&self, family: Family) {
        self.send(PortCmd::ResetPulse(family));
    }

    /// Signals the worker and returns without joining.
    pub fn close(self) {
        // Drop does the signalling.
    }
}

impl Drop for PortHandle {
    fn drop(&mut self) {
        self.send(PortCmd::Close);
    }
}

pub fn spawn_port(
    key: BoardKey,
    conn: ConnId,
    devnode: PathBuf,
    baud: u32,
    policy: LinePolicy,
    tx: Sender<Event>,
) -> PortHandle {
    let (cmd_tx, cmd_rx) = unbounded();
    let wake = match EventFd::from_flags(EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            // No wake fd: report and hand back an inert handle.
            let _ = tx.send(Event::PortOpenFailed {
                key,
                conn,
                err: OpenError::Other(format!("eventfd: {e}")),
            });
            return PortHandle {
                tx: cmd_tx,
                wake: None,
            };
        }
    };
    let name = format!(
        "port-{}",
        devnode
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    let worker_wake = wake.clone();
    let (k2, t2) = (key.clone(), tx.clone());
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        let res = catch_unwind(AssertUnwindSafe(|| {
            run(
                &key,
                conn,
                &devnode,
                baud,
                policy,
                &tx,
                &cmd_rx,
                &worker_wake,
            )
        }));
        if res.is_err() {
            let _ = tx.send(Event::PortGone {
                key,
                conn,
                reason: "worker panicked".into(),
            });
        }
    });
    if let Err(e) = spawned {
        let _ = t2.send(Event::PortOpenFailed {
            key: k2,
            conn,
            err: OpenError::Other(format!("thread spawn: {e}")),
        });
    }
    PortHandle {
        tx: cmd_tx,
        wake: Some(wake),
    }
}

fn group_name(devnode: &Path) -> String {
    match std::fs::metadata(devnode) {
        Ok(m) => {
            let gid = m.gid();
            match Group::from_gid(Gid::from_raw(gid)) {
                Ok(Some(g)) => g.name,
                _ => gid.to_string(),
            }
        }
        Err(_) => "unknown".into(),
    }
}

/// Errors that only mean "this is a pty / not a real modem": ignorable.
fn benign_tty_err(e: Errno) -> bool {
    matches!(e, Errno::ENOTTY | Errno::EINVAL)
}

mod ioc {
    nix::ioctl_write_ptr_bad!(tiocmset, nix::libc::TIOCMSET, nix::libc::c_int);
    nix::ioctl_read_bad!(tiocmget, nix::libc::TIOCMGET, nix::libc::c_int);
    nix::ioctl_read_bad!(tcgets2, nix::libc::TCGETS2, nix::libc::termios2);
    nix::ioctl_write_ptr_bad!(tcsets2, nix::libc::TCSETS2, nix::libc::termios2);
}

/// Modem bits to write so DTR/RTS match `policy` and every other bit is kept.
/// Used with a single TIOCMSET so the lines never pass through an
/// intermediate state (DTR=0,RTS=1 resets ESP boards).
fn target_modem_bits(current: nix::libc::c_int, policy: LinePolicy) -> nix::libc::c_int {
    let mut bits = current & !(nix::libc::TIOCM_DTR | nix::libc::TIOCM_RTS);
    if policy.dtr {
        bits |= nix::libc::TIOCM_DTR;
    }
    if policy.rts {
        bits |= nix::libc::TIOCM_RTS;
    }
    bits
}

/// Moves DTR/RTS to `policy` atomically. ENOTTY/EINVAL (ptys) are ignored.
fn apply_lines(fd: &OwnedFd, policy: LinePolicy) -> Result<(), Errno> {
    let mut cur: nix::libc::c_int = 0;
    match unsafe { ioc::tiocmget(fd.as_raw_fd(), &mut cur) } {
        Ok(_) => {}
        Err(e) if benign_tty_err(e) => return Ok(()),
        Err(e) => return Err(e),
    }
    let bits = target_modem_bits(cur, policy);
    match unsafe { ioc::tiocmset(fd.as_raw_fd(), &bits) } {
        Ok(_) => Ok(()),
        Err(e) if benign_tty_err(e) => Ok(()),
        Err(e) => Err(e),
    }
}

fn std_baud_rate(baud: u32) -> Option<BaudRate> {
    Some(match baud {
        300 => BaudRate::B300,
        600 => BaudRate::B600,
        1200 => BaudRate::B1200,
        2400 => BaudRate::B2400,
        4800 => BaudRate::B4800,
        9600 => BaudRate::B9600,
        19200 => BaudRate::B19200,
        38400 => BaudRate::B38400,
        57600 => BaudRate::B57600,
        115200 => BaudRate::B115200,
        230400 => BaudRate::B230400,
        460800 => BaudRate::B460800,
        500000 => BaudRate::B500000,
        576000 => BaudRate::B576000,
        921600 => BaudRate::B921600,
        1000000 => BaudRate::B1000000,
        1152000 => BaudRate::B1152000,
        1500000 => BaudRate::B1500000,
        2000000 => BaudRate::B2000000,
        2500000 => BaudRate::B2500000,
        3000000 => BaudRate::B3000000,
        3500000 => BaudRate::B3500000,
        4000000 => BaudRate::B4000000,
        _ => return None,
    })
}

/// Sets any baud: standard rates via termios, others via termios2 + BOTHER.
fn set_baud(fd: &OwnedFd, baud: u32) -> Result<(), String> {
    if let Some(rate) = std_baud_rate(baud) {
        let mut t = termios::tcgetattr(fd).map_err(|e| format!("tcgetattr: {e}"))?;
        termios::cfsetspeed(&mut t, rate).map_err(|e| format!("cfsetspeed: {e}"))?;
        return termios::tcsetattr(fd, SetArg::TCSANOW, &t).map_err(|e| format!("tcsetattr: {e}"));
    }
    let mut t: nix::libc::termios2 = unsafe { std::mem::zeroed() };
    unsafe { ioc::tcgets2(fd.as_raw_fd(), &mut t) }.map_err(|e| format!("TCGETS2: {e}"))?;
    t.c_cflag &= !(nix::libc::CBAUD as nix::libc::tcflag_t);
    t.c_cflag |= nix::libc::BOTHER as nix::libc::tcflag_t;
    t.c_ispeed = baud;
    t.c_ospeed = baud;
    unsafe { ioc::tcsets2(fd.as_raw_fd(), &t) }
        .map(|_| ())
        .map_err(|e| format!("TCSETS2: {e}"))
}

fn configure(fd: &OwnedFd) -> Result<(), String> {
    let mut t: Termios = termios::tcgetattr(fd).map_err(|e| format!("tcgetattr: {e}"))?;
    cfmakeraw(&mut t);
    t.input_flags
        .remove(InputFlags::IXON | InputFlags::IXOFF | InputFlags::IXANY);
    t.control_flags
        .remove(ControlFlags::CSIZE | ControlFlags::PARENB | ControlFlags::CSTOPB);
    t.control_flags.remove(ControlFlags::CRTSCTS);
    t.control_flags |= ControlFlags::CS8 | ControlFlags::CREAD | ControlFlags::CLOCAL;
    t.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
    t.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
    termios::cfsetspeed(&mut t, BaudRate::B115200).map_err(|e| format!("cfsetspeed: {e}"))?;
    termios::tcsetattr(fd, SetArg::TCSANOW, &t).map_err(|e| format!("tcsetattr: {e}"))
}

/// Maps an access/open errno: a missing node is `NotPresent` (the tab
/// waits and retries), EACCES is a permission problem, the rest is an error.
fn open_error(devnode: &Path, e: Errno) -> OpenError {
    match e {
        Errno::ENOENT | Errno::ENODEV | Errno::ENXIO => OpenError::NotPresent,
        Errno::EACCES => OpenError::PermissionDenied {
            group: group_name(devnode),
        },
        e => OpenError::Other(format!("{}: {e}", devnode.display())),
    }
}

fn open_port(devnode: &Path, baud: u32, policy: LinePolicy) -> Result<OwnedFd, OpenError> {
    access(devnode, AccessFlags::R_OK | AccessFlags::W_OK).map_err(|e| open_error(devnode, e))?;
    let fd = open(
        devnode,
        OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| open_error(devnode, e))?;
    configure(&fd).map_err(OpenError::Other)?;
    // A baud the driver rejects must not make the open fail; the port stays
    // at 115200 (the placeholder rate set by `configure`).
    let _ = set_baud(&fd, baud);
    apply_lines(&fd, policy).map_err(|e| OpenError::Other(format!("modem lines: {e}")))?;
    Ok(fd)
}

fn pulse_plan(family: Family, policy: LinePolicy) -> (LinePolicy, Duration) {
    match family {
        Family::UartBridge | Family::EspUsbJtag => (
            LinePolicy {
                dtr: false,
                rts: true,
            },
            Duration::from_millis(100),
        ),
        Family::NativeCdc | Family::Unknown => (
            LinePolicy {
                dtr: false,
                rts: policy.rts,
            },
            Duration::from_millis(50),
        ),
    }
}

/// Drains the command channel into `pending`; true if a Close was seen
/// (pending writes are then irrelevant).
fn drain(cmds: &Receiver<PortCmd>, pending: &mut VecDeque<PortCmd>) -> bool {
    while let Ok(cmd) = cmds.try_recv() {
        if matches!(cmd, PortCmd::Close) {
            return true;
        }
        pending.push_back(cmd);
    }
    false
}

fn poll_timeout(deadline: Option<Instant>) -> PollTimeout {
    match deadline {
        None => PollTimeout::NONE,
        Some(d) => {
            let left = d.saturating_duration_since(Instant::now());
            let ms = left.as_micros().div_ceil(1000).min(u128::from(u16::MAX));
            PollTimeout::from(ms as u16)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    key: &BoardKey,
    conn: ConnId,
    devnode: &Path,
    baud: u32,
    policy: LinePolicy,
    tx: &Sender<Event>,
    cmds: &Receiver<PortCmd>,
    wake: &EventFd,
) {
    let fd = match open_port(devnode, baud, policy) {
        Ok(fd) => fd,
        Err(err) => {
            let _ = tx.send(Event::PortOpenFailed {
                key: key.clone(),
                conn,
                err,
            });
            return;
        }
    };
    if tx
        .send(Event::PortOpened {
            key: key.clone(),
            conn,
        })
        .is_err()
    {
        return;
    }
    let gone = |reason: String| {
        let _ = tx.send(Event::PortGone {
            key: key.clone(),
            conn,
            reason,
        });
    };
    let mut buf = vec![0u8; READ_BUF];
    let mut pending: VecDeque<PortCmd> = VecDeque::new();
    let mut wbuf: VecDeque<u8> = VecDeque::new();
    let mut write_deadline: Option<Instant> = None;
    let mut pulse_end: Option<Instant> = None;
    loop {
        // Run queued commands; a pulse in flight holds back later ones.
        while pulse_end.is_none() {
            let Some(cmd) = pending.pop_front() else {
                break;
            };
            match cmd {
                PortCmd::Close => return,
                PortCmd::Write(bytes) => {
                    if wbuf.is_empty() {
                        write_deadline = Some(Instant::now() + WRITE_TIMEOUT);
                    }
                    wbuf.extend(bytes);
                }
                // A failed baud change keeps the old rate; Event has no
                // info variant, so it is intentionally silent.
                PortCmd::SetBaud(b) => {
                    let _ = set_baud(&fd, b);
                }
                PortCmd::ResetPulse(family) => {
                    let (first, dur) = pulse_plan(family, policy);
                    if let Err(e) = apply_lines(&fd, first) {
                        return gone(format!("reset: {e}"));
                    }
                    pulse_end = Some(Instant::now() + dur);
                }
            }
        }
        let now = Instant::now();
        if pulse_end.is_some_and(|d| now >= d) {
            pulse_end = None;
            write_deadline = Some(Instant::now() + WRITE_TIMEOUT);
            if let Err(e) = apply_lines(&fd, policy) {
                return gone(format!("reset: {e}"));
            }
            continue;
        }

        let want_out = !wbuf.is_empty() && pulse_end.is_none();
        let deadline = [pulse_end, if want_out { write_deadline } else { None }]
            .into_iter()
            .flatten()
            .min();
        let (port_ev, wake_ev) = {
            let mut flags = PollFlags::POLLIN;
            if want_out {
                flags |= PollFlags::POLLOUT;
            }
            let mut fds = [
                PollFd::new(fd.as_fd(), flags),
                PollFd::new(wake.as_fd(), PollFlags::POLLIN),
            ];
            match poll(&mut fds, poll_timeout(deadline)) {
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(e) => return gone(format!("poll: {e}")),
            }
            (
                fds[0].revents().unwrap_or(PollFlags::empty()),
                fds[1].revents().unwrap_or(PollFlags::empty()),
            )
        };

        if !wake_ev.is_empty() {
            let mut counter = [0u8; 8];
            let _ = read(wake.as_fd(), &mut counter);
            if drain(cmds, &mut pending) {
                return;
            }
        }

        if port_ev.contains(PollFlags::POLLIN) {
            match read(&fd, &mut buf) {
                Ok(0) => return gone("port closed".into()),
                Ok(n) => {
                    let ev = Event::PortData {
                        key: key.clone(),
                        conn,
                        bytes: buf[..n].to_vec(),
                    };
                    if tx.send(ev).is_err() {
                        return; // receiver gone
                    }
                }
                Err(Errno::EAGAIN | Errno::EINTR) => {}
                Err(e @ (Errno::EIO | Errno::ENXIO | Errno::ENODEV)) => {
                    return gone(format!("device gone: {e}"));
                }
                Err(e) => return gone(format!("read: {e}")),
            }
        } else if port_ev.intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL)
        {
            return gone("device hung up".into());
        }

        if want_out {
            if port_ev.contains(PollFlags::POLLOUT) {
                let (a, _) = wbuf.as_slices();
                match write(&fd, a) {
                    Ok(0) => return gone("write: zero".into()),
                    Ok(n) => {
                        wbuf.drain(..n);
                        write_deadline = Some(Instant::now() + WRITE_TIMEOUT);
                    }
                    Err(Errno::EAGAIN | Errno::EINTR) => {}
                    Err(e) => return gone(format!("write: {e}")),
                }
            }
            // A stuck write (no CTS, full buffer) drops what is pending but
            // keeps the port: reading must go on.
            if !wbuf.is_empty() && write_deadline.is_some_and(|d| Instant::now() >= d) {
                let dropped = wbuf.len();
                wbuf.clear();
                write_deadline = None;
                let ev = Event::PortWarning {
                    key: key.clone(),
                    conn,
                    msg: format!("write timed out, {dropped} bytes dropped"),
                };
                if tx.send(ev).is_err() {
                    return; // receiver gone
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::libc::{TIOCM_CTS, TIOCM_DTR, TIOCM_RTS};

    fn p(dtr: bool, rts: bool) -> LinePolicy {
        LinePolicy { dtr, rts }
    }

    #[test]
    fn kernel_default_to_both_off_keeps_other_bits() {
        let cur = TIOCM_DTR | TIOCM_RTS | TIOCM_CTS;
        assert_eq!(target_modem_bits(cur, p(false, false)), TIOCM_CTS);
    }

    #[test]
    fn policy_bits_set_and_unrelated_preserved() {
        assert_eq!(target_modem_bits(0, p(true, false)), TIOCM_DTR);
        assert_eq!(
            target_modem_bits(TIOCM_CTS, p(false, true)),
            TIOCM_CTS | TIOCM_RTS
        );
        assert_eq!(target_modem_bits(TIOCM_RTS, p(true, false)), TIOCM_DTR);
    }

    #[test]
    fn missing_node_errnos_map_to_not_present() {
        let p = Path::new("/dev/ttyACM0");
        for e in [Errno::ENOENT, Errno::ENODEV, Errno::ENXIO] {
            assert_eq!(open_error(p, e), OpenError::NotPresent);
        }
        assert!(matches!(
            open_error(p, Errno::EACCES),
            OpenError::PermissionDenied { .. }
        ));
        assert!(matches!(open_error(p, Errno::EBUSY), OpenError::Other(_)));
    }

    #[test]
    fn pulse_plans() {
        let pol = p(true, false);
        assert_eq!(pulse_plan(Family::UartBridge, pol).0, p(false, true));
        assert_eq!(pulse_plan(Family::NativeCdc, pol).0, p(false, false));
    }
}
