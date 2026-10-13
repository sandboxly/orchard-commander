//! GPU rendering tests: render frames offscreen and check pixels. They skip
//! (with a note) on machines without a GPU adapter or fonts; on macOS they
//! run on Metal.

#[path = "../examples/support/mod.rs"]
mod support;

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use oc_grid::{
    BoxStyle, CellRect, Cursor, CursorShape, Flags, FontConfig, Frame, HeadlessRenderer,
    ImagePlacement, Layer, PxRect, Rgba, Style, special,
};

const BG: Rgba = Rgba::hex(0x101010);
const FG: Rgba = Rgba::hex(0xf0f0f0);
const BORDER: Rgba = Rgba::hex(0xffc107);
const SEL: Rgba = Rgba::hex(0x3050a0);

/// One renderer for all tests (device creation is slow); tests take turns.
fn renderer() -> Option<MutexGuard<'static, HeadlessRenderer>> {
    static R: OnceLock<Option<Mutex<HeadlessRenderer>>> = OnceLock::new();
    R.get_or_init(|| match HeadlessRenderer::new(FontConfig::default(), 2.0) {
        Ok(r) => Some(Mutex::new(r)),
        Err(e) => {
            eprintln!("skipping GPU tests: {e}");
            None
        }
    })
    .as_ref()
    .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()))
}

fn out_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn sample_frame() -> Frame {
    let mut f = Frame::new(24, 8, BG);
    f.draw_box(
        CellRect::new(0, 0, 24, 7),
        BoxStyle::Rounded,
        Style::fg(BORDER),
    );
    f.put_str(2, 0, " Files ", Style::new(BG, BORDER).bold());
    f.put_str(2, 1, "Hello, grid!", Style::fg(FG));
    f.put_str(2, 2, "bold", Style::fg(FG).bold());
    f.put_str(7, 2, "under", Style::fg(FG).with(Flags::UNDERLINE));
    f.put_str(2, 3, "日本 ✓ é", Style::fg(FG));
    f.fill(CellRect::new(1, 4, 22, 1), ' ', Style::new(FG, SEL));
    f.put_str(2, 4, "selected row", Style::new(FG, SEL));
    f.put_str(2, 5, "┌─┬─┐ ═══ ▄▀█░ \u{e0b0}", Style::fg(FG));
    f.put_str(0, 7, "F3 View", Style::fg(BORDER));
    f.cursor = Some(Cursor {
        col: 20,
        row: 1,
        shape: CursorShape::Block,
        color: FG,
        text: Some(BG),
    });
    f
}

#[test]
fn renders_frame_with_crisp_text_and_joined_borders() {
    let Some(mut r) = renderer() else { return };
    let frame = sample_frame();
    let img = r.render(&frame).expect("render");
    img.save_png(out_path("oc-grid-sample.png")).unwrap();
    let m = r.renderer().metrics();
    let (cw, ch) = (m.width, m.height);
    assert_eq!((img.width, img.height), (24 * cw, 8 * ch));

    // Background where nothing is drawn (row 6 is the bottom border, row 7 col 20 is empty).
    assert_eq!(img.pixel(20 * cw + 1, 7 * ch + 1), BG);

    // The top border: the line rows of ─ run unbroken from the corner cell to the far corner.
    let mask = special::rasterize('─', cw, ch).unwrap();
    let line_rows: Vec<u32> = (0..ch)
        .filter(|&y| mask[(y * cw) as usize] == 255)
        .collect();
    assert!(!line_rows.is_empty());
    for &y in &line_rows {
        for x in (10 * cw)..(23 * cw) {
            assert_eq!(img.pixel(x, y), BORDER, "top border gap at ({x}, {y})");
        }
    }
    // The left border column runs unbroken through all inner rows.
    let vmask = special::rasterize('│', cw, ch).unwrap();
    let line_cols: Vec<u32> = (0..cw).filter(|&x| vmask[x as usize] == 255).collect();
    for &x in &line_cols {
        for y in ch..(6 * ch) {
            assert_eq!(img.pixel(x, y), BORDER, "left border gap at ({x}, {y})");
        }
    }

    // Text: the 'H' cell has foreground-colored pixels, the blank cell after "grid!" none.
    let count = |col: u32, row: u32, color: Rgba| {
        let mut n = 0;
        for y in row * ch..(row + 1) * ch {
            for x in col * cw..(col + 1) * cw {
                if img.pixel(x, y) == color {
                    n += 1;
                }
            }
        }
        n
    };
    assert!(count(2, 1, FG) > 0, "H drew no solid pixels (blurry text?)");
    assert_eq!(count(15, 1, BG), cw * ch, "blank cell is pure background");

    // Selection bar: a space cell in it is entirely the selection color.
    assert_eq!(count(20, 4, SEL), cw * ch);

    // Block cursor: the cursor cell is filled with the cursor color.
    assert_eq!(count(20, 1, FG), cw * ch);

    // Underline: a full-width solid row under "under".
    let uy = 2 * ch + m.underline_y;
    for x in 7 * cw..12 * cw {
        assert_eq!(img.pixel(x, uy), FG, "underline at ({x}, {uy})");
    }

    // Bold covers more pixels than regular.
    let ink = |col: u32, row: u32| -> u32 {
        let mut n = 0;
        for y in row * ch..(row + 1) * ch {
            for x in col * cw..(col + 1) * cw {
                n += (255 - img.pixel(x, y).r.abs_diff(FG.r)) as u32 / 64;
            }
        }
        n
    };
    let mut plain = Frame::new(4, 1, BG);
    plain.put_str(0, 0, "bold", Style::fg(FG));
    let plain_img = r.render(&plain).unwrap();
    let plain_ink: u32 = (0..ch)
        .flat_map(|y| (0..cw).map(move |x| (x, y)))
        .map(|(x, y)| (255 - plain_img.pixel(x, y).r.abs_diff(FG.r)) as u32 / 64)
        .sum();
    assert!(
        ink(2, 2) > plain_ink,
        "bold b ({}) not heavier than regular ({plain_ink})",
        ink(2, 2)
    );

    // Wide character: drawn across two cells (ink in the continuation cell too).
    assert!(
        count(2, 3, BG) < cw * ch && count(3, 3, BG) < cw * ch,
        "日 should span two cells"
    );
    // Full block fills its cell exactly.
    assert_eq!(count(14, 5, FG), cw * ch);
}

#[test]
fn line_spacing_keeps_borders_joined() {
    let Some(mut r) = renderer() else { return };
    let original = r.renderer().font_config().clone();
    r.renderer()
        .set_font(FontConfig {
            line_spacing: 1.7,
            ..original.clone()
        })
        .unwrap();
    let mut f = Frame::new(6, 4, BG);
    f.draw_box(
        CellRect::new(0, 0, 6, 4),
        BoxStyle::Double,
        Style::fg(BORDER),
    );
    let img = r.render(&f).unwrap();
    img.save_png(out_path("oc-grid-spacing.png")).unwrap();
    let m = r.renderer().metrics();
    let vmask = special::rasterize('║', m.width, m.height).unwrap();
    let cols: Vec<u32> = (0..m.width).filter(|&x| vmask[x as usize] == 255).collect();
    assert_eq!(cols.len(), 2 * special::thin(m.width) as usize, "two lines");
    for &x in &cols {
        for y in m.height..3 * m.height {
            assert_eq!(img.pixel(x, y), BORDER, "║ gap at ({x}, {y})");
        }
    }
    r.renderer().set_font(original).unwrap();
}

#[test]
fn images_are_placed_scaled_and_clipped() {
    let Some(mut r) = renderer() else { return };
    let red = [255, 0, 0, 255];
    let green = [0, 255, 0, 255];
    let blue = [0, 0, 255, 255];
    let white = [255, 255, 255, 255];
    let pixels: Vec<u8> = [red, green, blue, white].concat();
    let id = r.renderer().upload_image(2, 2, &pixels).unwrap();
    let mut f = Frame::new(10, 5, BG);
    f.add_image(ImagePlacement::new(id, PxRect::new(10.0, 10.0, 40.0, 40.0)));
    // Clipped: only the left half of a second copy shows.
    f.add_image(
        ImagePlacement::new(id, PxRect::new(60.0, 10.0, 40.0, 40.0))
            .clipped(PxRect::new(0.0, 0.0, 80.0, 200.0)),
    );
    f.add_rect(PxRect::new(0.0, 70.0, 5.0, 5.0), BORDER, Layer::AboveText);
    let img = r.render_sized(&f, (120, 100)).unwrap();
    img.save_png(out_path("oc-grid-images.png")).unwrap();
    // Quadrant centres keep their colors (nearest texel at the centre of each quarter).
    assert_eq!(img.pixel(15, 15), Rgba::from(red));
    assert_eq!(img.pixel(45, 15), Rgba::from(green));
    assert_eq!(img.pixel(15, 45), Rgba::from(blue));
    assert_eq!(img.pixel(45, 45), Rgba::from(white));
    assert_eq!(img.pixel(65, 15), Rgba::from(red));
    assert_eq!(img.pixel(95, 15), BG, "clipped away");
    assert_eq!(img.pixel(2, 72), BORDER);
    assert_eq!(
        img.pixel(115, 95),
        BG,
        "area beyond the grid shows the background"
    );

    assert!(r.renderer().update_image(id, 1, 1, &[0, 0, 0, 255]).is_ok());
    r.renderer().remove_image(id);
    assert!(
        r.renderer()
            .update_image(id, 1, 1, &[0, 0, 0, 255])
            .is_err()
    );
    assert!(r.renderer().upload_image(2, 2, &[0; 3]).is_err());
}

#[test]
fn cursor_shapes_and_inverse() {
    let Some(mut r) = renderer() else { return };
    let mut f = Frame::new(4, 1, BG);
    f.put_str(0, 0, "  ", Style::new(FG, BG).with(Flags::INVERSE));
    f.cursor = Some(Cursor {
        col: 3,
        row: 0,
        shape: CursorShape::Beam,
        color: BORDER,
        text: None,
    });
    let img = r.render(&f).unwrap();
    let m = r.renderer().metrics();
    assert_eq!(
        img.pixel(1, 1),
        FG,
        "inverse space shows the fg as background"
    );
    assert_eq!(
        img.pixel(3 * m.width, m.height / 2),
        BORDER,
        "beam at the cell's left edge"
    );
    assert_eq!(img.pixel(3 * m.width + m.width - 1, m.height / 2), BG);

    f.cursor = Some(Cursor {
        col: 3,
        row: 0,
        shape: CursorShape::HollowBlock,
        color: BORDER,
        text: None,
    });
    let img = r.render(&f).unwrap();
    assert_eq!(img.pixel(3 * m.width + m.width / 2, 0), BORDER);
    assert_eq!(
        img.pixel(3 * m.width + m.width / 2, m.height / 2),
        BG,
        "hollow"
    );
}

#[test]
fn emoji_and_fallback_glyphs_render() {
    let Some(mut r) = renderer() else { return };
    let mut f = Frame::new(8, 1, BG);
    f.put_str(0, 0, "😀 ✓ 日", Style::fg(FG));
    let img = r.render(&f).unwrap();
    img.save_png(out_path("oc-grid-emoji.png")).unwrap();
    let m = r.renderer().metrics();
    let colorful = (0..2 * m.width)
        .flat_map(|x| (0..m.height).map(move |y| (x, y)))
        .any(|(x, y)| {
            let p = img.pixel(x, y);
            p != BG && !(p.r == p.g && p.g == p.b)
        });
    if cfg!(target_os = "macos") {
        assert!(colorful, "emoji should be drawn in color");
    }
}

#[test]
fn fake_file_manager_full_frame() {
    let Some(mut r) = renderer() else { return };
    let m = r.renderer().metrics();
    let mut fm = support::FakeFm::new(100_000);
    fm.move_by(7);
    let mut frame = Frame::new(120, 36, support::BG);
    fm.draw(&mut frame, (m.width, m.height));
    let img = r.render(&frame).unwrap();
    img.save_png(out_path("oc-grid-filemanager.png")).unwrap();
    assert!(r.last_stats().glyphs > 1000);
}

/// Offscreen render time of a full 240x70 frame. Run with
/// `cargo test -p oc-grid --release --test render -- --ignored --nocapture bench`.
#[test]
#[ignore]
fn bench_full_frame_240x70() {
    let Some(mut r) = renderer() else { return };
    for scale in [1.0, 2.0] {
        r.renderer().set_scale_factor(scale).unwrap();
        let m = r.renderer().metrics();
        let mut fm = support::FakeFm::new(100_000);
        let mut frame = Frame::new(240, 70, support::BG);
        let size = (240 * m.width, 70 * m.height);
        fm.draw(&mut frame, (m.width, m.height));
        r.render_only(&frame, size).unwrap(); // warm the glyph cache

        let n = 200;
        let t = Instant::now();
        for i in 0..n {
            fm.move_by(if i % 2 == 0 { 1 } else { -1 });
            fm.draw(&mut frame, (m.width, m.height));
        }
        let build = t.elapsed() / n;

        let t = Instant::now();
        for _ in 0..n {
            r.renderer().prepare(&frame, size);
        }
        let prepare = t.elapsed() / n;

        let t = Instant::now();
        for _ in 0..n {
            r.render_only(&frame, size).unwrap();
        }
        let full = t.elapsed() / n;
        let t = Instant::now();
        for _ in 0..20 {
            r.render_sized(&frame, size).unwrap();
        }
        let readback = t.elapsed() / 20;
        let stats = r.last_stats();
        println!(
            "240x70 @ {}x{} px (cell {}x{}, {} glyphs, {} quads): frame build {build:?}, \
             prepare {prepare:?}, prepare+GPU {full:?}, with readback {readback:?}",
            size.0, size.1, m.width, m.height, stats.glyphs, stats.quads,
        );
    }
    // Worst case: every cell a glyph, colors changing every cell (no background runs).
    r.renderer().set_scale_factor(1.0).unwrap();
    let m = r.renderer().metrics();
    let mut dense = Frame::new(240, 70, support::BG);
    for row in 0..70u16 {
        for col in 0..240u16 {
            let ch = (b'!' + ((row as u32 * 7 + col as u32) % 94) as u8) as char;
            let fg = Rgba::rgb((col * 3) as u8, (row * 3) as u8, 200);
            let bg = Rgba::rgb(20 + (col % 2) as u8 * 10, 20, 30);
            dense.put_char(col, row, ch, Style::new(fg, bg));
        }
    }
    let size = (240 * m.width, 70 * m.height);
    r.render_only(&dense, size).unwrap();
    let n = 200;
    let t = Instant::now();
    for _ in 0..n {
        r.renderer().prepare(&dense, size);
    }
    let prepare = t.elapsed() / n;
    let t = Instant::now();
    for _ in 0..n {
        r.render_only(&dense, size).unwrap();
    }
    let full = t.elapsed() / n;
    let stats = r.last_stats();
    println!(
        "240x70 dense ({} glyphs, {} quads): prepare {prepare:?}, prepare+GPU {full:?}",
        stats.glyphs, stats.quads
    );
    r.renderer().set_scale_factor(2.0).unwrap();
}

#[test]
fn opaque_background_stays_opaque_under_text_and_overlays() {
    // Blending must not punch holes in the target's alpha: a window
    // compositor (or a PNG viewer) would otherwise show through text edges.
    let Some(mut r) = renderer() else { return };
    let mut f = Frame::new(12, 2, BG);
    f.put_str(0, 0, "Agw日本 é", Style::fg(FG));
    f.put_str(0, 1, "dim", Style::fg(FG).with(Flags::DIM));
    f.add_rect(
        CellRect::new(4, 1, 4, 1),
        Rgba::new(255, 0, 0, 100),
        Layer::AboveText,
    );
    let img = r.render(&f).unwrap();
    let translucent = img.data.as_chunks::<4>().0.iter().filter(|p| p[3] != 255).count();
    assert_eq!(translucent, 0, "pixels with alpha < 255");
}

#[test]
fn wide_char_underline_is_drawn_once() {
    // The continuation cell carries the style too; a translucent underline
    // must not be blended twice over the second half.
    let Some(mut r) = renderer() else { return };
    let mut f = Frame::new(4, 1, BG);
    let fg = Rgba::new(255, 255, 255, 128);
    f.put_str(0, 0, "日", Style::fg(fg).with(Flags::UNDERLINE));
    let img = r.render(&f).unwrap();
    let m = r.renderer().metrics();
    let y = m.underline_y;
    let first = img.pixel(1, y);
    let second = img.pixel(m.width + m.width / 2, y);
    assert_ne!(first, BG, "underline drawn");
    assert_eq!(first, second, "same blend over both halves");
}

#[test]
fn many_distinct_glyphs_grow_only_the_mask_atlas() {
    // More CJK glyphs than fit the initial 1024² coverage atlas at 2x: the
    // atlas grows and every glyph is still drawn; the color atlas (emoji,
    // 4 bytes per pixel) keeps its size.
    let Some(mut r) = renderer() else { return };
    let (mask0, color0) = r.renderer().atlas_sizes();
    let mut f = Frame::new(200, 20, BG);
    let mut cp = 0x4E00u32;
    for row in 0..20u16 {
        let mut col = 0;
        while col < 200 {
            let ch = char::from_u32(cp).unwrap();
            cp += 1;
            col = f.put_char(col, row, ch, Style::fg(FG));
        }
    }
    let img = r.render(&f).unwrap();
    let stats = r.last_stats();
    assert_eq!(stats.glyphs, 2000, "every glyph drawn");
    let (mask, color) = r.renderer().atlas_sizes();
    assert!(mask > mask0, "mask atlas grew ({mask0} -> {mask})");
    assert_eq!(color, color0, "color atlas untouched");
    // A glyph from the last row is really there.
    let m = r.renderer().metrics();
    let ink = (0..2 * m.width)
        .flat_map(|x| (19 * m.height..20 * m.height).map(move |y| (x, y)))
        .filter(|&(x, y)| img.pixel(x, y) != BG)
        .count();
    assert!(ink > 0);
    // Rendering again hits the cache: nothing new is rasterized.
    r.render(&f).unwrap();
    assert_eq!(r.last_stats().uploads, 0);
}

#[test]
fn block_cursor_on_a_wide_char_covers_both_cells() {
    let Some(mut r) = renderer() else { return };
    let mut f = Frame::new(4, 1, BG);
    f.put_str(0, 0, "日", Style::fg(FG));
    f.cursor = Some(Cursor {
        col: 0,
        row: 0,
        shape: CursorShape::Block,
        color: BORDER,
        text: Some(BG),
    });
    let img = r.render(&f).unwrap();
    let m = r.renderer().metrics();
    assert_eq!(img.pixel(0, 0), BORDER);
    assert_eq!(img.pixel(2 * m.width - 1, 0), BORDER, "second half");
    assert_eq!(img.pixel(2 * m.width, 0), BG, "not beyond");
}

#[test]
fn synthesized_or_real_italic_leans_right() {
    let Some(mut r) = renderer() else { return };
    let mut f = Frame::new(8, 1, BG);
    f.put_str(0, 0, "I", Style::fg(FG));
    f.put_str(2, 0, "I", Style::fg(FG).with(Flags::ITALIC));
    f.put_str(4, 0, "x", Style::fg(FG).with(Flags::STRIKETHROUGH));
    f.put_str(6, 0, "x", Style::fg(FG).with(Flags::DIM));
    let img = r.render(&f).unwrap();
    img.save_png(out_path("oc-grid-styles.png")).unwrap();
    let m = r.renderer().metrics();
    // Ink centroid (x) of the upper and lower third of a cell span.
    let centroid = |col: u32, rows: std::ops::Range<u32>| {
        let (mut sum, mut n) = (0f64, 0f64);
        for y in rows {
            for x in col * m.width..(col + 2) * m.width {
                let w = img.pixel(x, y).r.saturating_sub(BG.r) as f64;
                sum += w * x as f64;
                n += w;
            }
        }
        sum / n.max(1.0)
    };
    let (top, bottom) = (
        m.height / 6..m.height / 3,
        m.height * 2 / 3..m.height * 5 / 6,
    );
    let upright = centroid(0, top.clone()) - centroid(0, bottom.clone());
    let italic = centroid(2, top) - centroid(2, bottom);
    assert!(upright.abs() < 1.0, "upright I leans by {upright}");
    assert!(
        italic > 1.0,
        "italic I should lean right, leans by {italic}"
    );
    // Strikethrough: a solid row through the middle of the x cell.
    let sy = m.strike_y;
    assert!((4 * m.width..5 * m.width).all(|x| img.pixel(x, sy) == FG));
    // Dim text never reaches the full foreground.
    let brightest = (6 * m.width..7 * m.width)
        .flat_map(|x| (0..m.height).map(move |y| (x, y)))
        .map(|(x, y)| img.pixel(x, y).r)
        .max()
        .unwrap();
    assert!(
        brightest > BG.r && brightest < FG.r - 40,
        "dim max {brightest}"
    );
}

#[test]
fn strong_box_lines_are_thicker_other_glyphs_unchanged() {
    // The focused panel's frame: Flags::STRONG draws box lines one step thicker.
    let Some(mut r) = renderer() else { return };
    let mut f = Frame::new(6, 2, BG);
    f.put_str(0, 0, "──a", Style::fg(FG));
    f.put_str(0, 1, "──a", Style::fg(FG).with(Flags::STRONG));
    let img = r.render(&f).unwrap();
    let m = r.renderer().metrics();
    let lit = |row: u32, col: u32| {
        (0..m.height)
            .filter(|y| img.pixel(col * m.width + m.width / 2, row * m.height + y) != BG)
            .count()
    };
    assert_eq!(lit(1, 0), 2 * lit(0, 0), "twice the thin line");
    let glyph = |row: u32| {
        (0..m.width * m.height)
            .filter(|i| img.pixel(2 * m.width + i % m.width, row * m.height + i / m.width) != BG)
            .count()
    };
    assert_eq!(glyph(0), glyph(1), "text ignores STRONG");
}
