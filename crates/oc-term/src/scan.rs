//! A tiny side parser for what alacritty_terminal doesn't surface: OSC 7
//! (the shell's folder), the mouse modes it lacks (X10 `9`, urxvt `1015`),
//! and the legacy alternate screens `47`/`1047`.
//!
//! It only follows escape-sequence framing (ESC, CSI, OSC) so it stays in step
//! across reads that split a sequence; each event carries the offset just
//! past its sequence so the caller can apply order-sensitive ones (screen
//! switches) at the right point of the stream.

/// Longest OSC we keep: a path, percent-encoded. Longer ones are skipped.
const MAX_OSC: usize = 4096;
/// Enough for any private-mode list we care about.
const MAX_PARAMS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intercept {
    /// `ESC ] 7 ; url (BEL | ESC \)`.
    Osc7 { url: String, end: usize },
    /// `CSI ? n h` (set) / `CSI ? n l` (reset), one event per mode number.
    PrivateMode { mode: u16, set: bool, end: usize },
    /// `ESC c`: full reset.
    Reset { end: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    Csi,
    Osc,
    /// ESC seen inside an OSC: `\` ends it, anything else starts over.
    OscEscape,
}

#[derive(Debug)]
pub struct Scanner {
    state: State,
    /// CSI: parameter bytes after `?` (None = not a private-mode sequence).
    params: Vec<u8>,
    private: bool,
    /// CSI with intermediates / oversized params: not ours.
    ignore: bool,
    osc: Vec<u8>,
    osc_overflow: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Self {
            state: State::Ground,
            params: Vec::with_capacity(16),
            private: false,
            ignore: false,
            osc: Vec::new(),
            osc_overflow: false,
        }
    }
}

impl Scanner {
    /// Scan `data`, appending what it found to `out`.
    pub fn scan(&mut self, data: &[u8], out: &mut Vec<Intercept>) {
        let mut i = 0;
        while i < data.len() {
            if self.state == State::Ground {
                // Fast path: plain output has no ESC.
                match memchr_esc(&data[i..]) {
                    Some(pos) => {
                        i += pos + 1;
                        self.state = State::Escape;
                        continue;
                    }
                    None => return,
                }
            }
            let byte = data[i];
            i += 1;
            self.step(byte, i, out);
        }
    }

    fn step(&mut self, byte: u8, end: usize, out: &mut Vec<Intercept>) {
        match self.state {
            State::Ground => {
                if byte == 0x1b {
                    self.state = State::Escape;
                }
            }
            State::Escape => self.escape(byte, end, out),
            State::Csi => match byte {
                0x1b => self.state = State::Escape,
                b'?' if self.params.is_empty() && !self.private => self.private = true,
                b'0'..=b'9' | b';' => {
                    if self.params.len() < MAX_PARAMS {
                        self.params.push(byte);
                    } else {
                        self.ignore = true;
                    }
                }
                0x20..=0x2f | b'<'..=b'?' | b':' => self.ignore = true,
                0x40..=0x7e => {
                    if self.private && !self.ignore && (byte == b'h' || byte == b'l') {
                        for part in self.params.split(|&b| b == b';') {
                            if let Some(mode) = parse_u16(part) {
                                out.push(Intercept::PrivateMode {
                                    mode,
                                    set: byte == b'h',
                                    end,
                                });
                            }
                        }
                    }
                    self.state = State::Ground;
                }
                0x18 | 0x1a => self.state = State::Ground, // CAN / SUB abort
                _ => {}                                    // C0 controls execute mid-sequence
            },
            State::Osc => match byte {
                0x07 => self.finish_osc(end, out),
                0x1b => self.state = State::OscEscape,
                0x18 | 0x1a => self.state = State::Ground,
                _ => {
                    if self.osc.len() < MAX_OSC {
                        self.osc.push(byte);
                    } else {
                        self.osc_overflow = true;
                    }
                }
            },
            State::OscEscape => {
                if byte == b'\\' {
                    self.finish_osc(end, out);
                } else {
                    self.escape(byte, end, out); // the ESC began something new
                }
            }
        }
    }

    fn escape(&mut self, byte: u8, end: usize, out: &mut Vec<Intercept>) {
        self.state = match byte {
            b'[' => {
                self.params.clear();
                self.private = false;
                self.ignore = false;
                State::Csi
            }
            b']' => {
                self.osc.clear();
                self.osc_overflow = false;
                State::Osc
            }
            b'c' => {
                out.push(Intercept::Reset { end });
                State::Ground
            }
            0x1b => State::Escape,
            _ => State::Ground,
        };
    }

    fn finish_osc(&mut self, end: usize, out: &mut Vec<Intercept>) {
        self.state = State::Ground;
        if self.osc_overflow {
            return;
        }
        if let Some(url) = self.osc.strip_prefix(b"7;") {
            out.push(Intercept::Osc7 {
                url: String::from_utf8_lossy(url).into_owned(),
                end,
            });
        }
    }
}

fn memchr_esc(data: &[u8]) -> Option<usize> {
    data.iter().position(|&b| b == 0x1b)
}

fn parse_u16(digits: &[u8]) -> Option<u16> {
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u16, |acc, &d| {
        acc.checked_mul(10)?.checked_add(u16::from(d - b'0'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(chunks: &[&[u8]]) -> Vec<Intercept> {
        let mut s = Scanner::default();
        let mut out = Vec::new();
        for c in chunks {
            s.scan(c, &mut out);
        }
        out
    }

    #[test]
    fn osc7_with_bel_and_st_even_when_split() {
        let found = scan_all(&[
            b"pre\x1b]7;file://h/a\x07mid\x1b]7;file:",
            b"///b%20c\x1b",
            b"\\post",
        ]);
        assert_eq!(
            found,
            [
                Intercept::Osc7 {
                    url: "file://h/a".into(),
                    end: 18
                },
                Intercept::Osc7 {
                    url: "file:///b%20c".into(),
                    end: 1
                },
            ]
        );
    }

    #[test]
    fn other_oscs_are_ignored() {
        assert!(scan_all(&[b"\x1b]0;title\x07\x1b]2;x\x1b\\"]).is_empty());
        // an OSC cut short by ESC [ continues as a CSI
        assert_eq!(
            scan_all(&[b"\x1b]7;file:///x\x1b[?9h"]),
            [Intercept::PrivateMode {
                mode: 9,
                set: true,
                end: 18
            }]
        );
    }

    #[test]
    fn private_modes() {
        let found = scan_all(&[
            b"\x1b[?1000;1006h",
            b"x\x1b[?10",
            b"15l\x1b[1000h\x1b[>1h\x1bc",
        ]);
        assert_eq!(
            found,
            [
                Intercept::PrivateMode {
                    mode: 1000,
                    set: true,
                    end: 13
                },
                Intercept::PrivateMode {
                    mode: 1006,
                    set: true,
                    end: 13
                },
                Intercept::PrivateMode {
                    mode: 1015,
                    set: false,
                    end: 3
                },
                Intercept::Reset { end: 17 },
            ]
        );
    }

    #[test]
    fn oversized_osc_is_dropped() {
        let mut long = b"\x1b]7;file://".to_vec();
        long.extend(std::iter::repeat_n(b'a', MAX_OSC + 10));
        long.push(0x07);
        assert!(scan_all(&[&long]).is_empty());
        assert_eq!(parse_u16(b"70000"), None);
    }
}
