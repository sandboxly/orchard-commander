//! The small bitmap formats Qt reads and the `image` crate doesn't: WBMP,
//! XBM and XPM. Decoded to RGBA here, then shrunk like any other image.

use image::RgbaImage;

const BLACK: [u8; 4] = [0, 0, 0, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

/// Pixel count above which these are refused (they are icon formats).
const MAX_PIXELS: u64 = 64 * 1024 * 1024;

fn canvas(w: u64, h: u64) -> Result<RgbaImage, String> {
    if w == 0 || h == 0 || w.checked_mul(h).is_none_or(|n| n > MAX_PIXELS) {
        return Err(format!("invalid size {w}×{h}"));
    }
    Ok(RgbaImage::new(w as u32, h as u32))
}

/// Wireless bitmap (type 0): 1 bit per pixel, 1 = white, rows padded to bytes.
pub fn decode_wbmp(data: &[u8]) -> Result<RgbaImage, String> {
    let mut pos = 0;
    let mut int = || -> Result<u64, String> {
        let mut v: u64 = 0;
        loop {
            let b = *data.get(pos).ok_or("truncated WBMP header")?;
            pos += 1;
            v = (v << 7) | (b & 0x7f) as u64;
            if v > u32::MAX as u64 {
                return Err("invalid WBMP header".into());
            }
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
    };
    if int()? != 0 {
        return Err("unsupported WBMP type".into());
    }
    // The fixed header byte; extension headers (bit 7) aren't used by type 0.
    if int()? & 0x80 != 0 {
        return Err("unsupported WBMP header".into());
    }
    let (w, h) = (int()?, int()?);
    let mut img = canvas(w, h)?;
    let row = w.div_ceil(8) as usize;
    let bits = data.get(pos..).unwrap_or_default();
    if bits.len() < row * h as usize {
        return Err("truncated WBMP".into());
    }
    for (x, y, p) in img.enumerate_pixels_mut() {
        let byte = bits[y as usize * row + x as usize / 8];
        p.0 = if byte & (0x80 >> (x % 8)) != 0 { WHITE } else { BLACK };
    }
    Ok(img)
}

/// X bitmap: C source with `#define name_width/height` and a byte array,
/// least significant bit first, 1 = foreground (black) on white.
pub fn decode_xbm(data: &[u8]) -> Result<RgbaImage, String> {
    let text = std::str::from_utf8(data).map_err(|_| "not an XBM file")?;
    let define = |suffix: &str| -> Option<u64> {
        text.lines().find_map(|l| {
            let mut words = l.split_whitespace();
            (words.next()? == "#define" && words.next()?.ends_with(suffix)).then_some(())?;
            words.next()?.parse().ok()
        })
    };
    let (w, h) = (define("_width").ok_or("not an XBM file")?, define("_height").ok_or("not an XBM file")?);
    let body = text.split_once('{').ok_or("not an XBM file")?.1;
    let body = body.split_once('}').map_or(body, |b| b.0);
    let bytes: Vec<u8> = body
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).ok_or("invalid XBM data")?;
            u16::from_str_radix(hex, 16).map(|v| v as u8).map_err(|_| "invalid XBM data")
        })
        .collect::<Result<_, _>>()?;
    let mut img = canvas(w, h)?;
    let row = w.div_ceil(8) as usize;
    if bytes.len() < row * h as usize {
        return Err("truncated XBM".into());
    }
    for (x, y, p) in img.enumerate_pixels_mut() {
        let byte = bytes[y as usize * row + x as usize / 8];
        p.0 = if byte & (1 << (x % 8)) != 0 { BLACK } else { WHITE };
    }
    Ok(img)
}

/// The double-quoted C strings of an XPM file, in order.
fn c_strings(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(['"', '/']) {
        let tail = &rest[start..];
        if let Some(comment) = tail.strip_prefix("/*") {
            rest = comment.split_once("*/").map_or("", |c| c.1);
        } else if let Some(s) = tail.strip_prefix('"') {
            let end = s.find('"').unwrap_or(s.len());
            out.push(&s[..end]);
            rest = s.get(end + 1..).unwrap_or("");
        } else {
            rest = &tail[1..];
        }
    }
    out
}

/// An XPM color: `#RGB`/`#RRGGBB`/`#RRRRGGGGBBBB`, `None`, or a common X11 name.
fn xpm_color(spec: &str) -> [u8; 4] {
    if spec.eq_ignore_ascii_case("none") {
        return [0, 0, 0, 0];
    }
    if let Some(hex) = spec.strip_prefix('#') {
        let n = hex.len() / 3;
        if (1..=4).contains(&n) && hex.len() == n * 3 && hex.is_ascii() {
            let channel = |i: usize| {
                let v = u32::from_str_radix(&hex[i * n..(i + 1) * n], 16).unwrap_or(0);
                (v * 255 / ((1u32 << (4 * n)) - 1)) as u8
            };
            return [channel(0), channel(1), channel(2), 255];
        }
        return BLACK;
    }
    let name: String = spec.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    let name = name.replace("grey", "gray");
    if let Some(level) = name.strip_prefix("gray").and_then(|n| n.parse::<u32>().ok()).filter(|&n| n <= 100) {
        let v = ((level * 255 + 50) / 100) as u8;
        return [v, v, v, 255];
    }
    let rgb = match name.as_str() {
        "white" => [255, 255, 255],
        "red" => [255, 0, 0],
        "green" => [0, 255, 0],
        "blue" => [0, 0, 255],
        "yellow" => [255, 255, 0],
        "cyan" => [0, 255, 255],
        "magenta" => [255, 0, 255],
        "gray" => [190, 190, 190],
        "lightgray" => [211, 211, 211],
        "darkgray" => [169, 169, 169],
        "orange" => [255, 165, 0],
        "brown" => [165, 42, 42],
        "purple" => [160, 32, 240],
        "pink" => [255, 192, 203],
        "navy" | "navyblue" => [0, 0, 128],
        "maroon" => [176, 48, 96],
        "darkgreen" => [0, 100, 0],
        "darkred" => [139, 0, 0],
        "darkblue" => [0, 0, 139],
        _ => [0, 0, 0],
    };
    [rgb[0], rgb[1], rgb[2], 255]
}

/// X pixmap (XPM 3): a header string, one string per color, one per row.
pub fn decode_xpm(data: &[u8]) -> Result<RgbaImage, String> {
    let text = std::str::from_utf8(data).map_err(|_| "not an XPM file")?;
    let strings = c_strings(text);
    let (header, rest) = strings.split_first().ok_or("not an XPM file")?;
    let values: Vec<u64> = header.split_whitespace().take(4).filter_map(|v| v.parse().ok()).collect();
    let &[w, h, ncolors, cpp] = values.as_slice() else {
        return Err("not an XPM file".into());
    };
    let mut img = canvas(w, h)?;
    if cpp == 0 || cpp > 8 || (rest.len() as u64) < ncolors.saturating_add(h) {
        return Err("truncated XPM".into());
    }
    let cpp = cpp as usize;
    let mut colors = std::collections::HashMap::new();
    for line in &rest[..ncolors as usize] {
        let key = line.get(..cpp).ok_or("invalid XPM color")?;
        let words: Vec<&str> = line[cpp..].split_whitespace().collect();
        // Keys c (color), g, g4, m (mono), s (symbolic name); a value can be several words.
        let is_key = |w: &str| matches!(w, "c" | "g" | "g4" | "m" | "s");
        let value_of = |k: &str| {
            let i = words.iter().position(|&w| w == k)?;
            let end = words[i + 1..].iter().position(|&w| is_key(w)).map_or(words.len(), |e| i + 1 + e);
            Some(words[i + 1..end].join(" "))
        };
        let spec = ["c", "g", "g4", "m"].iter().find_map(|k| value_of(k)).unwrap_or_default();
        colors.insert(key, xpm_color(&spec));
    }
    let rows = &rest[ncolors as usize..];
    for (y, row) in rows.iter().take(h as usize).enumerate() {
        for x in 0..w as usize {
            let px = row.get(x * cpp..(x + 1) * cpp).and_then(|k| colors.get(k)).copied().unwrap_or([0, 0, 0, 0]);
            img.put_pixel(x as u32, y as u32, image::Rgba(px));
        }
    }
    Ok(img)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wbmp() {
        // 3×2: row 0 = white, black, white; row 1 = black, black, white.
        let img = decode_wbmp(&[0, 0, 3, 2, 0b1010_0000, 0b0010_0000]).unwrap();
        assert_eq!(img.dimensions(), (3, 2));
        assert_eq!(img.get_pixel(0, 0).0, WHITE);
        assert_eq!(img.get_pixel(1, 0).0, BLACK);
        assert_eq!(img.get_pixel(2, 1).0, WHITE);
        assert!(decode_wbmp(&[0, 0, 3, 2, 0]).is_err());
    }

    #[test]
    fn absurd_sizes_are_refused_not_overflowed() {
        let xbm = format!("#define t_width {m}\n#define t_height {m}\nstatic char t_bits[] = {{ 0x00 }};\n", m = u64::MAX);
        assert!(decode_xbm(xbm.as_bytes()).is_err());
        let xpm = format!("\"1 1 {m} 1\",\n\". c #000000\",\n\".\"", m = u64::MAX);
        assert!(decode_xpm(xpm.as_bytes()).is_err());
        let xpm = format!("\"{m} {m} 1 1\"", m = u64::MAX);
        assert!(decode_xpm(xpm.as_bytes()).is_err());
    }

    #[test]
    fn xbm() {
        let src = "#define t_width 3\n#define t_height 1\nstatic unsigned char t_bits[] = {\n 0x05 };\n";
        let img = decode_xbm(src.as_bytes()).unwrap();
        assert_eq!(img.dimensions(), (3, 1));
        assert_eq!(img.get_pixel(0, 0).0, BLACK);
        assert_eq!(img.get_pixel(1, 0).0, WHITE);
        assert_eq!(img.get_pixel(2, 0).0, BLACK);
    }

    #[test]
    fn xpm() {
        let src = r##"/* XPM */
static char *t[] = {
/* columns rows colors chars-per-pixel */
"2 2 3 1",
"  c None",
". c #FF0000",
"X c light gray s bg",
". ",
"X.",
};"##;
        let img = decode_xpm(src.as_bytes()).unwrap();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0, 0]);
        assert_eq!(img.get_pixel(0, 1).0, [211, 211, 211, 255]);
        assert_eq!(xpm_color("#fff").map(|c| c as u32), [255, 255, 255, 255]);
        assert_eq!(xpm_color("gray50")[0], 128);
    }
}
