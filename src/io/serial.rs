//! Serial I/O worker: one thread per open port.
//!
//! Opens the device node directly with termios instead of `serialport`'s
//! `open_native`: that call takes `TIOCEXCL` and `flock` (even in "shared"
//! mode), which would make uploaders such as esptool fail. We never lock.

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
    self, BaudRate, ControlFlags, SetArg, SpecialCharacterIndices, Termios, cfmakeraw,
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
    nix::ioctl_write_ptr_bad!(tiocmbis, nix::libc::TIOCMBIS, nix::libc::c_int);
    nix::ioctl_write_ptr_bad!(tiocmbic, nix::libc::TIOCMBIC, nix::libc::c_int);
}

fn set_line(fd: &OwnedFd, bit: nix::libc::c_int, level: bool) -> Result<(), Errno> {
    let r = unsafe {
        if level {
            ioc::tiocmbis(fd.as_raw_fd(), &bit)
        } else {
            ioc::tiocmbic(fd.as_raw_fd(), &bit)
        }
    };
    match r {
        Ok(_) => Ok(()),
        Err(e) if benign_tty_err(e) => Ok(()),
        Err(e) => Err(e),
    }
}

fn set_lines(fd: &OwnedFd, dtr: bool, rts: bool) -> Result<(), Errno> {
    set_line(fd, nix::libc::TIOCM_DTR, dtr)?;
    set_line(fd, nix::libc::TIOCM_RTS, rts)
}

fn baud_rate(baud: u32) -> Result<BaudRate, String> {
    Ok(match baud {
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
        _ => return Err(format!("unsupported baud {baud}")),
    })
}

fn configure(fd: &OwnedFd, baud: u32) -> Result<(), String> {
    let rate = baud_rate(baud)?;
    let mut t: Termios = termios::tcgetattr(fd).map_err(|e| format!("tcgetattr: {e}"))?;
    cfmakeraw(&mut t);
    t.control_flags
        .remove(ControlFlags::CSIZE | ControlFlags::PARENB | ControlFlags::CSTOPB);
    t.control_flags.remove(ControlFlags::CRTSCTS);
    t.control_flags |= ControlFlags::CS8 | ControlFlags::CREAD | ControlFlags::CLOCAL;
    t.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
    t.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
    termios::cfsetspeed(&mut t, rate).map_err(|e| format!("cfsetspeed: {e}"))?;
    termios::tcsetattr(fd, SetArg::TCSANOW, &t).map_err(|e| format!("tcsetattr: {e}"))
}

fn change_baud(fd: &OwnedFd, baud: u32) -> Result<(), String> {
    let rate = baud_rate(baud)?;
    let mut t = termios::tcgetattr(fd).map_err(|e| format!("tcgetattr: {e}"))?;
    termios::cfsetspeed(&mut t, rate).map_err(|e| format!("cfsetspeed: {e}"))?;
    termios::tcsetattr(fd, SetArg::TCSANOW, &t).map_err(|e| format!("tcsetattr: {e}"))
}

fn open_port(devnode: &Path, baud: u32, policy: LinePolicy) -> Result<OwnedFd, OpenError> {
    match access(devnode, AccessFlags::R_OK | AccessFlags::W_OK) {
        Ok(()) => {}
        Err(Errno::EACCES) => {
            return Err(OpenError::PermissionDenied {
                group: group_name(devnode),
            });
        }
        Err(e) => return Err(OpenError::Other(format!("{}: {e}", devnode.display()))),
    }
    let fd = open(
        devnode,
        OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| match e {
        Errno::EACCES => OpenError::PermissionDenied {
            group: group_name(devnode),
        },
        e => OpenError::Other(format!("{}: {e}", devnode.display())),
    })?;
    if let Err(e) = configure(&fd, baud) {
        // pty-like devices may reject termios details; real failures surface here.
        return Err(OpenError::Other(e));
    }
    set_lines(&fd, policy.dtr, policy.rts)
        .map_err(|e| OpenError::Other(format!("modem lines: {e}")))?;
    Ok(fd)
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
    loop {
        let (port_ev, wake_ev) = {
            let mut fds = [
                PollFd::new(fd.as_fd(), PollFlags::POLLIN),
                PollFd::new(wake.as_fd(), PollFlags::POLLIN),
            ];
            match poll(&mut fds, PollTimeout::NONE) {
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
            while let Ok(cmd) = cmds.try_recv() {
                match cmd {
                    PortCmd::Close => return,
                    PortCmd::Write(bytes) => {
                        if let Err(e) = write_all(&fd, &bytes) {
                            return gone(format!("write: {e}"));
                        }
                    }
                    PortCmd::SetBaud(b) => {
                        if let Err(e) = change_baud(&fd, b) {
                            return gone(e);
                        }
                    }
                    PortCmd::ResetPulse(family) => {
                        if let Err(e) = reset_pulse(&fd, family, policy) {
                            return gone(format!("reset: {e}"));
                        }
                    }
                }
            }
        }

        if port_ev.contains(PollFlags::POLLIN) {
            match read(&fd, &mut buf) {
                Ok(0) => return gone("port closed".into()),
                Ok(n) => {
                    let _ = tx.send(Event::PortData {
                        key: key.clone(),
                        conn,
                        bytes: buf[..n].to_vec(),
                    });
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
    }
}

fn write_all(fd: &OwnedFd, mut bytes: &[u8]) -> Result<(), Errno> {
    let deadline = Instant::now() + WRITE_TIMEOUT;
    while !bytes.is_empty() {
        match write(fd, bytes) {
            Ok(0) => return Err(Errno::EIO),
            Ok(n) => bytes = &bytes[n..],
            Err(Errno::EINTR) => {}
            Err(Errno::EAGAIN) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(Errno::ETIMEDOUT);
                }
                let mut p = [PollFd::new(fd.as_fd(), PollFlags::POLLOUT)];
                let ms = u16::try_from(left.as_millis().min(100)).unwrap_or(100);
                let _ = poll(&mut p, PollTimeout::from(ms));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn reset_pulse(fd: &OwnedFd, family: Family, policy: LinePolicy) -> Result<(), Errno> {
    match family {
        Family::UartBridge | Family::EspUsbJtag => {
            set_lines(fd, false, true)?;
            std::thread::sleep(Duration::from_millis(100));
            set_line(fd, nix::libc::TIOCM_RTS, false)?;
        }
        Family::NativeCdc | Family::Unknown => {
            set_line(fd, nix::libc::TIOCM_DTR, false)?;
            std::thread::sleep(Duration::from_millis(50));
            set_line(fd, nix::libc::TIOCM_DTR, true)?;
        }
    }
    set_lines(fd, policy.dtr, policy.rts)
}
