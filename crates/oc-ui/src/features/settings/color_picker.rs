//! A color picker in the grid, for the scheme editor's swatches (1.x:
//! `QColorDialog.getColor(color, parent, label)`): Qt's 48 basic colors, the
//! hue/saturation field with the value bar beside it, the numbers, an HTML
//! (`#rrggbb`) field, OK and Cancel. The scheme editor owns it while it is
//! open (like its syntax list) and takes the color on OK.

use oc_grid::{Frame, Rgba, Style};

use crate::clipboard::Clipboard;
use crate::dialogs::{dialog_rects, draw_box};
use crate::geom::Rect;
use crate::input::{Event, Key, Mods, MouseButton, MouseKind};
use crate::widgets::button::RowOutcome;
use crate::widgets::{ButtonRow, DrawCtx, TextInput, fill, put};

use super::form;

const WIDTH: u16 = 74;
const ROWS: u16 = 13;
/// Basic colors: 6 rows × 8 columns of swatches this wide, a column apart.
const WELL_ROWS: u16 = 6;
const WELL_COLS: u16 = 8;
const WELL_W: u16 = 4;
/// The hue/saturation field (cells; each holds two rows of colors) and the value bar.
const FIELD_X: u16 = 42;
const FIELD_W: u16 = 24;
const BAR_X: u16 = FIELD_X + FIELD_W + 1;
const BAR_W: u16 = 2;
/// The field is drawn at this value, like Qt's.
const FIELD_VALUE: i32 = 200;
/// 1.x's hex fields take at most this much ("#rrggbb").
const HTML_MAX: usize = 7;

/// Qt's standard colors (`qcolordialog.cpp`, `initRgb`), shown column by column.
fn basic_color(i: usize) -> Rgba {
    let (g, r, b) = (i / 12, i / 3 % 4, i % 3);
    Rgba::rgb((r * 255 / 3) as u8, (g * 255 / 3) as u8, (b * 255 / 2) as u8)
}

/// HSV (hue 0–359, saturation and value 0–255) → RGB, like `QColor::fromHsv`.
pub fn hsv_to_rgb(h: i32, s: i32, v: i32) -> Rgba {
    let (h, s, v) = (h.rem_euclid(360) as f64 / 60.0, s.clamp(0, 255) as f64 / 255.0, v.clamp(0, 255) as f64 / 255.0);
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let byte = |f: f64| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    Rgba::rgb(byte(r), byte(g), byte(b))
}

/// RGB → (hue or None for grays, saturation, value).
pub fn rgb_to_hsv(c: Rgba) -> (Option<i32>, i32, i32) {
    let (r, g, b) = (c.r as f64, c.g as f64, c.b as f64);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let s = if max == 0.0 { 0.0 } else { d / max * 255.0 };
    if d == 0.0 {
        return (None, s.round() as i32, max as i32);
    }
    let h = if max == r {
        60.0 * ((g - b) / d)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (Some((h.round() as i32).rem_euclid(360)), s.round() as i32, max as i32)
}

pub fn to_hex(c: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Basic,
    Field,
    Value,
    Html,
    Buttons,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drag {
    Field,
    Value,
}

/// What an event did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerOutcome {
    Consumed,
    /// OK: this color.
    Chosen(Rgba),
    Cancelled,
}

pub struct ColorPicker {
    title: String,
    initial: Rgba,
    color: Rgba,
    hue: i32,
    sat: i32,
    val: i32,
    /// The basic color under the keyboard cursor.
    well: usize,
    html: TextInput,
    buttons: ButtonRow,
    focus: Focus,
    drag: Option<Drag>,
}

struct Layout {
    outer: Rect,
    heading: Rect,
    wells: Rect,
    field: Rect,
    bar: Rect,
    swatch: Rect,
    numbers: Rect,
    html_label: Rect,
    html: Rect,
    buttons: Rect,
}

impl ColorPicker {
    /// A picker titled `title` (the color's role, like 1.x), starting at `color`.
    pub fn new(title: &str, color: Rgba) -> Self {
        let mut p = ColorPicker {
            title: title.to_owned(),
            initial: color,
            color,
            hue: 0,
            sat: 0,
            val: 0,
            well: (0..48).find(|&i| basic_color(i) == color).unwrap_or(0),
            html: TextInput::default(),
            buttons: ButtonRow::new(&["OK", "Cancel"], 0),
            focus: Focus::Basic,
            drag: None,
        };
        p.set_rgb(color);
        p
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// The color as picked so far.
    pub fn color(&self) -> Rgba {
        self.color
    }

    pub fn wants_text_input(&self) -> bool {
        self.focus == Focus::Html
    }

    fn set_rgb(&mut self, c: Rgba) {
        let (h, s, v) = rgb_to_hsv(c);
        if let Some(h) = h {
            self.hue = h; // grays keep the hue they had
        }
        (self.sat, self.val) = (s, v);
        self.color = c.with_alpha(255);
        self.html.set_text(&to_hex(self.color));
    }

    fn set_hsv(&mut self, h: i32, s: i32, v: i32) {
        (self.hue, self.sat, self.val) = (h.clamp(0, 359), s.clamp(0, 255), v.clamp(0, 255));
        self.color = hsv_to_rgb(self.hue, self.sat, self.val);
        self.html.set_text(&to_hex(self.color));
    }

    fn rects(&self, screen: Rect) -> Layout {
        let (outer, c) = dialog_rects(screen, WIDTH, ROWS);
        let at = |x: u16, y: u16, w: u16, h: u16| {
            let x = (c.x + x).min(c.right());
            let y = (c.y + y).min(c.bottom());
            Rect::new(x, y, w.min(c.right().saturating_sub(x)), h.min(c.bottom().saturating_sub(y)))
        };
        Layout {
            outer,
            heading: at(0, 0, FIELD_X - 2, 1),
            wells: at(0, 1, WELL_COLS * (WELL_W + 1), WELL_ROWS),
            field: at(FIELD_X, 1, FIELD_W, WELL_ROWS),
            bar: at(BAR_X, 1, BAR_W, WELL_ROWS),
            swatch: at(0, 8, 8, 2),
            numbers: at(12, 8, 40, 2),
            html_label: at(12, 10, 6, 1),
            html: at(18, 10, 10, 1),
            buttons: at(0, ROWS - 1, c.w, 1),
        }
    }

    /// The basic color well at a cell.
    fn well_at(l: &Layout, col: u16, row: u16) -> Option<usize> {
        if !l.wells.contains(col, row) {
            return None;
        }
        let (dx, dy) = (col - l.wells.x, row - l.wells.y);
        (dx % (WELL_W + 1) < WELL_W).then(|| (dx / (WELL_W + 1) * WELL_ROWS + dy) as usize)
    }

    /// Half-rows of the field and the bar (each cell shows two).
    fn halves(r: Rect) -> i32 {
        (r.h as i32 * 2).max(2)
    }

    fn field_hue(r: Rect, x: i32) -> i32 {
        360 - x.clamp(0, r.w as i32 - 1) * 360 / (r.w as i32 - 1).max(1)
    }

    fn half_level(r: Rect, y: i32) -> i32 {
        255 - y.clamp(0, Self::halves(r) - 1) * 255 / (Self::halves(r) - 1)
    }

    /// The field's cell (column, half-row) for the current hue and saturation.
    fn field_pos(&self, r: Rect) -> (i32, i32) {
        ((360 - self.hue) * (r.w as i32 - 1).max(1) / 360, (255 - self.sat) * (Self::halves(r) - 1) / 255)
    }

    fn value_pos(&self, r: Rect) -> i32 {
        (255 - self.val) * (Self::halves(r) - 1) / 255
    }

    fn pick_field(&mut self, r: Rect, x: i32, half: i32) {
        self.set_hsv(Self::field_hue(r, x).min(359), Self::half_level(r, half), self.val);
    }

    fn pick_value(&mut self, r: Rect, half: i32) {
        self.set_hsv(self.hue, self.sat, Self::half_level(r, half));
    }

    pub fn draw(&self, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let l = self.rects(screen);
        draw_box(f, l.outer, &self.title, ctx);
        form::heading(f, l.heading.x, l.heading.y, "Basic colors", l.heading.w, ctx);
        for i in 0..(WELL_ROWS * WELL_COLS) as usize {
            let (col, row) = (i as u16 / WELL_ROWS, i as u16 % WELL_ROWS);
            let r = Rect::new(l.wells.x + col * (WELL_W + 1), l.wells.y + row, WELL_W, 1);
            if r.right() > l.wells.right() || r.y >= l.wells.bottom() {
                continue;
            }
            let color = basic_color(i);
            fill(f, r, color);
            if i == self.well && (self.focus == Focus::Basic || color == self.color) {
                let mark = if self.focus == Focus::Basic { c.border_active } else { contrast(color) };
                put(f, r.x, r.y, "▕", Style::new(mark, color), 1);
                put(f, r.right() - 1, r.y, "▏", Style::new(mark, color), 1);
            }
        }
        // Hue across, saturation down (two levels per cell: ▀ in the upper color on the lower).
        let half = |r: Rect, y: i32| Self::half_level(r, y);
        for x in 0..l.field.w {
            let hue = Self::field_hue(l.field, x as i32).min(359);
            for y in 0..l.field.h {
                let top = hsv_to_rgb(hue, half(l.field, y as i32 * 2), FIELD_VALUE);
                let bottom = hsv_to_rgb(hue, half(l.field, y as i32 * 2 + 1), FIELD_VALUE);
                put(f, l.field.x + x, l.field.y + y, "▀", Style::new(top, bottom), 1);
            }
        }
        let (fx, fy) = self.field_pos(l.field);
        if !l.field.is_empty() {
            let (x, y) = (l.field.x + fx as u16, l.field.y + (fy / 2) as u16);
            let under = hsv_to_rgb(self.hue, self.sat, FIELD_VALUE);
            let mark = if self.focus == Focus::Field { c.border_active } else { contrast(under) };
            put(f, x, y, "┼", Style::new(mark, under), 1);
        }
        for y in 0..l.bar.h {
            let top = hsv_to_rgb(self.hue, self.sat, half(l.bar, y as i32 * 2));
            let bottom = hsv_to_rgb(self.hue, self.sat, half(l.bar, y as i32 * 2 + 1));
            for x in 0..l.bar.w {
                put(f, l.bar.x + x, l.bar.y + y, "▀", Style::new(top, bottom), 1);
            }
        }
        if !l.bar.is_empty() {
            let y = l.bar.y + (self.value_pos(l.bar) / 2) as u16;
            let mark = if self.focus == Focus::Value { c.border_active } else { c.foreground };
            put(f, l.bar.right(), y, "◀", Style::new(mark, c.surface), 1);
        }
        // The new color over the one it started from (Qt shows the current one).
        fill(f, Rect::new(l.swatch.x, l.swatch.y, l.swatch.w, l.swatch.h.min(1)), self.color);
        if l.swatch.h > 1 {
            fill(f, Rect::new(l.swatch.x, l.swatch.y + 1, l.swatch.w, 1), self.initial);
        }
        let numbers = [
            format!("Hue: {:>3}   Sat: {:>3}   Val: {:>3}", self.hue, self.sat, self.val),
            format!("Red: {:>3}   Green: {:>3}   Blue: {:>3}", self.color.r, self.color.g, self.color.b),
        ];
        for (i, line) in numbers.iter().enumerate().take(l.numbers.h as usize) {
            form::label(f, l.numbers.x, l.numbers.y + i as u16, line, l.numbers.w, ctx);
        }
        form::label(f, l.html_label.x, l.html_label.y, "HTML:", l.html_label.w, ctx);
        self.html.draw(f, l.html, ctx, self.focus == Focus::Html);
        self.buttons.draw(f, l.buttons, ctx, self.focus == Focus::Buttons);
    }

    fn move_focus(&mut self, delta: i32) {
        const ORDER: [Focus; 5] = [Focus::Basic, Focus::Field, Focus::Value, Focus::Html, Focus::Buttons];
        let i = ORDER.iter().position(|f| *f == self.focus).unwrap_or(0) as i32;
        self.focus = ORDER[(i + delta).rem_euclid(ORDER.len() as i32) as usize];
    }

    /// A key, click or drag; `screen` is the window's grid.
    pub fn handle_event(&mut self, ev: &Event, screen: Rect, clip: &mut dyn Clipboard) -> PickerOutcome {
        let l = self.rects(screen);
        match ev {
            Event::Key(k) => {
                let plain = k.mods.is_empty();
                match k.key {
                    Key::Escape => return PickerOutcome::Cancelled,
                    Key::Tab if plain || k.mods == Mods::SHIFT => {
                        self.move_focus(if k.mods.shift { -1 } else { 1 });
                        return PickerOutcome::Consumed;
                    }
                    Key::Enter if plain && self.focus != Focus::Buttons => return PickerOutcome::Chosen(self.color),
                    _ => {}
                }
                let step = |key: Key| match key {
                    Key::Left => (-1, 0),
                    Key::Right => (1, 0),
                    Key::Up => (0, -1),
                    Key::Down => (0, 1),
                    _ => (0, 0),
                };
                match self.focus {
                    Focus::Basic if plain => {
                        let (dx, dy) = step(k.key);
                        let (col, row) = ((self.well as i32) / WELL_ROWS as i32 + dx, (self.well as i32) % WELL_ROWS as i32 + dy);
                        if (dx, dy) != (0, 0) && (0..WELL_COLS as i32).contains(&col) && (0..WELL_ROWS as i32).contains(&row) {
                            self.well = (col * WELL_ROWS as i32 + row) as usize;
                        }
                        if (dx, dy) != (0, 0) || k.key == Key::Space {
                            self.set_rgb(basic_color(self.well));
                        }
                    }
                    Focus::Field if plain => {
                        let (dx, dy) = step(k.key);
                        if (dx, dy) != (0, 0) {
                            let (x, y) = self.field_pos(l.field);
                            self.pick_field(l.field, x + dx, y + dy);
                        }
                    }
                    Focus::Value if plain => {
                        let dy = match k.key {
                            Key::PageUp => -4,
                            Key::PageDown => 4,
                            other => step(other).1,
                        };
                        if dy != 0 {
                            let y = self.value_pos(l.bar);
                            self.pick_value(l.bar, y + dy);
                        }
                    }
                    Focus::Html => {
                        if self.html.handle_key(k, clip) {
                            self.typed();
                        }
                    }
                    Focus::Buttons => {
                        if let RowOutcome::Activated(i) = self.buttons.handle_key(k, true) {
                            return self.press(i);
                        }
                    }
                    _ => {}
                }
                PickerOutcome::Consumed
            }
            Event::Commit(_) | Event::Preedit(_) if self.focus == Focus::Html => {
                self.html.handle_event(ev, clip);
                self.typed();
                PickerOutcome::Consumed
            }
            Event::Mouse(m) => {
                let (col, row) = (m.col, m.row);
                match m.kind {
                    MouseKind::Down(MouseButton::Left) | MouseKind::DoubleClick(MouseButton::Left) => {
                        if let Some(i) = Self::well_at(&l, col, row) {
                            self.focus = Focus::Basic;
                            self.well = i;
                            self.set_rgb(basic_color(i));
                        } else if l.field.contains(col, row) {
                            self.focus = Focus::Field;
                            self.drag = Some(Drag::Field);
                            self.pick_field(l.field, (col - l.field.x) as i32, (row - l.field.y) as i32 * 2);
                        } else if l.bar.contains(col, row) {
                            self.focus = Focus::Value;
                            self.drag = Some(Drag::Value);
                            self.pick_value(l.bar, (row - l.bar.y) as i32 * 2);
                        } else if l.html.contains(col, row) {
                            self.focus = Focus::Html;
                            self.html.click(l.html, col);
                        }
                    }
                    MouseKind::Move => match self.drag {
                        Some(Drag::Field) => {
                            let (x, y) = (col as i32 - l.field.x as i32, row as i32 - l.field.y as i32);
                            self.pick_field(l.field, x, y * 2);
                        }
                        Some(Drag::Value) => self.pick_value(l.bar, (row as i32 - l.bar.y as i32) * 2),
                        None => {}
                    },
                    MouseKind::Up(MouseButton::Left) => {
                        self.drag = None;
                        if let Some(i) = self.buttons.hit(l.buttons, col, row) {
                            self.focus = Focus::Buttons;
                            self.buttons.focus = i;
                            return self.press(i);
                        }
                    }
                    _ => {}
                }
                PickerOutcome::Consumed
            }
            _ => PickerOutcome::Consumed,
        }
    }

    fn press(&self, i: usize) -> PickerOutcome {
        if i == 0 { PickerOutcome::Chosen(self.color) } else { PickerOutcome::Cancelled }
    }

    /// The HTML field changed: at most 7 characters (cut at the cursor, as in
    /// the scheme editor), the color once it is one.
    fn typed(&mut self) {
        let input = &mut self.html;
        while input.text.chars().count() > HTML_MAX && input.cursor > 0 {
            let prev = input.text[..input.cursor].char_indices().next_back().map_or(0, |(j, _)| j);
            input.text.replace_range(prev..input.cursor, "");
            input.cursor = prev;
        }
        let text = input.text.trim().to_owned();
        if oc_config::themes::is_color(&text)
            && let Some(c) = Rgba::parse(&text)
        {
            let (h, s, v) = rgb_to_hsv(c);
            if let Some(h) = h {
                self.hue = h;
            }
            (self.sat, self.val, self.color) = (s, v, c);
        }
    }

    /// Tests: the picker's state in words.
    pub fn text(&self) -> String {
        format!("picker={} color={} focus={:?}", self.title, to_hex(self.color), self.focus)
    }
}

/// Black or white, whichever shows on `bg`.
fn contrast(bg: Rgba) -> Rgba {
    let luma = bg.r as u32 * 299 + bg.g as u32 * 587 + bg.b as u32 * 114;
    if luma > 128_000 { Rgba::BLACK } else { Rgba::WHITE }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_colors_are_qts() {
        assert_eq!(basic_color(0), Rgba::rgb(0, 0, 0));
        assert_eq!(basic_color(2), Rgba::rgb(0, 0, 255));
        assert_eq!(basic_color(3), Rgba::rgb(85, 0, 0));
        assert_eq!(basic_color(47), Rgba::rgb(255, 255, 255));
    }

    #[test]
    fn hsv_round_trips() {
        for c in [Rgba::rgb(255, 0, 0), Rgba::rgb(18, 52, 86), Rgba::rgb(171, 205, 239), Rgba::rgb(0, 255, 128)] {
            let (h, s, v) = rgb_to_hsv(c);
            let back = hsv_to_rgb(h.unwrap(), s, v);
            assert!((back.r as i32 - c.r as i32).abs() <= 2, "{c:?} {back:?}");
            assert!((back.g as i32 - c.g as i32).abs() <= 2, "{c:?} {back:?}");
            assert!((back.b as i32 - c.b as i32).abs() <= 2, "{c:?} {back:?}");
        }
        assert_eq!(rgb_to_hsv(Rgba::rgb(128, 128, 128)), (None, 0, 128));
    }
}
