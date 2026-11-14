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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parity {
    None,
    Even,
    Odd,
}

/// Data bits, parity and stop bits of a serial line, such as `8N1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SerialFormat {
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: u8,
}

impl Default for SerialFormat {
    fn default() -> Self {
        Self::new(8, Parity::None, 1)
    }
}

impl SerialFormat {
    pub const fn new(data_bits: u8, parity: Parity, stop_bits: u8) -> Self {
        Self {
            data_bits,
            parity,
            stop_bits,
        }
    }

    /// Every format the picker offers: the usual ones first, then the rest.
    pub fn all() -> Vec<SerialFormat> {
        let common = ["8N1", "8E1", "8O1", "8N2", "7E1", "7O1", "7N1", "7E2"];
        let mut out: Vec<SerialFormat> = common
            .iter()
            .map(|s| s.parse().expect("valid format"))
            .collect();
        for d in (5..=8).rev() {
            for p in [Parity::None, Parity::Even, Parity::Odd] {
                for s in [1, 2] {
                    let f = SerialFormat::new(d, p, s);
                    if !out.contains(&f) {
                        out.push(f);
                    }
                }
            }
        }
        out
    }

    /// The next (or previous) format in `all()`, wrapping.
    pub fn step(self, forward: bool) -> SerialFormat {
        let all = Self::all();
        let i = all.iter().position(|f| *f == self).unwrap_or(0);
        let n = all.len();
        all[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

impl std::fmt::Display for SerialFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let p = match self.parity {
            Parity::None => 'N',
            Parity::Even => 'E',
            Parity::Odd => 'O',
        };
        write!(f, "{}{}{}", self.data_bits, p, self.stop_bits)
    }
}

impl std::str::FromStr for SerialFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let b = s.as_bytes();
        let bad = || format!("bad serial format {s:?}, expected for example 8N1");
        if b.len() != 3 {
            return Err(bad());
        }
        let data_bits = match b[0] {
            c @ b'5'..=b'8' => c - b'0',
            _ => return Err(bad()),
        };
        let parity = match b[1].to_ascii_uppercase() {
            b'N' => Parity::None,
            b'E' => Parity::Even,
            b'O' => Parity::Odd,
            _ => return Err(bad()),
        };
        let stop_bits = match b[2] {
            c @ (b'1' | b'2') => c - b'0',
            _ => return Err(bad()),
        };
        Ok(Self::new(data_bits, parity, stop_bits))
    }
}

impl TryFrom<String> for SerialFormat {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<SerialFormat> for String {
    fn from(f: SerialFormat) -> String {
        f.to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoardPrefs {
    pub baud: u32,
    pub line_ending: LineEnding,
    /// Missing in files written by older versions: 8N1.
    #[serde(default)]
    pub format: SerialFormat,
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
                format: SerialFormat::default(),
            },
        );
        m.insert(
            BoardKey("1a86:7523@pci-0000:00:14.0-usb-0:1:1.0".into()),
            BoardPrefs {
                baud: 9600,
                line_ending: LineEnding::None,
                format: SerialFormat::new(7, Parity::Even, 2),
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
    fn format_parses_displays_and_steps() {
        for s in ["8N1", "7E2", "5O1", "6N2"] {
            let f: SerialFormat = s.parse().unwrap();
            assert_eq!(f.to_string(), s);
        }
        assert_eq!(
            "8n1".parse::<SerialFormat>().unwrap(),
            SerialFormat::default()
        );
        for bad in ["", "9N1", "8X1", "8N3", "8N", "8N11"] {
            assert!(bad.parse::<SerialFormat>().is_err(), "{bad}");
        }
        let all = SerialFormat::all();
        assert_eq!(all.len(), 24);
        assert_eq!(all[0], SerialFormat::default());
        let f = SerialFormat::default();
        assert_eq!(f.step(true).step(false), f);
        assert_eq!(f.step(false), *all.last().unwrap());
    }

    #[test]
    fn old_prefs_without_format_load_as_8n1() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("boards.toml");
        std::fs::write(&p, "[\"a:b\"]\nbaud = 9600\nline_ending = \"Lf\"\n").unwrap();
        let m = load(&p);
        assert_eq!(m[&BoardKey("a:b".into())].format, SerialFormat::default());
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
        let p = state_path().expect("state dir resolvable (HOME set)");
        assert!(p.ends_with("comrade/boards.toml"), "{p:?}");
        assert!(p.is_absolute(), "{p:?}");
    }
}
