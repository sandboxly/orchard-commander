//! Painting the player's pixel graphics with fill rectangles: the spectrum's
//! LED bars, the waveform seek bar and the video slider (the paint code of
//! 1.x's `SpectrumWidget`, `WaveformWidget` and the `QSlider`).
//!
//! Everything here is a pure function of its inputs, sized from the cell
//! metrics (`cell_px` is in physical pixels) the way 1.x sized from the
//! font's "M" advance and line height.

use orchard_grid::{Frame, Layer, PxRect, Rgba, Style};
use oc_media::waveform::{played_x, x_to_ms};
use oc_media::{WaveformLevels, format_time};

use super::image::area_px;
use crate::geom::Rect;
use crate::widgets::{DrawCtx, put, width};

/// `a` → `b` by `t` (0..=1), in sRGB like 1.x's `_mix`.
pub fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Rgba::new(m(a.r, b.r), m(a.g, b.g), m(a.b, b.b), m(a.a, b.a))
}

/// `c` with its alpha multiplied by `f`.
fn fade(c: Rgba, f: f32) -> Rgba {
    c.with_alpha((c.a as f32 * f.clamp(0.0, 1.0)).round() as u8)
}

fn rect(f: &mut Frame, x: f32, y: f32, w: f32, h: f32, color: Rgba) {
    if w > 0.0 && h > 0.0 && color.a > 0 {
        f.add_rect(PxRect::new(x, y, w, h), color, Layer::BelowText);
    }
}

// -- spectrum ---------------------------------------------------------------

/// Bar width, gap and how many bars fit `width_px` (1.x: 0.9 and 0.25 of
/// the "M" advance, at most 96 bars).
pub fn spectrum_metrics(width_px: f32, cell_w: f32) -> (f32, f32, usize) {
    let bar = (cell_w * 0.9).round().max(4.0);
    let gap = (cell_w * 0.25).round().max(2.0);
    (bar, gap, oc_media::spectrum::bar_count(width_px, bar, gap))
}

/// The LED bars: lit segments from `border_active` through `accent` (lower
/// 70 %) to `error`, unlit ones faint, a `foreground` cap at each peak.
pub fn paint_spectrum(f: &mut Frame, area: Rect, ctx: &DrawCtx, bars: &[f32], peaks: &[f32]) {
    let c = &ctx.theme.colors;
    let px = area_px(area, ctx.cell_px);
    if px.is_empty() {
        return;
    }
    let (bar, gap, count) = spectrum_metrics(px.w, ctx.cell_px.0);
    let segment = (ctx.cell_px.1 / 4.0).round().max(3.0);
    let seg_gap = (segment / 3.0).round().max(1.0);
    let rows = (((px.h + seg_gap) / (segment + seg_gap)).floor() as usize).max(1);
    let total = count as f32 * (bar + gap) - gap;
    let left = (px.x + (px.w - total) / 2.0).floor();
    let knee = rows as f32 * 0.7;
    let colors: Vec<Rgba> = (0..rows)
        .map(|r| {
            let r = r as f32;
            if r < knee { mix(c.border_active, c.accent, r / knee) } else { mix(c.accent, c.error, (r - knee) / (rows as f32 - knee)) }
        })
        .collect();
    let unlit = fade(c.border, 0.35);
    let level = |v: &[f32], i: usize| if v.len() == count { v[i] } else { 0.0 };
    for i in 0..count {
        let x = left + i as f32 * (bar + gap);
        let lit = (level(bars, i) * rows as f32).round() as usize;
        let peak_row = ((level(peaks, i) * rows as f32).round() as usize).min(rows - 1);
        for (r, lit_color) in colors.iter().enumerate() {
            let y = px.y + px.h - (r + 1) as f32 * (segment + seg_gap) + seg_gap;
            let color = if r < lit {
                *lit_color
            } else if r == peak_row && peak_row > 0 {
                c.foreground
            } else {
                unlit
            };
            rect(f, x, y, bar, segment, color);
        }
    }
}

// -- waveform ---------------------------------------------------------------

/// What the waveform shows besides the levels.
pub struct WaveState<'a> {
    pub levels: &'a WaveformLevels,
    /// Shown instead of bars (e.g. [`oc_media::waveform::REMOTE_NOTE`]).
    pub note: &'a str,
    pub duration_ms: i64,
    pub position_ms: i64,
    /// The column the mouse is over.
    pub hover: Option<u16>,
}

/// Waveform bar width and gap (1.x: 0.45 of the "M" advance).
fn wave_metrics(cell_w: f32) -> (f32, f32) {
    let bar = (cell_w * 0.45).round().max(3.0);
    (bar, (bar * 0.35).round().max(1.0))
}

/// The whole file's levels as the seek bar: the played part lit, a
/// reflection below, the playhead, and a time chip where the mouse is.
pub fn paint_waveform(f: &mut Frame, area: Rect, ctx: &DrawCtx, s: &WaveState) {
    let c = &ctx.theme.colors;
    let px = area_px(area, ctx.cell_px);
    if px.is_empty() {
        return;
    }
    let line = ctx.cell_px.1;
    let (top, bottom) = (px.y + line * 1.2, px.y + px.h - line * 0.4);
    let mid = top + (bottom - top) * 0.68;
    let (up, down) = (mid - top, bottom - mid);
    let total = s.levels.total_ms(s.duration_ms);
    let played = played_x(s.position_ms, px.w, total);

    if s.levels.peaks.is_empty() {
        rect(f, px.x, mid.floor(), px.w, 1.0, c.border);
        let text = s.levels.status_text(s.note);
        if !text.is_empty() {
            // Centered in the space above the line.
            let row = area.y + (((top + up / 2.0) - px.y) / line).floor() as u16;
            let w = width(&text).min(area.w);
            put(f, area.x + (area.w - w) / 2, row.min(area.bottom().saturating_sub(1)), &text, Style::new(c.muted, c.surface), area.w);
        }
    } else {
        let (bar, gap) = wave_metrics(ctx.cell_px.0);
        let count = (((px.w + gap) / (bar + gap)).floor() as usize).max(1);
        let columns = s.levels.columns(count, s.duration_ms);
        let loudest = columns.iter().flatten().map(|c| c.0).fold(0.05f32, f32::max);
        rect(f, px.x, top, played, bottom - top, fade(c.border_active, 0.07));
        for (i, column) in columns.iter().enumerate() {
            let x = px.x + i as f32 * (bar + gap);
            let Some((peak, rms)) = *column else {
                rect(f, x, (mid - 0.5).floor(), bar, 1.0, c.border); // still decoding
                continue;
            };
            let (peak, rms) = (peak / loudest, (rms / loudest).min(peak / loudest));
            let lit = x + bar / 2.0 - px.x <= played;
            for (level, alpha) in [(peak, 0.45f32), (rms, 1.0)] {
                let height = (level * up).max(1.0);
                if lit {
                    // 1.x's gradient, accent at the top to border_active at the line.
                    const STEPS: usize = 3;
                    let step = height / STEPS as f32;
                    for k in 0..STEPS {
                        let y0 = mid - height + k as f32 * step;
                        let t = 1.0 - (mid - (y0 + step / 2.0)) / up.max(1.0);
                        rect(f, x, y0, bar, step, fade(mix(c.accent, c.border_active, t), alpha));
                    }
                } else {
                    rect(f, x, mid - height, bar, height, fade(c.muted, if alpha < 1.0 { 0.35 } else { 0.85 }));
                }
                let reflection = if lit { fade(c.border_active, alpha * 0.35) } else { fade(c.muted, if alpha < 1.0 { 0.35 * 0.35 } else { 0.85 * 0.35 }) };
                rect(f, x, mid + 1.0, bar, (level * down * 0.8).max(1.0), reflection);
            }
        }
    }

    // The playhead, and the hover position with its time.
    if total > 0 && (s.position_ms > 0 || s.hover.is_none()) {
        rect(f, (px.x + played - 1.0).round(), top - line * 0.2, 2.0, bottom - top + line * 0.2, c.foreground);
    }
    if let (Some(col), true) = (s.hover, total > 0) {
        let x = (col.saturating_sub(area.x) as f32 + 0.5) * ctx.cell_px.0;
        rect(f, (px.x + x).round(), top, 1.0, bottom - top, fade(c.foreground, 0.35));
        let label = format!(" {} ", format_time(hover_ms(area, col, total)));
        let w = width(&label).min(area.w);
        let left = (col.saturating_sub(w / 2)).clamp(area.x, area.right().saturating_sub(w));
        put(f, left, area.y, &label, Style::new(c.surface, c.border_active), w);
    }
}

/// The time at column `col` of a seek bar in `area` that stands for `total_ms`.
pub fn hover_ms(area: Rect, col: u16, total_ms: i64) -> i64 {
    x_to_ms(col.saturating_sub(area.x) as f32 + 0.5, area.w as f32, total_ms)
}

// -- slider -----------------------------------------------------------------

/// The video's seek slider: a groove, the played part, a handle.
pub fn paint_slider(f: &mut Frame, area: Rect, ctx: &DrawCtx, position_ms: i64, duration_ms: i64) {
    let c = &ctx.theme.colors;
    let px = area_px(area, ctx.cell_px);
    if px.is_empty() {
        return;
    }
    let groove = (ctx.cell_px.1 / 8.0).round().max(2.0);
    let y = (px.y + (px.h - groove) / 2.0).round();
    let handle_w = (ctx.cell_px.0 * 0.8).round().max(4.0);
    let travel = (px.w - handle_w).max(0.0);
    let frac = if duration_ms > 0 { (position_ms as f32 / duration_ms as f32).clamp(0.0, 1.0) } else { 0.0 };
    let hx = (px.x + travel * frac).round();
    rect(f, px.x, y, px.w, groove, c.border);
    rect(f, px.x, y, hx - px.x + handle_w / 2.0, groove, c.border_active);
    let hh = (px.h * 0.6).round();
    rect(f, hx, (px.y + (px.h - hh) / 2.0).round(), handle_w, hh, c.foreground);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixes_and_metrics() {
        assert_eq!(mix(Rgba::rgb(0, 0, 0), Rgba::rgb(200, 100, 50), 0.5), Rgba::rgb(100, 50, 25));
        assert_eq!(mix(Rgba::rgb(0, 0, 0), Rgba::rgb(200, 100, 50), 2.0), Rgba::rgb(200, 100, 50));
        // 16 px cells: 14 px bars, 4 px gaps.
        let (bar, gap, count) = spectrum_metrics(180.0, 16.0);
        assert_eq!((bar, gap, count), (14.0, 4.0, 10));
        assert_eq!(spectrum_metrics(100_000.0, 16.0).2, 96);
    }

    #[test]
    fn hover_times_span_the_bar() {
        let area = Rect::new(10, 0, 20, 5);
        assert_eq!(hover_ms(area, 10, 20_000), 500);
        assert_eq!(hover_ms(area, 29, 20_000), 19_500);
        assert_eq!(hover_ms(area, 99, 20_000), 20_000);
    }
}
