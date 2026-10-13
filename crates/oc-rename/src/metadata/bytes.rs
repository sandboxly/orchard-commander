//! Bounds-checked helpers for the hand-written header parsers: a byte cursor
//! over a buffer (every read is `None` past the end instead of a panic), a
//! bit reader for codec headers and a bounded positioned read.

use std::io::{self, Read, SeekFrom};

use super::ReadSeek;

/// Up to `len` bytes from `offset`; fewer at the end of the file.
pub(crate) fn read_at(r: &mut dyn ReadSeek, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(offset))?;
    let mut out = Vec::with_capacity(len.min(1 << 20));
    r.take(len as u64).read_to_end(&mut out)?;
    Ok(out)
}

/// A little reader over a byte slice.
#[derive(Clone, Copy)]
pub(crate) struct Cur<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Cur { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    pub fn u8(&mut self) -> Option<u8> {
        self.array::<1>().map(|b| b[0])
    }

    pub fn u16_le(&mut self) -> Option<u16> {
        self.array().map(u16::from_le_bytes)
    }

    pub fn u32_le(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }

    pub fn u64_le(&mut self) -> Option<u64> {
        self.array().map(u64::from_le_bytes)
    }

    pub fn u16_be(&mut self) -> Option<u16> {
        self.array().map(u16::from_be_bytes)
    }

    pub fn u24_be(&mut self) -> Option<u32> {
        self.array::<3>().map(|b| u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }

    pub fn u32_be(&mut self) -> Option<u32> {
        self.array().map(u32::from_be_bytes)
    }

    pub fn f64_be(&mut self) -> Option<f64> {
        self.array().map(f64::from_be_bytes)
    }
}

/// UTF-16LE text as mutagen decodes it, NULs stripped at both ends (lossy
/// where mutagen would fail on the whole file).
pub(crate) fn utf16le(data: &[u8]) -> String {
    let units: Vec<u16> = data.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
    String::from_utf16_lossy(&units).trim_matches('\0').to_string()
}

/// Big-endian bits, e.g. of a video sequence header; `None` past the end.
pub(crate) struct Bits<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Bits { data, bit: 0 }
    }

    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.bit / 8)?;
        let value = (byte >> (7 - self.bit % 8)) & 1;
        self.bit += 1;
        Some(value as u32)
    }

    /// Up to 32 bits.
    pub fn bits(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        (0..n).try_fold(0u32, |acc, _| Some((acc << 1) | self.bit()?))
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        let end = self.bit.checked_add(n)?;
        if end > self.data.len() * 8 {
            return None;
        }
        self.bit = end;
        Some(())
    }

    /// Exp-Golomb unsigned (H.264 / HEVC `ue(v)`).
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)? as u64;
        u32::try_from((1u64 << zeros) - 1 + rest).ok()
    }

    /// Exp-Golomb signed (`se(v)`).
    pub fn se(&mut self) -> Option<i64> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `(value, width in bits)` fields packed into the low bits of a number, first field highest.
    pub(crate) fn pack(fields: &[(u64, u32)]) -> u64 {
        fields.iter().fold(0, |acc, &(value, width)| (acc << width) | value)
    }

    #[test]
    fn cursor_bounds() {
        let mut c = Cur::new(&[1, 2, 3]);
        assert_eq!(c.u16_le(), Some(0x0201));
        assert_eq!(c.u16_le(), None);
        assert_eq!(c.u8(), Some(3));
        assert_eq!(c.u8(), None);
        assert_eq!(c.take(usize::MAX), None);
        assert_eq!(Cur::new(&[0, 0, 1]).u24_be(), Some(1));
    }

    #[test]
    fn exp_golomb() {
        // 1 | 010 | 011 | 00100 | 00101 -> 0, 1, 2, 3, 4
        let data = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut b = Bits::new(&data);
        assert_eq!((b.ue(), b.ue(), b.ue(), b.ue(), b.ue()), (Some(0), Some(1), Some(2), Some(3), Some(4)));
        let mut b = Bits::new(&[0b0100_1100]);
        assert_eq!((b.se(), b.se()), (Some(1), Some(-1)));
        assert_eq!(Bits::new(&[0, 0, 0, 0, 0]).ue(), None); // too many leading zeros
        assert_eq!(Bits::new(&[]).bit(), None);
    }

    #[test]
    fn utf16_text() {
        assert_eq!(utf16le(b"A\0b\0\0\0"), "Ab");
        assert_eq!(utf16le(b"A\0b"), "A"); // odd byte dropped
    }
}
