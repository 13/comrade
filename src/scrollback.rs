//! Ring-buffer scrollback with follow/freeze, wrapping and search.
//!
//! Scroll units are logical lines. The frozen position is an absolute line
//! number (`dropped + index`), so it stays valid while lines are appended or
//! the ring drops old lines. Indexes in the public API (`search`, `jump_to`,
//! `get`) are positions in the current buffer, 0 = oldest retained line.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::Range;

use unicode_width::UnicodeWidthChar;

use crate::lines::Line;

pub struct VisibleRow<'a> {
    pub line: &'a Line,
    /// Wrapped segment index within the line (0 when not wrapping).
    pub segment: usize,
    pub is_partial: bool,
}

pub struct Scrollback {
    lines: VecDeque<Line>,
    cap: usize,
    dropped: u64,
    follow: bool,
    /// Absolute index of the bottom-most visible line while frozen.
    anchor: u64,
    new_since_freeze: usize,
}

/// Remove SGR (and any other CSI) sequences.
pub fn strip_sgr(s: &str) -> Cow<'_, str> {
    if !s.contains('\x1b') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            it.next();
            for d in it.by_ref() {
                if ('\x40'..='\x7e').contains(&d) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Rows a line occupies at `width` columns (greedy char wrap, min 1).
/// `text` must be SGR-stripped.
pub fn row_count(text: &str, width: usize) -> usize {
    let width = width.max(1);
    let (mut rows, mut cur) = (1, 0);
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if cur + w > width && cur > 0 {
            rows += 1;
            cur = 0;
        }
        cur += w;
    }
    rows
}

/// Byte ranges of `text` (SGR-stripped) for each wrapped row at `width`
/// columns. Same greedy rule as `row_count`, so lengths always agree.
pub fn wrap_segments(text: &str, width: u16) -> Vec<Range<usize>> {
    let width = (width as usize).max(1);
    let (mut out, mut start, mut cur) = (Vec::new(), 0, 0);
    for (i, c) in text.char_indices() {
        let w = c.width().unwrap_or(0);
        if cur + w > width && cur > 0 {
            out.push(start..i);
            start = i;
            cur = 0;
        }
        cur += w;
    }
    out.push(start..text.len());
    out
}

impl Scrollback {
    pub fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            cap: cap.max(1),
            dropped: 0,
            follow: true,
            anchor: 0,
            new_since_freeze: 0,
        }
    }

    pub fn push(&mut self, line: Line) {
        if self.lines.len() == self.cap {
            self.lines.pop_front();
            self.dropped += 1;
            self.anchor = self.anchor.max(self.dropped);
        }
        self.lines.push_back(line);
        if !self.follow {
            self.new_since_freeze += 1;
        }
    }

    pub fn clear(&mut self) {
        self.dropped += self.lines.len() as u64;
        self.lines.clear();
        self.follow = true;
        self.anchor = self.dropped;
        self.new_since_freeze = 0;
    }

    /// Lines pushed since creation, including those dropped or cleared.
    pub fn total(&self) -> u64 {
        self.dropped + self.lines.len() as u64
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn get(&self, idx: usize) -> Option<&Line> {
        self.lines.get(idx)
    }

    pub fn follow(&self) -> bool {
        self.follow
    }

    pub fn new_since_freeze(&self) -> usize {
        self.new_since_freeze
    }

    /// Buffer index of the bottom-most visible line (last line when following).
    pub fn position(&self) -> usize {
        if self.follow {
            self.lines.len().saturating_sub(1)
        } else {
            (self.anchor.saturating_sub(self.dropped) as usize)
                .min(self.lines.len().saturating_sub(1))
        }
    }

    fn last_abs(&self) -> u64 {
        (self.dropped + self.lines.len() as u64).saturating_sub(1)
    }

    pub fn scroll_up(&mut self, n: usize) {
        if self.lines.is_empty() {
            return;
        }
        let base = if self.follow {
            self.follow = false;
            self.new_since_freeze = 0;
            self.last_abs()
        } else {
            self.anchor
        };
        self.anchor = base.saturating_sub(n as u64).max(self.dropped);
    }

    pub fn scroll_down(&mut self, n: usize) {
        if self.follow {
            return;
        }
        let next = self.anchor.saturating_add(n as u64);
        if next >= self.last_abs() {
            self.end();
        } else {
            self.anchor = next;
        }
    }

    pub fn home(&mut self) {
        if self.lines.is_empty() {
            return;
        }
        if self.follow {
            self.follow = false;
            self.new_since_freeze = 0;
        }
        self.anchor = self.dropped;
    }

    pub fn end(&mut self) {
        self.follow = true;
        self.new_since_freeze = 0;
    }

    /// Freeze the view with line `idx` (buffer position) as the bottom line.
    pub fn jump_to(&mut self, idx: usize) {
        if self.lines.is_empty() {
            return;
        }
        if self.follow {
            self.follow = false;
            self.new_since_freeze = 0;
        }
        let idx = idx.min(self.lines.len() - 1);
        self.anchor = self.dropped + idx as u64;
    }

    pub fn visible<'a>(
        &'a self,
        rows: u16,
        cols: u16,
        wrap: bool,
        partial: Option<&'a Line>,
        ts_width: u16,
    ) -> Vec<VisibleRow<'a>> {
        let rows = rows as usize;
        if rows == 0 {
            return Vec::new();
        }
        let tw = (cols.saturating_sub(ts_width) as usize).max(1);
        let height = |l: &Line| {
            if wrap {
                row_count(&strip_sgr(&l.text), tw)
            } else {
                1
            }
        };

        // Collected bottom-up, reversed at the end.
        let mut out: Vec<VisibleRow<'a>> = Vec::with_capacity(rows);
        let add_up = |out: &mut Vec<VisibleRow<'a>>, line: &'a Line, is_partial: bool| {
            for segment in (0..height(line)).rev() {
                if out.len() == rows {
                    return;
                }
                out.push(VisibleRow {
                    line,
                    segment,
                    is_partial,
                });
            }
        };

        let (bottom, below_start) = if self.follow {
            if let Some(p) = partial {
                add_up(&mut out, p, true);
            }
            (self.lines.len(), self.lines.len())
        } else {
            let a = (self.anchor.saturating_sub(self.dropped) as usize)
                .min(self.lines.len().saturating_sub(1));
            (a + 1, a + 1)
        };
        for line in self.lines.range(..bottom).rev() {
            if out.len() == rows {
                break;
            }
            add_up(&mut out, line, false);
        }
        out.reverse();

        if !self.follow {
            for line in self.lines.range(below_start..) {
                for segment in 0..height(line) {
                    if out.len() == rows {
                        return out;
                    }
                    out.push(VisibleRow {
                        line,
                        segment,
                        is_partial: false,
                    });
                }
            }
        }
        out
    }

    /// Case-insensitive search over SGR-stripped text. `from` is inclusive;
    /// no wrap-around. Returns a buffer index.
    pub fn search(&self, needle: &str, from: usize, forward: bool) -> Option<usize> {
        if needle.is_empty() || self.lines.is_empty() {
            return None;
        }
        let needle = needle.to_lowercase();
        let hit = |i: usize| {
            strip_sgr(&self.lines[i].text)
                .to_lowercase()
                .contains(&needle)
        };
        if forward {
            (from..self.lines.len()).find(|&i| hit(i))
        } else {
            let start = from.min(self.lines.len() - 1);
            (0..=start).rev().find(|&i| hit(i))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lines::LineKind;
    use chrono::Local;

    fn l(t: &str) -> Line {
        Line {
            at: Local::now(),
            kind: LineKind::Data,
            text: t.to_string(),
        }
    }
    fn filled(n: usize) -> Scrollback {
        let mut s = Scrollback::new(100);
        for i in 0..n {
            s.push(l(&i.to_string()));
        }
        s
    }
    #[test]
    fn wrap_segments_matches_row_count() {
        for t in [
            "",
            "abc",
            "abcdefghij",
            "日本語テキスト",
            "a日b日c",
            "x\u{0301}yz",
        ] {
            for w in 0u16..12 {
                let segs = wrap_segments(t, w);
                assert_eq!(segs.len(), row_count(t, w as usize), "{t:?} w={w}");
                assert_eq!(segs.first().map(|r| r.start), Some(0));
                assert_eq!(segs.last().map(|r| r.end), Some(t.len()));
                for p in segs.windows(2) {
                    assert_eq!(p[0].end, p[1].start);
                }
            }
        }
        assert_eq!(wrap_segments("abcd", 2), vec![0..2, 2..4]);
    }

    #[test]
    fn position_tracks_anchor() {
        let mut s = filled(10);
        assert_eq!(s.position(), 9);
        s.jump_to(4);
        assert_eq!(s.position(), 4);
    }

    fn texts(v: &[VisibleRow]) -> Vec<String> {
        v.iter().map(|r| r.line.text.clone()).collect()
    }

    #[test]
    fn ring_drops_oldest() {
        let mut s = Scrollback::new(3);
        for i in 0..5 {
            s.push(l(&i.to_string()));
        }
        assert_eq!(s.len(), 3);
        assert_eq!(s.get(0).unwrap().text, "2");
    }

    #[test]
    fn follow_shows_tail() {
        let s = filled(10);
        let v = s.visible(3, 80, true, None, 0);
        assert_eq!(texts(&v), ["7", "8", "9"]);
        let p = l("par");
        let v = s.visible(3, 80, true, Some(&p), 0);
        assert_eq!(texts(&v), ["8", "9", "par"]);
        assert!(v[2].is_partial);
    }

    #[test]
    fn scroll_up_freezes_and_counts_new() {
        let mut s = filled(10);
        s.scroll_up(2);
        assert!(!s.follow());
        let before = texts(&s.visible(3, 80, true, None, 0));
        assert_eq!(before, ["5", "6", "7"]);
        for i in 0..4 {
            s.push(l(&format!("n{i}")));
        }
        assert_eq!(s.new_since_freeze(), 4);
        assert_eq!(texts(&s.visible(3, 80, true, None, 0)), before);
    }

    #[test]
    fn end_resumes_follow() {
        let mut s = filled(10);
        s.scroll_up(2);
        s.push(l("x"));
        s.end();
        assert!(s.follow());
        assert_eq!(s.new_since_freeze(), 0);
        assert_eq!(texts(&s.visible(1, 80, true, None, 0)), ["x"]);
    }

    #[test]
    fn scroll_down_to_bottom_resumes() {
        let mut s = filled(10);
        s.scroll_up(3);
        s.scroll_down(1);
        assert!(!s.follow());
        s.scroll_down(5);
        assert!(s.follow());
    }

    #[test]
    fn home_shows_top_filled() {
        let mut s = filled(10);
        s.home();
        assert_eq!(texts(&s.visible(3, 80, true, None, 0)), ["0", "1", "2"]);
    }

    #[test]
    fn wrap_counts_rows() {
        let mut s = Scrollback::new(10);
        s.push(l(&"a".repeat(25)));
        let v = s.visible(10, 10, true, None, 0);
        assert_eq!(v.len(), 3);
        assert_eq!(v.iter().map(|r| r.segment).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(s.visible(10, 10, false, None, 0).len(), 1);
        // ts_width reduces text width: 25 chars at 5 cols = 5 rows
        assert_eq!(s.visible(10, 10, true, None, 5).len(), 5);
    }

    #[test]
    fn wrap_ignores_sgr() {
        let mut s = Scrollback::new(10);
        s.push(l("\x1b[31mabcde\x1b[0m"));
        assert_eq!(s.visible(10, 5, true, None, 0).len(), 1);
    }

    #[test]
    fn visible_never_exceeds_rows() {
        let mut s = Scrollback::new(100);
        for _ in 0..20 {
            s.push(l(&"a".repeat(37)));
        }
        for rows in 1..12 {
            assert_eq!(s.visible(rows, 10, true, None, 0).len(), rows as usize);
            s.scroll_up(3);
            assert_eq!(s.visible(rows, 10, true, None, 0).len(), rows as usize);
            s.end();
        }
    }

    #[test]
    fn zero_rows_returns_empty() {
        let s = filled(5);
        assert!(s.visible(0, 80, true, None, 0).is_empty());
    }

    #[test]
    fn search_finds_case_insensitive_skipping_sgr() {
        let mut s = Scrollback::new(10);
        s.push(l("hello"));
        s.push(l("\x1b[31mWo\x1b[0mRLD"));
        s.push(l("world again"));
        assert_eq!(s.search("world", 0, true), Some(1));
        assert_eq!(s.search("world", 2, true), Some(2));
        assert_eq!(s.search("world", 1, false), Some(1));
        assert_eq!(s.search("hello", 2, false), Some(0));
        assert_eq!(s.search("zzz", 0, true), None);
        assert_eq!(s.search("", 0, true), None);
    }

    #[test]
    fn frozen_index_survives_ring_drop() {
        let mut s = Scrollback::new(5);
        for i in 0..5 {
            s.push(l(&i.to_string()));
        }
        s.scroll_up(1); // anchor on "3"
        s.push(l("5")); // drops "0"
        s.push(l("6")); // drops "1"
        assert_eq!(texts(&s.visible(1, 80, true, None, 0)), ["3"]);
        for i in 7..12 {
            s.push(l(&i.to_string())); // drops "3" too: clamp to oldest
        }
        assert_eq!(
            texts(&s.visible(1, 80, true, None, 0)),
            [s.get(0).unwrap().text.clone()]
        );
    }

    #[test]
    fn jump_to_freezes() {
        let mut s = filled(10);
        s.jump_to(4);
        assert!(!s.follow());
        let v = s.visible(1, 80, true, None, 0);
        assert_eq!(texts(&v), ["4"]);
    }

    #[test]
    fn strip_sgr_works() {
        assert_eq!(strip_sgr("a\x1b[1;31mb\x1b[0mc"), "abc");
        assert!(matches!(strip_sgr("plain"), Cow::Borrowed(_)));
    }

    #[test]
    fn wide_chars_wrap_by_display_width() {
        let mut s = Scrollback::new(10);
        s.push(l("\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}"));
        // 5 wide chars (width 10) at 4 cols: 2 chars per row -> 3 rows
        assert_eq!(s.visible(10, 4, true, None, 0).len(), 3);
    }

    #[test]
    fn wide_char_wider_than_width_terminates() {
        assert_eq!(row_count("\u{4e2d}", 1), 1);
        assert_eq!(row_count("\u{4e2d}\u{4e2d}", 1), 2);
        let mut s = Scrollback::new(10);
        s.push(l("\u{4e2d}\u{4e2d}"));
        assert_eq!(s.visible(10, 1, true, None, 0).len(), 2);
        assert_eq!(s.visible(10, 0, true, None, 0).len(), 2);
    }

    #[test]
    fn frozen_visible_ignores_partial() {
        let mut s = filled(10);
        s.scroll_up(2);
        let p = l("par");
        let v = s.visible(3, 80, true, Some(&p), 0);
        assert_eq!(texts(&v), ["5", "6", "7"]);
        assert!(v.iter().all(|r| !r.is_partial));
    }

    fn wrapped() -> Scrollback {
        let mut s = Scrollback::new(10);
        s.push(l(&"a".repeat(25))); // 3 rows at cols 10
        s.push(l("bbbbb"));
        s.push(l("ccccc"));
        s
    }

    #[test]
    fn frozen_wrapped_clips_top_to_bottom_segments() {
        let mut s = wrapped();
        s.scroll_up(1); // anchor on "bbbbb"
        let v = s.visible(3, 10, true, None, 0);
        assert_eq!(texts(&v), [&"a".repeat(25), &"a".repeat(25), "bbbbb"]);
        assert_eq!(v.iter().map(|r| r.segment).collect::<Vec<_>>(), [1, 2, 0]);
    }

    #[test]
    fn frozen_fills_from_below_anchor() {
        let mut s = wrapped();
        s.home();
        let v = s.visible(5, 10, true, None, 0);
        assert_eq!(
            v.iter().map(|r| r.segment).collect::<Vec<_>>(),
            [0, 1, 2, 0, 0]
        );
        assert_eq!(v[3].line.text, "bbbbb");
        assert_eq!(v[4].line.text, "ccccc");
    }

    #[test]
    fn clear_resets() {
        let mut s = filled(5);
        s.scroll_up(1);
        s.push(l("x"));
        s.clear();
        assert_eq!(s.len(), 0);
        assert!(s.follow());
        assert_eq!(s.new_since_freeze(), 0);
        assert!(s.visible(3, 80, true, None, 0).is_empty());
        s.push(l("y"));
        assert_eq!(texts(&s.visible(3, 80, true, None, 0)), ["y"]);
    }

    #[test]
    fn scroll_up_after_end_resets_new_count() {
        let mut s = filled(10);
        s.scroll_up(1);
        s.push(l("a"));
        s.push(l("b"));
        assert_eq!(s.new_since_freeze(), 2);
        s.end();
        s.scroll_up(1);
        assert_eq!(s.new_since_freeze(), 0);
        s.push(l("c"));
        assert_eq!(s.new_since_freeze(), 1);
    }

    #[test]
    fn scroll_down_exactly_to_last_resumes() {
        let mut s = filled(10);
        s.scroll_up(3); // anchor 6, last 9
        s.scroll_down(2); // 8
        assert!(!s.follow());
        s.scroll_down(1); // 9 == last
        assert!(s.follow());
        assert_eq!(s.new_since_freeze(), 0);
    }
}
