//! Foreign-open detection (spec §6.4): inotify on device nodes plus a /proc
//! holder scan.
//!
//! inotify tells us *that* a watched node was opened or closed, within
//! microseconds and without root. It does not say by whom, and it merges
//! identical unread events, so a /proc scan is the authority whenever it can
//! see the holder. comrade's own opens are announced up front through
//! [`OpenerWatch::note_own_open`] / [`OpenerWatch::note_own_close`] so they
//! can be told apart from holders the scan cannot see (other users, root).
//!
//! All paths are canonicalized internally so `/dev/serial/by-id/...` symlinks
//! and `/dev/ttyACM0` match. Events carry the devnode exactly as it was given
//! to [`OpenerWatch::set_watched`].

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use crossbeam_channel::Sender;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};

use crate::event::{Event, Holder};

/// Re-scans when an IN_OPEN shows no visible holder and no own open is
/// pending. The kernel queues IN_OPEN just before the opener's fd lands in
/// its fd table, so a very fast scan can miss it.
const OPEN_RESCANS: u32 = 3;
const OPEN_RESCAN_GAP: Duration = Duration::from_millis(2);

fn node_mask() -> WatchMask {
    WatchMask::OPEN
        | WatchMask::CLOSE_WRITE
        | WatchMask::CLOSE_NOWRITE
        | WatchMask::ATTRIB
        | WatchMask::DELETE_SELF
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// (st_dev, st_ino) of a path, following symlinks.
fn inode_of(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

struct Node {
    /// The path as given to `set_watched`; events carry this one.
    original: PathBuf,
    wd: WatchDescriptor,
    /// Inode the watch sits on, to notice a node that was removed and
    /// recreated under the same name.
    inode: Option<(u64, u64)>,
    /// `None` = no foreign holder.
    state: Option<Holder>,
}

struct Shared {
    watches: Watches,
    /// canonical path -> watched node
    nodes: HashMap<PathBuf, Node>,
    /// watch descriptor -> canonical path
    by_wd: HashMap<WatchDescriptor, PathBuf>,
    /// canonical path -> (pending own opens, pending own closes)
    own: HashMap<PathBuf, (u32, u32)>,
    tx: Sender<Event>,
    /// Receiver gone or `OpenerWatch` dropped: the reader thread exits.
    closed: bool,
}

impl Shared {
    fn send(&mut self, ev: Event) {
        if self.tx.send(ev).is_err() {
            self.closed = true;
        }
    }

    fn take_own_open(&mut self, canon: &Path) -> bool {
        match self.own.get_mut(canon) {
            Some((opens, _)) if *opens > 0 => {
                *opens -= 1;
                true
            }
            _ => false,
        }
    }

    fn take_own_close(&mut self, canon: &Path) -> bool {
        match self.own.get_mut(canon) {
            Some((_, closes)) if *closes > 0 => {
                *closes -= 1;
                true
            }
            _ => false,
        }
    }

    /// Forget a node. Emits `ForeignReleased` if a foreign holder was known,
    /// so Core never keeps a stale holder.
    fn drop_node(&mut self, canon: &Path) {
        let Some(node) = self.nodes.remove(canon) else {
            return;
        };
        self.by_wd.remove(&node.wd);
        self.own.remove(canon);
        // Fails harmlessly after IN_IGNORED: the kernel already removed it.
        let _ = self.watches.remove(node.wd);
        if node.state.is_some() {
            self.send(Event::ForeignReleased {
                devnode: node.original,
            });
        }
    }

    fn add_node(&mut self, canon: PathBuf, original: PathBuf) {
        let Ok(wd) = self.watches.add(&canon, node_mask()) else {
            return;
        };
        let state = foreign_holders(&canon, std::process::id())
            .into_iter()
            .next();
        if let Some(holder) = &state {
            self.send(Event::ForeignOpen {
                devnode: original.clone(),
                holder: holder.clone(),
            });
        }
        self.by_wd.insert(wd.clone(), canon.clone());
        self.nodes.insert(
            canon.clone(),
            Node {
                original,
                wd,
                inode: inode_of(&canon),
                state,
            },
        );
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Watches device nodes for foreign opens and emits
/// `Event::ForeignOpen` / `Event::ForeignReleased`.
pub struct OpenerWatch {
    shared: Arc<Mutex<Shared>>,
}

impl OpenerWatch {
    /// Starts the `opener-watch` reader thread.
    pub fn spawn(tx: Sender<Event>) -> io::Result<Self> {
        let inotify = Inotify::init()?;
        let shared = Arc::new(Mutex::new(Shared {
            watches: inotify.watches(),
            nodes: HashMap::new(),
            by_wd: HashMap::new(),
            own: HashMap::new(),
            tx,
            closed: false,
        }));
        let reader_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("opener-watch".into())
            .spawn(move || reader(inotify, &reader_shared))?;
        Ok(Self { shared })
    }

    /// Replaces the watched set (`Command::Watch`). New nodes are scanned
    /// right away; removed nodes that had a foreign holder get
    /// `ForeignReleased`. Nodes that do not exist are skipped.
    pub fn set_watched(&self, nodes: Vec<PathBuf>) {
        let mut wanted: HashMap<PathBuf, PathBuf> = HashMap::new();
        for original in nodes {
            if let Ok(canon) = std::fs::canonicalize(&original) {
                wanted.insert(canon, original);
            }
        }
        let mut s = lock(&self.shared);
        let gone: Vec<PathBuf> = s
            .nodes
            .keys()
            .filter(|c| !wanted.contains_key(*c))
            .cloned()
            .collect();
        for canon in gone {
            s.drop_node(&canon);
        }
        for (canon, original) in wanted {
            match s.nodes.get_mut(&canon) {
                Some(node) => node.original = original,
                None => s.add_node(canon, original),
            }
        }
    }

    /// Call right before comrade opens `node` itself.
    pub fn note_own_open(&self, node: &Path) {
        let canon = canonical(node);
        lock(&self.shared).own.entry(canon).or_default().0 += 1;
    }

    /// Call right before comrade closes its fd on `node`.
    pub fn note_own_close(&self, node: &Path) {
        let canon = canonical(node);
        lock(&self.shared).own.entry(canon).or_default().1 += 1;
    }
}

impl Drop for OpenerWatch {
    fn drop(&mut self) {
        let mut s = lock(&self.shared);
        s.closed = true;
        // Wake the blocked reader: adding and removing a watch queues an
        // IN_IGNORED event, after which it sees `closed` and exits.
        if let Ok(wd) = s.watches.add("/", WatchMask::DELETE_SELF) {
            let _ = s.watches.remove(wd);
        }
    }
}

fn reader(mut inotify: Inotify, shared: &Mutex<Shared>) {
    let mut buf = [0u8; 4096];
    loop {
        let batch: Vec<(WatchDescriptor, EventMask)> = match inotify.read_events_blocking(&mut buf)
        {
            Ok(events) => events.map(|e| (e.wd, e.mask)).collect(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        for (wd, mask) in batch {
            if lock(shared).closed {
                return;
            }
            handle(shared, &wd, mask);
        }
        if lock(shared).closed {
            return;
        }
    }
}

fn handle(shared: &Mutex<Shared>, wd: &WatchDescriptor, mask: EventMask) {
    if mask.contains(EventMask::Q_OVERFLOW) {
        resync(&mut lock(shared));
        return;
    }
    let canon = {
        let s = lock(shared);
        match s.by_wd.get(wd) {
            Some(c) => c.clone(),
            None => return, // stale event for a watch we already dropped
        }
    };
    if mask.intersects(EventMask::IGNORED | EventMask::DELETE_SELF) {
        lock(shared).drop_node(&canon);
        return;
    }
    if mask.contains(EventMask::ATTRIB) {
        let mut s = lock(shared);
        let gone = s
            .nodes
            .get(&canon)
            .is_some_and(|n| n.inode.is_none() || inode_of(&canon) != n.inode);
        if gone {
            s.drop_node(&canon);
        }
        return;
    }
    if mask.contains(EventMask::OPEN) {
        on_open(shared, &canon);
    }
    if mask.intersects(EventMask::CLOSE_WRITE | EventMask::CLOSE_NOWRITE) {
        on_close(shared, &canon);
    }
}

fn on_open(shared: &Mutex<Shared>, canon: &Path) {
    let own_pending = lock(shared).own.get(canon).is_some_and(|c| c.0 > 0);
    let me = std::process::id();
    let mut named = foreign_holders(canon, me);
    if named.is_empty() && !own_pending {
        for _ in 0..OPEN_RESCANS {
            thread::sleep(OPEN_RESCAN_GAP);
            named = foreign_holders(canon, me);
            if !named.is_empty() {
                break;
            }
        }
    }
    let mut s = lock(shared);
    let own = s.take_own_open(canon);
    let Some(node) = s.nodes.get_mut(canon) else {
        return;
    };
    let was_free = node.state.is_none();
    let holder = match named.into_iter().next() {
        Some(h) => h,
        None if !own => Holder::Unknown,
        None => return,
    };
    node.state = Some(holder.clone());
    if was_free {
        let devnode = node.original.clone();
        s.send(Event::ForeignOpen { devnode, holder });
    }
}

fn on_close(shared: &Mutex<Shared>, canon: &Path) {
    let own = lock(shared).take_own_close(canon);
    let named = foreign_holders(canon, std::process::id());
    let mut s = lock(shared);
    let Some(node) = s.nodes.get_mut(canon) else {
        return;
    };
    if node.state.is_some() && named.is_empty() && (!own || node.state != Some(Holder::Unknown)) {
        node.state = None;
        let devnode = node.original.clone();
        s.send(Event::ForeignReleased { devnode });
    }
}

/// Events were lost (queue overflow): reconcile every node with /proc.
/// Unknown holders are left alone since the scan cannot see them.
fn resync(s: &mut Shared) {
    let me = std::process::id();
    let mut out = Vec::new();
    for (canon, node) in s.nodes.iter_mut() {
        let named = foreign_holders(canon, me);
        match (named.into_iter().next(), &node.state) {
            (Some(holder), None) => {
                node.state = Some(holder.clone());
                out.push(Event::ForeignOpen {
                    devnode: node.original.clone(),
                    holder,
                });
            }
            (Some(holder), Some(_)) => node.state = Some(holder),
            (None, Some(Holder::Named { .. })) => {
                node.state = None;
                out.push(Event::ForeignReleased {
                    devnode: node.original.clone(),
                });
            }
            (None, _) => {}
        }
    }
    for ev in out {
        s.send(ev);
    }
}

/// Processes other than `own_pid` that hold `node` open, found by following
/// `/proc/<pid>/fd/*` links. Processes whose fd dir is unreadable (other
/// users, root) are skipped.
pub fn foreign_holders(node: &Path, own_pid: u32) -> Vec<Holder> {
    let target = canonical(node);
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in procs.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own_pid {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let holds = fds
            .flatten()
            .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|l| l == target));
        if holds {
            let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
            out.push(Holder::Named {
                pid,
                comm: comm.trim().to_string(),
            });
        }
    }
    out
}
