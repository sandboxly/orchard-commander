//! Drawing the info window: facts as label / value rows, then for folders
//! the space chart with its legend.

use orchard_grid::{Flags, Frame, Layer, PxRect, Rgba, Style};
use oc_preview::content::thousands;
use oc_vfs::human_size;

use super::chart;
use super::{InfoWindow, Usage};
use crate::geom::Rect;
use crate::panel::draw::format_mtime;
use crate::theme::Colors;
use crate::widgets::{BoxFrame, DrawCtx, fill, put, put_elided, put_right};

/// Label column, value starts after it.
const LABEL_W: u16 = 13;
/// The chart's hole, as a share of its diameter.
const HOLE: f32 = 0.58;

pub fn draw(w: &InfoWindow, f: &mut Frame, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    let screen = Rect::screen(f.cols(), f.rows());
    let status = status(w);
    BoxFrame { title: &w.name, subtitle: &status, active: ctx.window_focused }.draw(f, screen, ctx);
    let inner = screen.inset(3, 2);
    if inner.is_empty() {
        return;
    }
    let label = Style::new(c.muted, c.surface);
    let value = Style::new(c.foreground, c.surface);
    let mut y = inner.y;
    if let Some(error) = &w.error {
        put_elided(f, inner.x, y, error, Style::new(c.error, c.surface), inner.w);
        return;
    }
    let value_w = inner.w.saturating_sub(LABEL_W);
    for (name, text) in facts(w) {
        if y >= inner.bottom() {
            return;
        }
        put(f, inner.x, y, name, label, LABEL_W);
        put_elided(f, inner.x + LABEL_W, y, &text, value, value_w);
        y += 1;
    }
    let Some(usage) = w.usage.as_ref().filter(|_| w.is_folder()) else { return };
    y += 1;
    if y + 2 >= inner.bottom() {
        return;
    }
    put(f, inner.x, y, "Space used", Style::new(c.accent, c.surface).with(Flags::BOLD), inner.w);
    let area = Rect::new(inner.x, y + 2, inner.w, inner.bottom().saturating_sub(y + 2));
    draw_usage(usage, f, area, ctx);
}

/// The bottom line: how far a measurement got, or what it couldn't read.
fn status(w: &InfoWindow) -> String {
    match &w.usage {
        Some(u) if !u.done => format!("Measuring… {} items", thousands(u.files + u.dirs)),
        Some(u) if u.unreadable > 0 => format!("{} items couldn't be read: the sizes leave them out", thousands(u.unreadable)),
        _ => String::new(),
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

/// "1.2 M (1,234,567 bytes)", or "247 bytes".
fn size_text(bytes: u64) -> String {
    if bytes < 1024 { plural(bytes, "byte", "bytes") } else { format!("{} ({} bytes)", human_size(bytes), thousands(bytes)) }
}

/// The label / value rows; rows the item has nothing for are left out.
pub fn facts(w: &InfoWindow) -> Vec<(&'static str, String)> {
    let mut rows = vec![("Name", w.name.clone()), ("Path", w.location.display())];
    let Some(e) = &w.entry else {
        rows.push(("Type", "…".into()));
        return rows;
    };
    let mut kind = if e.is_link {
        if e.is_dir { "Link to a folder".to_string() } else { "Link to a file".to_string() }
    } else if e.is_dir {
        "Folder".to_string()
    } else if e.is_executable {
        "Program".to_string()
    } else {
        match e.ext() {
            ext if ext.is_empty() => "File".to_string(),
            ext => format!("{} file", ext.to_uppercase()),
        }
    };
    if !w.location.is_local() {
        kind = format!("{kind} on {}", w.location.fs.type_name());
    }
    if e.is_hidden {
        kind.push_str(", hidden");
    }
    rows.push(("Type", kind));
    if let Some(target) = w.details.as_ref().and_then(|d| d.link_target.as_ref()) {
        rows.push(("Link to", target.display().to_string()));
    }
    if w.is_folder() {
        if let Some(u) = &w.usage {
            let so_far = if u.done { "" } else { " so far" };
            rows.push(("Size", format!("{}{so_far}", size_text(u.bytes))));
            rows.push(("Contains", format!("{}, {}", plural(u.files, "file", "files"), plural(u.dirs, "folder", "folders"))));
        }
        if let Some(free) = w.free {
            rows.push(("Free space", format!("{} on this disk", human_size(free))));
        }
    } else if !e.is_dir {
        rows.push(("Size", size_text(e.size)));
    }
    if e.is_online_only {
        rows.push(("Stored", "Online only: opening it downloads it".into()));
    }
    let date = |t: f64| Some(format_mtime(t)).filter(|s| !s.is_empty());
    if let Some(t) = date(e.mtime) {
        rows.push(("Modified", t));
    }
    if let Some(d) = &w.details {
        if let Some(t) = d.created.and_then(date) {
            rows.push(("Created", t));
        }
        if let Some(t) = d.accessed.and_then(date) {
            rows.push(("Accessed", t));
        }
        if let Some(p) = &d.permissions {
            rows.push((if cfg!(windows) { "Attributes" } else { "Permissions" }, p.clone()));
        }
        if let Some(o) = &d.owner {
            rows.push(("Owner", o.clone()));
        }
    }
    rows
}

/// Colors closer than this (RGB distance) are too alike to tell slices apart.
const DISTINCT: f32 = 48.0;

fn distance(a: Rgba, b: Rgba) -> f32 {
    let d = |x: u8, y: u8| (f32::from(x) - f32::from(y)).powi(2);
    (d(a.r, b.r) + d(a.g, b.g) + d(a.b, b.b)).sqrt()
}

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Rgba::rgb(m(a.r, b.r), m(a.g, b.g), m(a.b, b.b))
}

/// `n` slice colors from the color scheme: its accent and file colors,
/// leaving out ones too close to each other, to the surface or to the
/// others' gray (`muted`); past those, the same colors again in shades
/// toward the surface.
pub fn slice_colors(c: &Colors, n: usize) -> Vec<Rgba> {
    let mut base: Vec<Rgba> = Vec::new();
    for color in [c.accent, c.dir, c.exec, c.link, c.marked, c.success, c.warning, c.error, c.hidden] {
        if [c.surface, c.muted].into_iter().chain(base.iter().copied()).all(|o| distance(color, o) >= DISTINCT) {
            base.push(color);
        }
    }
    if base.is_empty() {
        base.push(c.foreground);
    }
    (0..n).map(|i| {
        let round = i / base.len();
        mix(base[i % base.len()], c.surface, (0.4 * round as f32).min(0.7))
    }).collect()
}

/// The legend's entries: (name, bytes, color), the others last.
pub fn legend(u: &Usage, c: &Colors) -> Vec<(String, u64, Rgba)> {
    let colors = slice_colors(c, u.parts.len());
    let mut items: Vec<(String, u64, Rgba)> = u
        .parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = if p.is_dir { format!("{}/", p.name) } else { p.name.clone() };
            (name, p.bytes, colors[i])
        })
        .collect();
    let (count, bytes) = u.others;
    if count > 0 && bytes > 0 {
        items.push((plural(count, "other item", "other items"), bytes, c.muted));
    }
    items
}

fn percent(part: u64, total: u64) -> String {
    match (part as f64 * 100.0 / total.max(1) as f64).round() as u64 {
        0 => "<1%".to_string(),
        p => format!("{p}%"),
    }
}

fn draw_usage(u: &Usage, f: &mut Frame, area: Rect, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    let muted = Style::new(c.muted, c.surface);
    if u.bytes == 0 {
        let text = if !u.done {
            "Measuring…"
        } else if u.files == 0 && u.dirs == 0 {
            "Empty folder"
        } else {
            "Only empty files"
        };
        put(f, area.x, area.y, text, muted, area.w);
        return;
    }
    let items = legend(u, c);
    let (cw, ch) = ctx.cell_px;
    // The chart takes up to the height and about 40 % of the width; a
    // narrow window shows only the legend.
    let max_cols = (area.w as f32 * 0.4).floor();
    let diameter = if area.w >= 40 && area.h >= 4 { (area.h as f32 * ch).min(max_cols * cw).floor() as u32 } else { 0 };
    let chart_cols = (diameter as f32 / cw).ceil() as u16;
    if diameter > 0 {
        let x0 = (area.x as f32 * cw).floor();
        let y0 = (area.y as f32 * ch + (area.h as f32 * ch - diameter as f32) / 2.0).floor();
        let slices: Vec<(f64, Rgba)> = items.iter().map(|(_, b, color)| (*b as f64, *color)).collect();
        for run in chart::donut(diameter, HOLE, &slices, c.surface) {
            let px = PxRect::new(x0 + run.x as f32, y0 + run.y as f32, run.len as f32, 1.0);
            f.add_rect(px, run.color, Layer::BelowText);
        }
        // The total in the hole, if it fits.
        let total = human_size(u.bytes);
        let hole_cols = (diameter as f32 * HOLE * 0.8 / cw).floor() as u16;
        if total.chars().count() as u16 <= hole_cols {
            let cy = ((y0 + diameter as f32 / 2.0) / ch).floor() as u16;
            let cx = area.x + (chart_cols.saturating_sub(total.chars().count() as u16)) / 2;
            put(f, cx, cy, &total, Style::new(c.foreground, c.surface).with(Flags::BOLD), hole_cols);
        }
    }
    let lx = if diameter > 0 { area.x + chart_cols + 3 } else { area.x };
    let lw = area.right().saturating_sub(lx);
    let top = area.y + (area.h.saturating_sub(items.len() as u16)) / 2;
    for (i, (name, bytes, color)) in items.iter().enumerate() {
        let y = top + i as u16;
        if y >= area.bottom() || lw < 12 {
            break;
        }
        fill(f, Rect::new(lx, y, 2, 1), *color);
        let numbers = format!("{:>6}  {:>4}", human_size(*bytes), percent(*bytes, u.bytes));
        let right = area.right();
        let numbers_x = right.saturating_sub(numbers.chars().count() as u16);
        put_elided(f, lx + 3, y, name, Style::new(c.foreground, c.surface), numbers_x.saturating_sub(lx + 4));
        put_right(f, right, y, &numbers, muted, lx + 3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_shares_in_words() {
        assert_eq!(size_text(1), "1 byte");
        assert_eq!(size_text(247), "247 bytes");
        assert_eq!(size_text(1_234_567), "1.2 M (1,234,567 bytes)");
        assert_eq!(percent(1, 1000), "<1%");
        assert_eq!(percent(1, 3), "33%");
    }

    #[test]
    fn slice_colors_are_told_apart() {
        let theme = crate::theme::Theme::default_dark();
        let c = &theme.colors;
        let colors = slice_colors(c, 12);
        assert_eq!(colors.len(), 12);
        for (i, a) in colors.iter().enumerate() {
            assert!(distance(*a, c.muted) > 1.0 && distance(*a, c.surface) > 1.0);
            assert!(colors[..i].iter().all(|b| a != b), "{i} repeats a color");
        }
    }
}
