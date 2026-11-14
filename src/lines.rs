//! Byte stream to display lines: line breaking, UTF-8 decoding, ANSI filtering.

use chrono::{DateTime, Local};

pub const MAX_LINE_BYTES: usize = 16_384;

#[derive(Clone, Debug, PartialEq)]
pub enum LineKind {
    Data,
    Separator,
    Info,
    Error,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub at: DateTime<Local>,
    pub kind: LineKind,
    /// For `Data` lines: plain text plus SGR sequences only.
    pub text: String,
}

#[derive(Debug, Default)]
pub struct Pushed {
    pub complete: Vec<Line>,
    pub partial_changed: bool,
    pub rom_banner: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Normal,
    SawCr,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

pub struct LineAssembler {
    state: State,
    cur: Option<Line>,
    csi: Vec<u8>,
    utf8: Vec<u8>,
}

impl Default for LineAssembler {
    fn default() -> Self {
        Self::new()
    }
}

fn utf8_len(lead: u8) -> usize {
    match lead {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 0,
    }
}

fn strip_sgr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("\x1b[") {
        out.push_str(&rest[..i]);
        match rest[i..].find('m') {
            Some(j) => rest = &rest[i + j + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

impl LineAssembler {
    pub fn new() -> Self {
        Self {
            state: State::Normal,
            cur: None,
            csi: Vec::new(),
            utf8: Vec::new(),
        }
    }

    pub fn push(&mut self, bytes: &[u8], at: DateTime<Local>) -> Pushed {
        let mut p = Pushed::default();
        for &b in bytes {
            self.feed(b, at, &mut p);
        }
        p
    }

    /// Current unterminated line, if it has visible text.
    pub fn partial(&self) -> Option<&Line> {
        self.cur.as_ref().filter(|l| !l.text.is_empty())
    }

    /// Emit whatever is pending (used on disconnect).
    pub fn flush(&mut self) -> Option<Line> {
        if !self.utf8.is_empty() {
            self.utf8.clear();
            let at = self.cur.as_ref().map(|l| l.at);
            if let Some(at) = at {
                let mut p = Pushed::default();
                self.append("\u{FFFD}", at, &mut p);
            }
        }
        self.state = State::Normal;
        self.csi.clear();
        self.cur.take().filter(|l| !l.text.is_empty())
    }

    fn touch(&mut self, at: DateTime<Local>) {
        if self.cur.is_none() {
            self.cur = Some(Line {
                at,
                kind: LineKind::Data,
                text: String::new(),
            });
        }
    }

    fn finish(&mut self, p: &mut Pushed) {
        if let Some(line) = self.cur.take() {
            let plain = strip_sgr(&line.text);
            if plain.starts_with("ESP-ROM:") || plain.starts_with("rst:0x") {
                p.rom_banner = true;
            }
            p.complete.push(line);
            p.partial_changed = true;
        }
    }

    fn append(&mut self, s: &str, at: DateTime<Local>, p: &mut Pushed) {
        self.touch(at);
        if self
            .cur
            .as_ref()
            .is_some_and(|l| l.text.len() + s.len() > MAX_LINE_BYTES)
        {
            self.finish(p);
            self.touch(at);
        }
        if let Some(l) = self.cur.as_mut() {
            l.text.push_str(s);
        }
        p.partial_changed = true;
    }

    fn feed(&mut self, b: u8, at: DateTime<Local>, p: &mut Pushed) {
        loop {
            match self.state {
                State::SawCr => {
                    self.state = State::Normal;
                    self.finish(p);
                    if b == b'\n' {
                        return;
                    }
                }
                State::Esc => {
                    self.state = match b {
                        b'[' => {
                            self.csi.clear();
                            State::Csi
                        }
                        b']' => State::Osc,
                        _ => State::Normal,
                    };
                    return;
                }
                State::Csi => match b {
                    0x20..=0x3F => {
                        if self.csi.len() < 64 {
                            self.csi.push(b);
                        } else {
                            self.state = State::Normal;
                        }
                        return;
                    }
                    0x40..=0x7E => {
                        self.state = State::Normal;
                        if b == b'm' && self.csi.iter().all(|c| c.is_ascii_digit() || *c == b';') {
                            let sgr = format!("\x1b[{}m", String::from_utf8_lossy(&self.csi));
                            self.append(&sgr, at, p);
                        }
                        return;
                    }
                    _ => self.state = State::Normal,
                },
                State::Osc => match b {
                    0x07 => {
                        self.state = State::Normal;
                        return;
                    }
                    0x1B => {
                        self.state = State::OscEsc;
                        return;
                    }
                    b'\n' | b'\r' => self.state = State::Normal,
                    _ => return,
                },
                State::OscEsc => {
                    if b == b'\\' {
                        self.state = State::Normal;
                        return;
                    }
                    self.state = State::Esc;
                }
                State::Normal => {
                    self.touch(at);
                    if !self.utf8.is_empty() {
                        if (0x80..=0xBF).contains(&b) {
                            self.utf8.push(b);
                            if self.utf8.len() == utf8_len(self.utf8[0]) {
                                let bytes = std::mem::take(&mut self.utf8);
                                let s = String::from_utf8_lossy(&bytes).into_owned();
                                self.append(&s, at, p);
                            }
                            return;
                        }
                        self.utf8.clear();
                        self.append("\u{FFFD}", at, p);
                    }
                    match b {
                        b'\n' => self.finish(p),
                        b'\r' => self.state = State::SawCr,
                        0x1B => self.state = State::Esc,
                        b'\t' => self.append("\t", at, p),
                        0x00..=0x1F => {
                            let c = char::from_u32(0x2400 + b as u32).unwrap_or('\u{FFFD}');
                            self.append(&c.to_string(), at, p);
                        }
                        0x7F => self.append("\u{2421}", at, p),
                        0x20..=0x7E => self.append(std::str::from_utf8(&[b]).unwrap_or(""), at, p),
                        _ => {
                            if utf8_len(b) > 0 {
                                self.utf8.push(b);
                            } else {
                                self.append("\u{FFFD}", at, p);
                            }
                        }
                    }
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 1, 1, 0, 0, s).unwrap()
    }

    fn texts(p: &Pushed) -> Vec<&str> {
        p.complete.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn crlf_split_across_chunks() {
        let mut a = LineAssembler::new();
        let mut all = a.push(b"a\r", t(0)).complete;
        all.extend(a.push(b"\nb\n", t(1)).complete);
        let v: Vec<_> = all.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(v, ["a", "b"]);
    }

    #[test]
    fn crlf_in_one_chunk() {
        let mut a = LineAssembler::new();
        assert_eq!(texts(&a.push(b"a\r\nb\r\n", t(0))), ["a", "b"]);
    }

    #[test]
    fn bare_cr_breaks() {
        let mut a = LineAssembler::new();
        assert_eq!(texts(&a.push(b"a\rb\n", t(0))), ["a", "b"]);
    }

    #[test]
    fn trailing_cr_keeps_partial_visible() {
        let mut a = LineAssembler::new();
        let p = a.push(b"abc\r", t(0));
        assert!(p.complete.is_empty());
        assert_eq!(a.partial().unwrap().text, "abc");
    }

    #[test]
    fn partial_visible_then_completed() {
        let mut a = LineAssembler::new();
        let p = a.push(b"hel", t(0));
        assert!(p.complete.is_empty());
        assert!(p.partial_changed);
        assert_eq!(a.partial().unwrap().text, "hel");
        let p = a.push(b"lo\n", t(1));
        assert_eq!(texts(&p), ["hello"]);
        assert!(a.partial().is_none());
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut a = LineAssembler::new();
        let e = "é".as_bytes();
        assert!(a.push(&e[..1], t(0)).complete.is_empty());
        let p = a.push(&[e[1], b'\n'], t(1));
        assert_eq!(texts(&p), ["é"]);
    }

    #[test]
    fn invalid_utf8_is_lossy() {
        let mut a = LineAssembler::new();
        assert_eq!(texts(&a.push(b"a\xffb\n", t(0))), ["a\u{FFFD}b"]);
    }

    #[test]
    fn keeps_sgr() {
        let mut a = LineAssembler::new();
        let s = "\x1b[0;32mI (1) x\x1b[0m";
        let p = a.push(format!("{s}\n").as_bytes(), t(0));
        assert_eq!(texts(&p), [s]);
    }

    #[test]
    fn strips_non_sgr_escapes() {
        let mut a = LineAssembler::new();
        let p = a.push(b"\x1b[2Jhi\x1b]0;title\x07\x1b[5;5H!\n", t(0));
        assert_eq!(texts(&p), ["hi!"]);
    }

    #[test]
    fn escape_split_across_chunks() {
        let mut a = LineAssembler::new();
        let mut all = Vec::new();
        for c in [
            &b"\x1b"[..],
            b"[0;3",
            b"2mhi\x1b",
            b"]0;ti",
            b"tle\x1b",
            b"\\x\n",
        ] {
            all.extend(a.push(c, t(0)).complete);
        }
        let v: Vec<_> = all.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(v, ["\x1b[0;32mhix"]);
    }

    #[test]
    fn control_pictures() {
        let mut a = LineAssembler::new();
        assert_eq!(texts(&a.push(b"a\0b\n", t(0))), ["a\u{2400}b"]);
        assert_eq!(texts(&a.push(b"a\x7fb\tc\n", t(0))), ["a\u{2421}b\tc"]);
    }

    #[test]
    fn long_line_is_split() {
        let mut a = LineAssembler::new();
        let p = a.push(&vec![b'x'; 20_000], t(0));
        assert_eq!(p.complete.len(), 1);
        assert_eq!(p.complete[0].text.len(), MAX_LINE_BYTES);
        assert_eq!(a.partial().unwrap().text.len(), 20_000 - MAX_LINE_BYTES);
    }

    #[test]
    fn rom_banner_detected() {
        let mut a = LineAssembler::new();
        assert!(a.push(b"ESP-ROM:esp32s3-20210327\n", t(0)).rom_banner);
        assert!(a.push(b"\x1b[0mrst:0x1 (POWERON)\n", t(0)).rom_banner);
        assert!(!a.push(b"hello\n", t(0)).rom_banner);
    }

    #[test]
    fn timestamp_is_first_byte() {
        let mut a = LineAssembler::new();
        a.push(b"ab", t(5));
        let p = a.push(b"c\n", t(9));
        assert_eq!(p.complete[0].at, t(5));
    }

    #[test]
    fn flush_returns_pending() {
        let mut a = LineAssembler::new();
        a.push(b"abc", t(0));
        assert_eq!(a.flush().unwrap().text, "abc");
        assert!(a.flush().is_none());
    }

    /// Splitting the input into reads anywhere must not change the lines:
    /// ANSI escapes, multi-byte UTF-8, lone `\r` and `\r\n` can all straddle reads.
    #[test]
    fn chunking_does_not_change_the_lines() {
        let at = Local.timestamp_opt(0, 0).unwrap();
        let pieces: [&[u8]; 12] = [
            b"abc",
            b"\n",
            b"\r",
            b"\r\n",
            "h\u{e9}llo \u{2713} \u{1f980}".as_bytes(),
            b"\x1b[31m",
            b"\x1b[0m",
            b"\x1b[2K",
            b"\x1b[1;32mgreen",
            &[0xe2, 0x9c],
            &[0xff, 0x00, 0x07],
            b"\t",
        ];
        let mut x: u32 = 99;
        let mut next = |n: usize| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 16) as usize % n
        };
        for _ in 0..300 {
            let mut input = Vec::new();
            for _ in 0..next(25) + 1 {
                input.extend_from_slice(pieces[next(pieces.len())]);
            }
            let run = |chunk: usize| {
                let mut a = LineAssembler::new();
                let mut out: Vec<(LineKind, String)> = Vec::new();
                for c in input.chunks(chunk) {
                    for l in a.push(c, at).complete {
                        out.push((l.kind, l.text));
                    }
                }
                let tail = a.flush().map(|l| (l.kind, l.text));
                (out, tail)
            };
            let whole = run(input.len().max(1));
            for chunk in [1, 2, 3, 5, 7] {
                assert_eq!(run(chunk), whole, "chunk {chunk}, input {input:?}");
            }
        }
    }
}
