//! Detects output that looks like a wrong baud rate: bytes that are not text.

/// Bytes per window. A window is judged once it is full.
const WINDOW: usize = 256;
/// Bad windows in a row before the hint fires.
const BAD_WINDOWS: u8 = 2;

#[derive(Debug, Default)]
pub struct BaudGuard {
    buf: Vec<u8>,
    bad_in_a_row: u8,
    fired: bool,
}

impl BaudGuard {
    /// Feed received bytes. Returns `true` once, when the data has looked like
    /// garbage for `BAD_WINDOWS` full windows in a row.
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        if self.fired {
            return false;
        }
        for &b in bytes {
            self.buf.push(b);
            if self.buf.len() < WINDOW {
                continue;
            }
            if looks_like_garbage(&self.buf) {
                self.bad_in_a_row += 1;
            } else {
                self.bad_in_a_row = 0;
            }
            self.buf.clear();
            if self.bad_in_a_row >= BAD_WINDOWS {
                self.fired = true;
                return true;
            }
        }
        false
    }

    /// Start over, for example after a reconnect or a baud change.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Text has few control bytes and is valid UTF-8 (a character cut off at the
/// end of the window does not count against it).
fn looks_like_garbage(w: &[u8]) -> bool {
    let control = w
        .iter()
        .filter(|&&b| (b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x1b)) || b == 0x7f)
        .count();
    if control * 10 > w.len() {
        return true;
    }
    match std::str::from_utf8(w) {
        Ok(_) => false,
        Err(e) => e.error_len().is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise, standing in for a wrong-baud capture.
    fn noise(n: usize) -> Vec<u8> {
        let mut x: u32 = 0x1234_5678;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn noise_fires_once() {
        let mut g = BaudGuard::default();
        assert!(!g.push(&noise(WINDOW)));
        assert!(g.push(&noise(WINDOW)));
        assert!(!g.push(&noise(WINDOW * 4)), "fires only once");
    }

    #[test]
    fn text_never_fires() {
        let mut g = BaudGuard::default();
        let text = "14:02:31 temperature 23.5 °C ✓\r\n".repeat(100);
        assert!(!g.push(text.as_bytes()));
    }

    #[test]
    fn text_between_noise_resets_the_count() {
        let mut g = BaudGuard::default();
        assert!(!g.push(&noise(WINDOW)));
        assert!(!g.push(&b"hello world\n".repeat(30)[..WINDOW]));
        assert!(!g.push(&noise(WINDOW)));
    }

    #[test]
    fn reset_allows_another_hint() {
        let mut g = BaudGuard::default();
        assert!(g.push(&noise(WINDOW * 2)));
        g.reset();
        assert!(g.push(&noise(WINDOW * 2)));
    }
}
