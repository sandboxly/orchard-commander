//! Straight (non-premultiplied) sRGB colors, as themes write them.

use std::fmt;

/// An sRGB color with straight alpha. Blending happens in sRGB space (like Qt
/// and CoreGraphics), so text weight matches the Python version.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    /// Fully transparent: a cell background that lets the frame background show.
    pub const TRANSPARENT: Rgba = Rgba::new(0, 0, 0, 0);
    pub const BLACK: Rgba = Rgba::rgb(0, 0, 0);
    pub const WHITE: Rgba = Rgba::rgb(255, 255, 255);

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Rgba { r, g, b, a }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Rgba { r, g, b, a: 255 }
    }

    /// `0xRRGGBB`, the way colors are usually written in code.
    pub const fn hex(rgb: u32) -> Self {
        Rgba::rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    }

    /// Parse `#rgb`, `#rrggbb` or `#rrggbbaa` (the `#` is optional) — the
    /// forms theme files use.
    pub fn parse(text: &str) -> Option<Self> {
        let s = text.trim().strip_prefix('#').unwrap_or(text.trim());
        if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
        match s.len() {
            3 => {
                let n = |i: usize| u8::from_str_radix(&s[i..i + 1], 16).ok().map(|v| v * 17);
                Some(Rgba::rgb(n(0)?, n(1)?, n(2)?))
            }
            6 => Some(Rgba::rgb(byte(0)?, byte(2)?, byte(4)?)),
            8 => Some(Rgba::new(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => None,
        }
    }

    pub const fn with_alpha(self, a: u8) -> Self {
        Rgba { a, ..self }
    }

    pub const fn is_transparent(self) -> bool {
        self.a == 0
    }

    /// `self` drawn over an opaque `below`, in sRGB space like the GPU blend.
    pub fn over(self, below: Rgba) -> Rgba {
        let a = self.a as u32;
        let mix = |f: u8, b: u8| ((f as u32 * a + b as u32 * (255 - a) + 127) / 255) as u8;
        Rgba::rgb(
            mix(self.r, below.r),
            mix(self.g, below.g),
            mix(self.b, below.b),
        )
    }
}

impl fmt::Debug for Rgba {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "#{:02x}{:02x}{:02x}{:02x}",
            self.r, self.g, self.b, self.a
        )
    }
}

impl From<[u8; 4]> for Rgba {
    fn from(v: [u8; 4]) -> Self {
        Rgba::new(v[0], v[1], v[2], v[3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_theme_notation() {
        assert_eq!(Rgba::parse("#ffc107"), Some(Rgba::rgb(255, 193, 7)));
        assert_eq!(Rgba::parse("fff"), Some(Rgba::WHITE));
        assert_eq!(Rgba::parse("#00000080"), Some(Rgba::new(0, 0, 0, 128)));
        assert_eq!(Rgba::parse("#12345"), None);
        assert_eq!(Rgba::parse("#gg0000"), None);
        assert_eq!(Rgba::hex(0x102030), Rgba::rgb(0x10, 0x20, 0x30));
    }

    #[test]
    fn over_blends_in_srgb_space() {
        assert_eq!(
            Rgba::new(255, 255, 255, 128).over(Rgba::BLACK),
            Rgba::rgb(128, 128, 128)
        );
        assert_eq!(Rgba::TRANSPARENT.over(Rgba::WHITE), Rgba::WHITE);
    }
}
