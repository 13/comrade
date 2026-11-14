//! Per-board preferences persisted to `$XDG_STATE_HOME/comrade/boards.toml`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::board::BoardKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineEnding {
    Lf,
    CrLf,
    None,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoardPrefs {
    pub baud: u32,
    pub line_ending: LineEnding,
}

/// `$XDG_STATE_HOME/comrade/boards.toml` (default `~/.local/state/comrade/boards.toml`).
pub fn state_path() -> Option<PathBuf> {
    let dirs = directories::BaseDirs::new()?;
    Some(dirs.state_dir()?.join("comrade").join("boards.toml"))
}

/// Read saved prefs. Never fails: a missing, unreadable or corrupt file yields an
/// empty map. Corruption is reported on stderr (called before the TUI starts).
pub fn load(path: &Path) -> HashMap<BoardKey, BoardPrefs> {
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    match toml::from_str::<BTreeMap<String, BoardPrefs>>(&text) {
        Ok(m) => m.into_iter().map(|(k, v)| (BoardKey(k), v)).collect(),
        Err(e) => {
            eprintln!(
                "comrade: ignoring corrupt {}: {}",
                path.display(),
                e.message()
            );
            HashMap::new()
        }
    }
}

/// Write prefs atomically: `boards.toml.tmp` in the same dir, then rename.
pub fn save(path: &Path, map: &HashMap<BoardKey, BoardPrefs>) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // TOML tables need string keys; BTreeMap also gives a stable order.
    let sorted: BTreeMap<&str, &BoardPrefs> = map.iter().map(|(k, v)| (k.0.as_str(), v)).collect();
    let text = toml::to_string(&sorted).map_err(io::Error::other)?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HashMap<BoardKey, BoardPrefs> {
        let mut m = HashMap::new();
        m.insert(
            BoardKey("303a:1001:AB:CD".into()),
            BoardPrefs {
                baud: 115_200,
                line_ending: LineEnding::CrLf,
            },
        );
        m.insert(
            BoardKey("1a86:7523@pci-0000:00:14.0-usb-0:1:1.0".into()),
            BoardPrefs {
                baud: 9600,
                line_ending: LineEnding::None,
            },
        );
        m
    }

    #[test]
    fn store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/comrade/boards.toml");
        let m = sample();
        save(&path, &m).unwrap();
        assert!(path.exists());
        assert!(!path.with_file_name("boards.toml.tmp").exists());
        assert_eq!(load(&path), m);
        // Overwrite in place.
        let mut m2 = m.clone();
        m2.remove(&BoardKey("303a:1001:AB:CD".into()));
        save(&path, &m2).unwrap();
        assert_eq!(load(&path), m2);
    }

    #[test]
    fn store_output_is_stably_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boards.toml");
        save(&path, &sample()).unwrap();
        let a = std::fs::read_to_string(&path).unwrap();
        save(&path, &sample()).unwrap();
        let b = std::fs::read_to_string(&path).unwrap();
        assert_eq!(a, b);
        assert!(a.find("1a86").unwrap() < a.find("303a").unwrap(), "{a}");
    }

    #[test]
    fn store_corrupt_file_is_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boards.toml");
        std::fs::write(&path, "this is = = not toml [[[").unwrap();
        assert!(load(&path).is_empty());
        std::fs::write(&path, "[\"k\"]\nbaud = \"fast\"\nline_ending = \"Lf\"\n").unwrap();
        assert!(load(&path).is_empty());
    }

    #[test]
    fn store_missing_file_is_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("nope.toml")).is_empty());
    }

    #[test]
    fn state_path_ends_with_comrade_boards() {
        if let Some(p) = state_path() {
            assert!(p.ends_with("comrade/boards.toml"), "{p:?}");
        }
    }
}
