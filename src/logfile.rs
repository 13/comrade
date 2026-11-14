//! `--log-dir`: appends every tab's lines to a file as they arrive.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

use crate::board::BoardKey;
use crate::core::Tab;
use crate::scrollback::strip_sgr;

struct Sink {
    file: File,
    /// Lines pushed to the tab's scrollback (`Scrollback::total`) that are already in the file.
    written: u64,
}

pub struct LogSink {
    dir: PathBuf,
    sinks: HashMap<BoardKey, Sink>,
}

impl LogSink {
    pub fn new(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            sinks: HashMap::new(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes the lines that are new since the last call. Each write goes
    /// straight to the file, so a kill loses nothing that was already synced.
    pub fn sync(&mut self, tabs: &[Tab], now: DateTime<Local>) -> std::io::Result<()> {
        self.sinks.retain(|k, _| tabs.iter().any(|t| &t.key == k));
        for t in tabs {
            let total = t.scroll.total();
            if !self.sinks.contains_key(&t.key) {
                if total == 0 {
                    continue;
                }
                let file = create(&self.dir, &t.name, now)?;
                // Lines already in the scrollback when the file opens are written too.
                let written = total - t.scroll.len() as u64;
                self.sinks.insert(t.key.clone(), Sink { file, written });
            }
            let s = self.sinks.get_mut(&t.key).expect("inserted above");
            if s.written >= total {
                s.written = total;
                continue;
            }
            // Lines pushed out of the scrollback before they were synced are lost.
            let first = s.written.max(total - t.scroll.len() as u64);
            let mut out = String::new();
            for abs in first..total {
                let idx = (abs - (total - t.scroll.len() as u64)) as usize;
                if let Some(l) = t.scroll.get(idx) {
                    out.push_str(&l.at.format("%Y-%m-%d %H:%M:%S%.3f ").to_string());
                    out.push_str(&strip_sgr(&l.text));
                    out.push('\n');
                }
            }
            s.written = total;
            s.file.write_all(out.as_bytes())?;
        }
        Ok(())
    }
}

fn create(dir: &Path, name: &str, now: DateTime<Local>) -> std::io::Result<File> {
    let base = format!(
        "comrade-{}-{}",
        crate::app::sanitize_name(name),
        now.format("%Y%m%d-%H%M%S")
    );
    for n in 0.. {
        let file = if n == 0 {
            format!("{base}.log")
        } else {
            format!("{base}-{n}.log")
        };
        match OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(dir.join(file))
        {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            r => return r,
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Core;
    use crate::event::Now;
    use crate::lines::{Line, LineKind};
    use std::time::Instant;

    fn tab_core() -> Core {
        Core::new(
            Default::default(),
            115_200,
            vec!["/dev/null-comrade-test".into()],
        )
    }

    fn push(c: &mut Core, text: &str) {
        c.tabs_mut()[0].scroll.push(Line {
            at: Local::now(),
            kind: LineKind::Data,
            text: text.into(),
        });
    }

    #[test]
    fn appends_only_new_lines_and_survives_clear() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = tab_core();
        let mut log = LogSink::new(dir.path().to_path_buf()).unwrap();
        c.startup(Now {
            mono: Instant::now(),
            wall: Local::now(),
        });
        push(&mut c, "\x1b[31mone\x1b[0m");
        log.sync(c.tabs(), Local::now()).unwrap();
        push(&mut c, "two");
        log.sync(c.tabs(), Local::now()).unwrap();
        log.sync(c.tabs(), Local::now()).unwrap();
        c.tabs_mut()[0].scroll.clear();
        push(&mut c, "three");
        log.sync(c.tabs(), Local::now()).unwrap();
        let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(files.len(), 1);
        let text = std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let tail: Vec<&str> = lines.iter().map(|l| l.split_once(' ').unwrap().1).collect();
        assert!(tail[0].ends_with("one"), "{text}");
        assert!(tail.iter().any(|l| l.ends_with("two")));
        assert!(tail.last().unwrap().ends_with("three"));
        assert_eq!(tail.iter().filter(|l| l.ends_with("one")).count(), 1);
    }
}
