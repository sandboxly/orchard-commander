//! A fake two-panel file manager screen (like the site screenshots), shared
//! by the demo, the benchmark and the render tests.

#![allow(dead_code)]

use oc_grid::{
    BoxStyle, CellRect, Flags, Frame, ImageId, ImagePlacement, Layer, PxRect, Rgba, Style,
    elide_left, elide_right,
};

pub const BG: Rgba = Rgba::hex(0x1f1f1f);
pub const SURFACE: Rgba = Rgba::hex(0x282828);
pub const TEXT: Rgba = Rgba::hex(0xeadfc8);
pub const MUTED: Rgba = Rgba::hex(0x8c8c8c);
pub const ACCENT: Rgba = Rgba::hex(0xfbbf24);
pub const HEADER: Rgba = Rgba::hex(0xf97316);
pub const BORDER: Rgba = Rgba::hex(0x4a4a4a);
pub const DIR: Rgba = Rgba::hex(0x93c5fd);
pub const DARK: Rgba = Rgba::hex(0x1b1b1b);

pub struct FakeFm {
    pub total: usize,
    pub cursor: usize,
    pub scroll: usize,
    /// Rows of the file list visible in the last draw (for paging).
    pub page: usize,
    pub preview: Option<ImageId>,
    pub path: String,
}

impl FakeFm {
    pub fn new(total: usize) -> Self {
        FakeFm {
            total,
            cursor: 0,
            scroll: 0,
            page: 20,
            preview: None,
            path: "~/Downloads/photos-from-maria.zip/Lisbon trip/Day 1".into(),
        }
    }

    pub fn name(i: usize) -> String {
        const EXT: [&str; 6] = ["JPG", "txt", "rs", "pdf", "mp4", "zip"];
        if i % 17 == 3 {
            format!("folder_{i:06}")
        } else {
            format!("IMG_{:06}.{}", i, EXT[i % EXT.len()])
        }
    }

    fn size(i: usize) -> String {
        let n = (i.wrapping_mul(2654435761) >> 7) % 99_000;
        format!("{}.{} K", n / 1000 + 1, n % 10)
    }

    pub fn move_by(&mut self, delta: isize) {
        let max = self.total.saturating_sub(1) as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + self.page {
            self.scroll = self.cursor + 1 - self.page;
        }
    }

    /// Draw the whole screen into `frame` (sized to the window), using
    /// `cell` = (width, height) pixels for the pixel-placed overlays.
    pub fn draw(&mut self, frame: &mut Frame, cell: (u32, u32)) {
        frame.clear(BG);
        let (cols, rows) = (frame.cols(), frame.rows());
        if cols < 20 || rows < 6 {
            frame.put_str(0, 0, "window too small", Style::fg(TEXT));
            return;
        }
        let half = cols / 2;
        let text = Style::fg(TEXT);
        let tab_on = Style::new(DARK, ACCENT).bold();
        let tab_off = Style::fg(MUTED);

        // Tabs above each panel.
        for (x, active) in [(0u16, true), (half, false)] {
            frame.put_str(x + 2, 0, " Files ", if active { tab_on } else { tab_off });
            frame.put_str(
                x + 11,
                0,
                " Preview ",
                if active {
                    tab_off
                } else {
                    Style::new(TEXT, BORDER).bold()
                },
            );
        }

        // Panels.
        let body_rows = rows - 2;
        let left = CellRect::new(0, 1, half, body_rows);
        let right = CellRect::new(half, 1, cols - half, body_rows);
        frame.fill(
            CellRect::new(1, 2, half - 2, body_rows - 2),
            ' ',
            Style::new(TEXT, SURFACE),
        );
        frame.fill(
            CellRect::new(half + 1, 2, cols - half - 2, body_rows - 2),
            ' ',
            Style::new(TEXT, SURFACE),
        );
        frame.draw_box(left, BoxStyle::Rounded, Style::fg(ACCENT));
        frame.draw_box(right, BoxStyle::Rounded, Style::fg(BORDER));

        let title = format!(
            " {} ",
            elide_left(&self.path, (half as usize).saturating_sub(8))
        );
        frame.put_str(2, 1, &title, tab_on);
        frame.put_str(half + 2, 1, " IMG_000001.JPG ", Style::new(MUTED, SURFACE));

        // Header. Text runs from column 2 to half - 3 (one blank column inside each border).
        let date_x = half - 18;
        let size_end = date_x - 2;
        let size_w = 9u16;
        let name_w = size_end.saturating_sub(size_w + 3);
        let hdr = Style::new(HEADER, SURFACE).bold();
        frame.put_str(2, 2, "Name ▲", hdr);
        frame.put_str(size_end - 4, 2, "Size", hdr);
        frame.put_str(date_x, 2, "Modified", hdr);

        // File list.
        let list_top = 3u16;
        let list_rows = (body_rows - 3) as usize;
        self.page = list_rows.max(1);
        self.move_by(0);
        for r in 0..list_rows.min(self.total - self.scroll.min(self.total)) {
            let i = self.scroll + r;
            let y = list_top + r as u16;
            let selected = i == self.cursor;
            let base = if selected {
                Style::new(DARK, ACCENT)
            } else {
                Style::new(TEXT, SURFACE)
            };
            if selected {
                frame.fill(CellRect::new(1, y, half - 2, 1), ' ', base);
            }
            let name = Self::name(i);
            let is_dir = name.starts_with("folder");
            let name_style = if is_dir && !selected {
                Style::new(DIR, SURFACE).bold()
            } else {
                base
            };
            frame.put_str_clipped(
                2,
                y,
                &elide_right(&name, name_w as usize),
                name_style,
                2 + name_w,
            );
            let size = if is_dir {
                "<DIR>".to_string()
            } else {
                Self::size(i)
            };
            frame.put_str(size_end - size.chars().count() as u16, y, &size, base);
            frame.put_str(date_x, y, "2026-10-12 16:59", base);
        }

        // Scrollbar thumb, pixel-placed over the right border column.
        if self.total > list_rows {
            let (cw, ch) = (cell.0 as f32, cell.1 as f32);
            let track = list_rows as f32 * ch;
            let thumb = (track * list_rows as f32 / self.total as f32).max(ch / 2.0);
            let pos = (track - thumb) * self.scroll as f32 / (self.total - list_rows) as f32;
            // Centred on the right border line.
            let x = (half - 1) as f32 * cw + (cw / 2.0).floor() - 1.0;
            frame.add_rect(
                PxRect::new(x, list_top as f32 * ch + pos, 3.0, thumb),
                MUTED,
                Layer::AboveText,
            );
        }

        // Status in the bottom border.
        let status = format!(
            " {} of {} · zip archive · read-only ",
            self.cursor + 1,
            self.total
        );
        let sx = half.saturating_sub(status.chars().count() as u16 + 2);
        frame.put_str(sx, rows - 2, &status, Style::new(MUTED, BG));
        frame.put_str(
            cols - 30,
            rows - 2,
            " 1600×1067 · JPEG · 88.1 K ",
            Style::new(MUTED, BG),
        );

        // Right panel: a "View as" switch and an image preview.
        frame.put_str(half + 2, 2, "View as ", Style::new(MUTED, SURFACE));
        frame.put_str(half + 10, 2, "  Image  ", Style::new(TEXT, BORDER).bold());
        frame.put_str(half + 22, 2, "Hex", Style::new(MUTED, SURFACE));
        if let Some(id) = self.preview {
            let area = CellRect::new(half + 1, 4, cols - half - 2, body_rows.saturating_sub(5));
            frame.add_image(ImagePlacement::new(id, area));
        } else {
            let sample = [
                "fn main() {",
                "    println!(\"日本語 ✓ ü é\");",
                "}",
                "",
                "░▒▓█ ▁▂▃▄▅▆▇█ ┼─┬─┐",
                "",
            ];
            for (i, line) in sample.iter().enumerate() {
                frame.put_str(half + 2, 4 + i as u16, line, Style::new(TEXT, SURFACE));
            }
            frame.put_str(half + 2, 10, "bold", Style::new(TEXT, SURFACE).bold());
            frame.put_str(
                half + 7,
                10,
                "italic",
                Style::new(TEXT, SURFACE).with(Flags::ITALIC),
            );
            frame.put_str(
                half + 14,
                10,
                "under",
                Style::new(TEXT, SURFACE).with(Flags::UNDERLINE),
            );
            frame.put_str(
                half + 20,
                10,
                "strike",
                Style::new(TEXT, SURFACE).with(Flags::STRIKETHROUGH),
            );
            frame.put_str(
                half + 27,
                10,
                "dim",
                Style::new(TEXT, SURFACE).with(Flags::DIM),
            );
            frame.put_str(
                half + 2,
                11,
                "\u{e0b6}\u{e0b4} \u{e0b0}\u{e0b2} ",
                Style::new(ACCENT, SURFACE),
            );
        }

        // F-key bar.
        let keys = [
            ("F3", "Preview"),
            ("F4", "Edit"),
            ("F5", "Copy"),
            ("F6", "Move"),
            ("F7", "MkDir"),
            ("F8", "Trash"),
            ("F9", "Menu"),
            ("F10", "Quit"),
        ];
        let mut x = 1;
        let y = rows - 1;
        for (k, label) in keys {
            x = frame.put_str(x, y, k, Style::fg(ACCENT).bold());
            x = frame.put_str(x, y, &format!(" {label}  "), Style::fg(TEXT));
        }
        let right_keys = "⌘K Connect  ⌘P Commands ";
        let rx = cols.saturating_sub(right_keys.chars().count() as u16);
        if rx > x {
            frame.put_str(rx, y, "⌘K", Style::fg(ACCENT).bold());
            frame.put_str(rx + 2, y, " Connect  ", text);
            frame.put_str(rx + 12, y, "⌘P", Style::fg(ACCENT).bold());
            frame.put_str(rx + 14, y, " Commands", text);
        }
    }
}

/// A sky-and-mountains gradient for the preview pane (no image decoder needed).
pub fn landscape(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let fy = y as f32 / h as f32;
            let fx = x as f32 / w as f32;
            let ridge = 0.55 + 0.08 * (fx * 23.0).sin() + 0.05 * (fx * 61.0).cos();
            let px = if fy > 0.8 {
                [20, 70 + (fy * 40.0) as u8, 100, 255]
            } else if fy > ridge {
                [
                    10,
                    40 + ((1.0 - fy) * 60.0) as u8,
                    80 + ((1.0 - fy) * 60.0) as u8,
                    255,
                ]
            } else {
                let sun = ((fx - 0.3).powi(2) + (fy - 0.45).powi(2)).sqrt();
                if sun < 0.05 {
                    [250, 240, 200, 255]
                } else {
                    [
                        20 + (fy * 80.0) as u8,
                        90 + (fy * 100.0) as u8,
                        150 + (fy * 80.0) as u8,
                        255,
                    ]
                }
            };
            out.extend_from_slice(&px);
        }
    }
    out
}
