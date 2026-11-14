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

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};

use crate::event::{Event, Holder};

/// Re-scans when an IN_OPEN shows no visible holder. The kernel queues
/// IN_OPEN just before the opener's fd lands in its fd table, so a very fast
/// scan can miss it. Done even when an own open is pending: comrade's and an
/// uploader's simultaneous opens can merge into one event.
const OPEN_RESCANS: u32 = 3;
const OPEN_RESCAN_GAP: Duration = Duration::from_millis(2);

/// A noted own open/close that no inotify event consumed within this time is
/// forgotten (the open failed, or its event was lost).
pub const OWN_TTL: Duration = Duration::from_secs(1);

const PROC: &str = "/proc";

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

/// Consumes one pending own operation noted within `OWN_TTL` before `now`.
/// Older entries are dropped first. Returns whether one was consumed.
pub fn take_pending(queue: &mut VecDeque<Instant>, now: Instant) -> bool {
    while queue
        .front()
        .is_some_and(|t| now.saturating_duration_since(*t) > OWN_TTL)
    {
        queue.pop_front();
    }
    queue.pop_front().is_some()
}

/// Own opens/closes announced by the app but not yet seen as events.
#[derive(Default)]
struct OwnPending {
    opens: VecDeque<Instant>,
    closes: VecDeque<Instant>,
}

struct Node {
    /// The path as given to `set_watched`; events carry this one.
    original: PathBuf,
    wd: WatchDescriptor,
    /// Inode the watch sits on, to notice a node that was removed and
    /// recreated under the same name.
    inode: Option<(u64, u64)>,
    /// `None` = no foreign holder. This is what Core was told.
    state: Option<Holder>,
    /// Every foreign holder visible in /proc at the last check. On close
    /// only these pids are re-checked.
    holders: Vec<Holder>,
    own: OwnPending,
}

struct Shared {
    watches: Watches,
    /// canonical path -> watched node
    nodes: HashMap<PathBuf, Node>,
    /// watch descriptor -> canonical path
    by_wd: HashMap<WatchDescriptor, PathBuf>,
    tx: Sender<Event>,
    /// Receiver gone or `OpenerWatch` dropped: the reader thread exits.
    closed: bool,
    /// canonical path -> pids the user accepted with `p` (see
    /// `accept_holders`). Never reported; dropped once they let go.
    /// Kept per path, not per watch, so a re-watch does not re-report them.
    ignored: HashMap<PathBuf, HashSet<u32>>,
}

impl Shared {
    fn send(&mut self, ev: Event) {
        if self.tx.send(ev).is_err() {
            self.closed = true;
        }
    }

    /// Forget a node. Emits `ForeignReleased` if a foreign holder was known,
    /// so Core never keeps a stale holder.
    fn drop_node(&mut self, canon: &Path) {
        let Some(node) = self.nodes.remove(canon) else {
            return;
        };
        self.by_wd.remove(&node.wd);
        // Fails harmlessly after IN_IGNORED: the kernel already removed it.
        let _ = self.watches.remove(node.wd);
        if node.state.is_some() {
            self.send(Event::ForeignReleased {
                devnode: node.original,
            });
        }
    }

    /// Drops holders the user accepted for `canon`.
    fn unignored(&self, canon: &Path, holders: Vec<Holder>) -> Vec<Holder> {
        let Some(ign) = self.ignored.get(canon) else {
            return holders;
        };
        holders
            .into_iter()
            .filter(|h| !pid_of(h).is_some_and(|p| ign.contains(&p)))
            .collect()
    }

    /// Starts watching with state free and no pending own operations.
    /// Returns false if inotify refused the watch.
    fn add_node(&mut self, canon: PathBuf, original: PathBuf) -> bool {
        let Ok(wd) = self.watches.add(&canon, node_mask()) else {
            return false;
        };
        self.by_wd.insert(wd.clone(), canon.clone());
        let inode = inode_of(&canon);
        self.nodes.insert(
            canon,
            Node {
                original,
                wd,
                inode,
                state: None,
                holders: Vec::new(),
                own: OwnPending::default(),
            },
        );
        true
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
            tx,
            closed: false,
            ignored: HashMap::new(),
        }));
        let reader_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("opener-watch".into())
            .spawn(move || reader(inotify, &reader_shared))?;
        Ok(Self { shared })
    }

    /// Replaces the watched set (`Command::Watch`). New nodes are scanned
    /// right away (one /proc pass, outside the lock); removed nodes that had
    /// a foreign holder get `ForeignReleased`. Nodes that do not exist are
    /// skipped.
    pub fn set_watched(&self, nodes: Vec<PathBuf>) {
        let mut wanted: HashMap<PathBuf, PathBuf> = HashMap::new();
        for original in nodes {
            if let Ok(canon) = std::fs::canonicalize(&original) {
                wanted.insert(canon, original);
            }
        }
        let mut added = HashSet::new();
        {
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
                    None => {
                        if s.add_node(canon.clone(), original) {
                            added.insert(canon);
                        }
                    }
                }
            }
        }
        if added.is_empty() {
            return;
        }
        // The watch is already live, so an open racing this scan is handled
        // by the reader; whoever sets state first wins and the other stays
        // silent.
        let found = holders_in(Path::new(PROC), &added, std::process::id());
        let mut s = lock(&self.shared);
        for (canon, holders) in found {
            let holders = s.unignored(&canon, holders);
            let Some(node) = s.nodes.get_mut(&canon) else {
                continue; // unwatched meanwhile
            };
            if node.state.is_some() {
                continue;
            }
            // Re-check under the lock: a holder that closed after the scan
            // has already had its IN_CLOSE handled against a free state.
            let holders: Vec<Holder> = holders
                .into_iter()
                .filter(|h| still_holds(h, &canon))
                .collect();
            let Some(first) = holders.first().cloned() else {
                continue;
            };
            node.holders = holders;
            node.state = Some(first.clone());
            let devnode = node.original.clone();
            s.send(Event::ForeignOpen {
                devnode,
                holder: first,
            });
        }
    }

    /// The user resumed (`p`) over the foreign holders of `node`: its
    /// current holders are ignored from now on (until they exit), its state
    /// becomes free, and no event is sent. Any other opener is reported as
    /// usual.
    pub fn accept_holders(&self, node: &Path) {
        let canon = canonical(node);
        let found = scan_one(&canon);
        let mut s = lock(&self.shared);
        let mut pids: HashSet<u32> = found.iter().filter_map(pid_of).collect();
        if let Some(n) = s.nodes.get_mut(&canon) {
            pids.extend(n.holders.iter().filter_map(pid_of));
            n.state = None;
            n.holders.clear();
        }
        if !pids.is_empty() {
            s.ignored.entry(canon).or_default().extend(pids);
        }
    }

    /// Call right before comrade opens `node` itself. No-op unless `node`
    /// is watched.
    pub fn note_own_open(&self, node: &Path) {
        let canon = canonical(node);
        if let Some(n) = lock(&self.shared).nodes.get_mut(&canon) {
            n.own.opens.push_back(Instant::now());
        }
    }

    /// Call when an open announced with `note_own_open` failed.
    pub fn cancel_own_open(&self, node: &Path) {
        let canon = canonical(node);
        if let Some(n) = lock(&self.shared).nodes.get_mut(&canon) {
            n.own.opens.pop_back();
        }
    }

    /// Call right before comrade closes its fd on `node`. No-op unless
    /// `node` is watched.
    pub fn note_own_close(&self, node: &Path) {
        let canon = canonical(node);
        if let Some(n) = lock(&self.shared).nodes.get_mut(&canon) {
            n.own.closes.push_back(Instant::now());
        }
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

fn scan_one(canon: &Path) -> Vec<Holder> {
    let targets = HashSet::from([canon.to_path_buf()]);
    holders_in(Path::new(PROC), &targets, std::process::id())
        .remove(canon)
        .unwrap_or_default()
}

fn pid_of(h: &Holder) -> Option<u32> {
    match h {
        Holder::Named { pid, .. } => Some(*pid),
        Holder::Unknown => None,
    }
}

fn on_open(shared: &Mutex<Shared>, canon: &Path) {
    // Pids already accounted for: recorded holders and accepted ones.
    let known: HashSet<u32> = {
        let s = lock(shared);
        let recorded = s
            .nodes
            .get(canon)
            .into_iter()
            .flat_map(|n| n.holders.iter().filter_map(pid_of));
        let ignored = s.ignored.get(canon).into_iter().flatten().copied();
        recorded.chain(ignored).collect()
    };
    // Rescan until a pid we did not know shows up: the opener's fd may not
    // be installed yet when IN_OPEN arrives.
    let mut found = scan_one(canon);
    for _ in 0..OPEN_RESCANS {
        if found
            .iter()
            .any(|h| pid_of(h).is_some_and(|p| !known.contains(&p)))
        {
            break;
        }
        thread::sleep(OPEN_RESCAN_GAP);
        found = scan_one(canon);
    }
    let mut s = lock(shared);
    let named = s.unignored(canon, found);
    let Some(node) = s.nodes.get_mut(canon) else {
        return;
    };
    let own = take_pending(&mut node.own.opens, Instant::now());
    let was_free = node.state.is_none();
    node.holders.clone_from(&named);
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

/// What an IN_CLOSE leads to, given the current state, the recorded
/// holders that still hold the node, and whether the close was comrade's own.
#[derive(Clone, Debug, PartialEq)]
pub enum CloseAction {
    /// Nothing changes. `relabel` names a remaining holder to report from
    /// now on (silently) when the reported one left.
    Keep { relabel: Option<Holder> },
    /// Emit `ForeignReleased` now. With `verify`, run a full /proc scan
    /// afterwards and re-emit `ForeignOpen` if a holder we never recorded
    /// turns up (its open merged with another one).
    Release { verify: bool },
}

/// Pure close decision (brief's release rule plus the fast-path ruling).
pub fn close_action(state: Option<&Holder>, remaining: &[Holder], own: bool) -> CloseAction {
    match (state, remaining.first()) {
        (None, _) => CloseAction::Keep { relabel: None },
        (Some(Holder::Unknown), None) if own => CloseAction::Keep { relabel: None },
        (Some(Holder::Unknown), None) => CloseAction::Release { verify: false },
        (Some(Holder::Named { .. }), None) => CloseAction::Release { verify: true },
        (Some(Holder::Unknown), Some(_)) => CloseAction::Keep { relabel: None },
        (Some(current), Some(first)) => CloseAction::Keep {
            relabel: (!remaining.contains(current)).then(|| first.clone()),
        },
    }
}

fn on_close(shared: &Mutex<Shared>, canon: &Path) {
    let (own, recorded, ignored) = {
        let mut s = lock(shared);
        let ignored: Vec<u32> = s
            .ignored
            .get(canon)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        let Some(node) = s.nodes.get_mut(canon) else {
            return;
        };
        let own = take_pending(&mut node.own.closes, Instant::now());
        (own, node.holders.clone(), ignored)
    };
    // Accepted holders that let go are forgotten (their pid may be reused).
    if !ignored.is_empty() {
        let gone: Vec<u32> = ignored
            .into_iter()
            .filter(|p| !pid_holds(*p, canon))
            .collect();
        let mut s = lock(shared);
        if let Some(set) = s.ignored.get_mut(canon) {
            set.retain(|p| !gone.contains(p));
            if set.is_empty() {
                s.ignored.remove(canon);
            }
        }
    }
    // Fast path: only the pids we know about. A gone pid counts as released.
    let remaining: Vec<Holder> = recorded
        .into_iter()
        .filter(|h| still_holds(h, canon))
        .collect();
    let verify = {
        let mut s = lock(shared);
        let Some(node) = s.nodes.get_mut(canon) else {
            return;
        };
        let action = close_action(node.state.as_ref(), &remaining, own);
        node.holders = remaining;
        match action {
            CloseAction::Keep { relabel } => {
                if let Some(h) = relabel {
                    node.state = Some(h);
                }
                false
            }
            CloseAction::Release { verify } => {
                node.state = None;
                let devnode = node.original.clone();
                s.send(Event::ForeignReleased { devnode });
                verify
            }
        }
    };
    if verify {
        verify_release(shared, canon);
    }
}

/// After a fast-path release: one full scan for a holder that was never
/// recorded. Comrade may reopen in this window; uploaders retry their sync.
fn verify_release(shared: &Mutex<Shared>, canon: &Path) {
    let found = scan_one(canon);
    if found.is_empty() {
        return;
    }
    let mut s = lock(shared);
    let found = s.unignored(canon, found);
    let Some(node) = s.nodes.get_mut(canon) else {
        return;
    };
    if node.state.is_some() {
        return;
    }
    // Same re-check as set_watched: drop holders that closed after the scan.
    let holders: Vec<Holder> = found
        .into_iter()
        .filter(|h| still_holds(h, canon))
        .collect();
    let Some(first) = holders.first().cloned() else {
        return;
    };
    node.holders = holders;
    node.state = Some(first.clone());
    let devnode = node.original.clone();
    s.send(Event::ForeignOpen {
        devnode,
        holder: first,
    });
}

/// Events were lost (queue overflow): reconcile every node with one /proc
/// pass and forget pending own operations, whose events may be gone.
/// Unknown holders are left alone since the scan cannot see them.
fn resync(s: &mut Shared) {
    let targets: HashSet<PathBuf> = s.nodes.keys().cloned().collect();
    let mut found = holders_in(Path::new(PROC), &targets, std::process::id());
    for (canon, hs) in found.iter_mut() {
        *hs = s.unignored(canon, std::mem::take(hs));
    }
    let mut out = Vec::new();
    for (canon, node) in s.nodes.iter_mut() {
        node.own = OwnPending::default();
        let named = found.remove(canon).unwrap_or_default();
        node.holders.clone_from(&named);
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

/// Whether a recorded holder still has `canon` open. Reads only that pid's
/// fd dir. An exited (or now unreadable) pid counts as released.
fn still_holds(holder: &Holder, canon: &Path) -> bool {
    pid_of(holder).is_some_and(|p| pid_holds(p, canon))
}

/// Whether process `pid` has `canon` open.
fn pid_holds(pid: u32, canon: &Path) -> bool {
    let Ok(fds) = std::fs::read_dir(Path::new(PROC).join(pid.to_string()).join("fd")) else {
        return false;
    };
    fds.flatten()
        .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|l| l == canon))
}

/// One pass over `<proc_root>/<pid>/fd/*` matching every link against
/// `targets` (canonical paths). Returns the holders per target that has
/// any, in pid order of the directory listing.
///
/// Skipped: `own_pid`, non-numeric entries, processes whose fd dir is
/// unreadable (other users, root), and processes whose `comm` is unreadable
/// or empty (they exited mid-scan).
pub fn holders_in(
    proc_root: &Path,
    targets: &HashSet<PathBuf>,
    own_pid: u32,
) -> HashMap<PathBuf, Vec<Holder>> {
    let mut out: HashMap<PathBuf, Vec<Holder>> = HashMap::new();
    if targets.is_empty() {
        return out;
    }
    let Ok(procs) = std::fs::read_dir(proc_root) else {
        return out;
    };
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
        let mut held: Vec<PathBuf> = Vec::new();
        for fd in fds.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path())
                && targets.contains(&link)
                && !held.contains(&link)
            {
                held.push(link);
            }
        }
        if held.is_empty() {
            continue;
        }
        let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        let comm = comm.trim();
        if comm.is_empty() {
            continue;
        }
        for target in held {
            out.entry(target).or_default().push(Holder::Named {
                pid,
                comm: comm.to_string(),
            });
        }
    }
    out
}

/// Processes other than `own_pid` that hold `node` open (see
/// [`holders_in`]).
pub fn foreign_holders(node: &Path, own_pid: u32) -> Vec<Holder> {
    let target = canonical(node);
    let targets = HashSet::from([target.clone()]);
    holders_in(Path::new(PROC), &targets, own_pid)
        .remove(&target)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn take_pending_consumes_fresh_entry() {
        let now = Instant::now();
        let mut q = VecDeque::from([now]);
        assert!(take_pending(&mut q, now + Duration::from_millis(500)));
        assert!(q.is_empty());
        assert!(!take_pending(&mut q, now));
    }

    #[test]
    fn take_pending_drops_expired_entries() {
        let t0 = Instant::now();
        let mut q = VecDeque::from([t0, t0 + Duration::from_millis(100)]);
        // both older than OWN_TTL
        assert!(!take_pending(&mut q, t0 + Duration::from_millis(1200)));
        assert!(q.is_empty());
    }

    #[test]
    fn take_pending_skips_expired_but_takes_fresh() {
        let t0 = Instant::now();
        let fresh = t0 + Duration::from_millis(900);
        let mut q = VecDeque::from([t0, fresh, fresh]);
        assert!(take_pending(&mut q, t0 + Duration::from_millis(1500)));
        assert_eq!(q, VecDeque::from([fresh]));
    }

    #[test]
    fn take_pending_at_exact_ttl_still_counts() {
        let t0 = Instant::now();
        let mut q = VecDeque::from([t0]);
        assert!(take_pending(&mut q, t0 + OWN_TTL));
    }

    fn named(pid: u32, comm: &str) -> Holder {
        Holder::Named {
            pid,
            comm: comm.into(),
        }
    }

    #[test]
    fn close_on_free_node_keeps() {
        let keep = CloseAction::Keep { relabel: None };
        assert_eq!(close_action(None, &[], false), keep);
        assert_eq!(close_action(None, &[named(1, "a")], true), keep);
    }

    #[test]
    fn close_of_last_named_holder_releases_then_verifies() {
        let esptool = named(10, "esptool");
        for own in [false, true] {
            assert_eq!(
                close_action(Some(&esptool), &[], own),
                CloseAction::Release { verify: true }
            );
        }
    }

    #[test]
    fn close_with_unknown_holder() {
        let u = Holder::Unknown;
        // foreign close, nothing visible: release, nothing to verify
        assert_eq!(
            close_action(Some(&u), &[], false),
            CloseAction::Release { verify: false }
        );
        // our own close says nothing about the invisible holder
        assert_eq!(
            close_action(Some(&u), &[], true),
            CloseAction::Keep { relabel: None }
        );
    }

    #[test]
    fn close_with_remaining_holders_keeps_and_relabels() {
        let a = named(10, "esptool");
        let b = named(20, "avrdude");
        // reported holder still there: label unchanged
        assert_eq!(
            close_action(Some(&a), &[b.clone(), a.clone()], false),
            CloseAction::Keep { relabel: None }
        );
        // reported holder left, another remains: silent relabel
        assert_eq!(
            close_action(Some(&a), std::slice::from_ref(&b), false),
            CloseAction::Keep { relabel: Some(b) }
        );
    }

    fn proc_entry(root: &Path, pid: &str, comm: Option<&str>, links: &[&Path]) {
        let fd = root.join(pid).join("fd");
        std::fs::create_dir_all(&fd).unwrap();
        for (i, l) in links.iter().enumerate() {
            symlink(l, fd.join(i.to_string())).unwrap();
        }
        if let Some(c) = comm {
            std::fs::write(root.join(pid).join("comm"), c).unwrap();
        }
    }

    #[test]
    fn holders_in_matches_many_targets_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let acm0 = Path::new("/dev/ttyACM0");
        let usb0 = Path::new("/dev/ttyUSB0");
        let other = Path::new("/dev/null");
        proc_entry(root, "100", Some("esptool\n"), &[other, acm0, acm0]);
        proc_entry(root, "200", Some("avrdude\n"), &[usb0, acm0]);
        proc_entry(root, "300", None, &[acm0]); // exited mid-scan: no comm
        proc_entry(root, "301", Some("\n"), &[acm0]); // empty comm
        proc_entry(root, "400", Some("comrade\n"), &[acm0]); // own pid
        proc_entry(root, "500", Some("bash\n"), &[other]);
        std::fs::create_dir_all(root.join("600")).unwrap(); // no fd dir
        proc_entry(root, "self", Some("x\n"), &[acm0]); // not a pid

        let targets = HashSet::from([acm0.to_path_buf(), usb0.to_path_buf()]);
        let mut found = holders_in(root, &targets, 400);
        let mut acm = found.remove(acm0).unwrap();
        acm.sort_by_key(|h| match h {
            Holder::Named { pid, .. } => *pid,
            Holder::Unknown => 0,
        });
        assert_eq!(acm, vec![named(100, "esptool"), named(200, "avrdude")]);
        assert_eq!(found.remove(usb0).unwrap(), vec![named(200, "avrdude")]);
        assert!(found.is_empty());
    }

    #[test]
    fn holders_in_empty_targets_or_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        assert!(holders_in(dir.path(), &HashSet::new(), 0).is_empty());
        let targets = HashSet::from([PathBuf::from("/dev/ttyACM0")]);
        assert!(holders_in(&dir.path().join("nope"), &targets, 0).is_empty());
    }
}
