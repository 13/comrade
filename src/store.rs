use serde::{Deserialize, Serialize};

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
