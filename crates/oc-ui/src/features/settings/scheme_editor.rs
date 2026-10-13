//! The color scheme editor (port of `SchemeEditor` in `settings_dialog.py`):
//! a scheme's colors as hex values with swatches (a click, or Space on a
//! focused swatch, opens a [`ColorPicker`] titled with the color's role),
//! its name, dark flag and syntax colors, previewed live on the whole window. Built-in schemes are
//! read-only: changes to them are saved as a new scheme ("<name> (custom)").
//! Saving writes `color-schemes/<slug>.toml`; Delete removes a user scheme.
//! Both happen on a worker; the editor stays open until they are done.

use oc_config::ColorScheme;
use oc_config::themes::{self, COLOR_ROLES};
use oc_grid::{Frame, Rgba, Style};

use super::Slot;
use super::color_picker::{self, ColorPicker, PickerOutcome};
use super::form::{self, Choice, Popup, PopupOutcome};
use crate::app::{App, PlatformRequest};
use crate::dialogs::{self, dialog_rects, draw_box};
use crate::geom::Rect;
use crate::input::{Event, Key, MouseButton, MouseKind};
use crate::modal::{Modal, ModalId, ModalOutcome};
use crate::theme::Theme;
use crate::widgets::button::RowOutcome;
use crate::widgets::{ButtonRow, Checkbox, DrawCtx, TextInput, fill, put, width};

/// Pygments' styles (`sorted(get_all_styles())` of Pygments 2.19), the
/// values 1.x offers for "Syntax colors".
pub const PYGMENTS_STYLES: &[&str] = &[
    "abap",
    "algol",
    "algol_nu",
    "arduino",
    "autumn",
    "borland",
    "bw",
    "coffee",
    "colorful",
    "default",
    "dracula",
    "emacs",
    "friendly",
    "friendly_grayscale",
    "fruity",
    "github-dark",
    "gruvbox-dark",
    "gruvbox-light",
    "igor",
    "inkpot",
    "lightbulb",
    "lilypond",
    "lovelace",
    "manni",
    "material",
    "monokai",
    "murphy",
    "native",
    "night-owl",
    "nord",
    "nord-darker",
    "one-dark",
    "paraiso-dark",
    "paraiso-light",
    "pastie",
    "perldoc",
    "rainbow_dash",
    "rrt",
    "sas",
    "solarized-dark",
    "solarized-light",
    "staroffice",
    "stata-dark",
    "stata-light",
    "tango",
    "trac",
    "vim",
    "vs",
    "xcode",
    "zenburn",
];

pub const TITLE: &str = "Edit color scheme";
const WIDTH: u16 = 96;
/// Rows of the taller color column: Files (1 + 5), a gap, Messages (1 + 3),
/// a gap, Key bar (1 + 3).
const COLUMN_ROWS: u16 = 16;
const LABEL_W: u16 = 14;
const SWATCH_W: u16 = 5;
const HEX_W: u16 = 9;
const SYNTAX_W: u16 = 24;
/// 1.x: `hex_edit.setMaxLength(7)` ("#rrggbb").
const HEX_MAX: usize = 7;

/// What the editor's worker reports back (the editor itself is a boxed
/// modal the result can't reach).
#[derive(Default)]
pub struct EditorStatus {
    pub error: Option<String>,
    pub busy: bool,
    pub modal: Option<ModalId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Name,
    Dark,
    Syntax,
    /// A color's swatch (1.x: a button before its hex field in the Tab order).
    Swatch(usize),
    Hex(usize),
    Buttons,
}

pub struct SchemeEditor {
    slot: Slot,
    builtin: bool,
    /// The user scheme being edited (may be renamed); None for a built-in.
    original: Option<String>,
    scheme: ColorScheme,
    name: TextInput,
    dark: Checkbox,
    hex: Vec<TextInput>,
    buttons: ButtonRow,
    focus: Focus,
    popup: Option<Popup>,
    /// The color picker for a role, while open.
    picker: Option<(usize, ColorPicker)>,
    /// A swatch the mouse went down on (it opens the picker on release, like a button).
    pressed_swatch: Option<usize>,
}

/// Open the editor for the scheme in `slot`'s setting.
pub fn open(app: &mut App, slot: Slot) {
    let name = slot.value(&app.settings.appearance()).to_owned();
    let scheme = app.color_scheme(&name);
    let editor = SchemeEditor::new(slot, scheme);
    editor.preview(app);
    let id = app.show(editor);
    *app.state::<EditorStatus>() = EditorStatus { modal: Some(id), ..Default::default() };
}

impl SchemeEditor {
    pub fn new(slot: Slot, scheme: ColorScheme) -> Self {
        let builtin = themes::is_builtin(&scheme.name);
        let name = if builtin { format!("{} (custom)", scheme.name) } else { scheme.name.clone() };
        let hex = COLOR_ROLES.iter().map(|(role, _, _)| TextInput::new(scheme.color(role).unwrap_or_default())).collect();
        let labels: &[&str] = if builtin { &["Save", "Cancel"] } else { &["Save", "Delete", "Cancel"] };
        SchemeEditor {
            slot,
            builtin,
            original: (!builtin).then(|| scheme.name.clone()),
            dark: Checkbox::new("Dark scheme", scheme.dark),
            scheme,
            name: TextInput::selected_all(&name),
            hex,
            buttons: ButtonRow::new(labels, 0),
            focus: Focus::Name,
            popup: None,
            picker: None,
            pressed_swatch: None,
        }
    }

    /// Tab stops (`swatches`: also each color's swatch, as Tab goes in 1.x;
    /// ↑/↓ go from color to color).
    fn order(&self, swatches: bool) -> Vec<Focus> {
        let mut v = vec![Focus::Name, Focus::Dark, Focus::Syntax];
        for i in 0..COLOR_ROLES.len() {
            if swatches {
                v.push(Focus::Swatch(i));
            }
            v.push(Focus::Hex(i));
        }
        v.push(Focus::Buttons);
        v
    }

    fn move_focus(&mut self, delta: i32, swatches: bool) {
        let order = self.order(swatches);
        let at = match self.focus {
            Focus::Swatch(i) if !swatches => Focus::Hex(i),
            f => f,
        };
        let i = order.iter().position(|f| *f == at).unwrap_or(0) as i32;
        let n = order.len() as i32;
        self.focus = order[(i + delta).rem_euclid(n) as usize];
    }

    /// 1.x `_pick`: a color dialog titled with the role's label.
    fn open_picker(&mut self, i: usize) {
        let (role, label, _) = COLOR_ROLES[i];
        let color = self.scheme.color(role).and_then(Rgba::parse).unwrap_or(Rgba::BLACK);
        self.picker = Some((i, ColorPicker::new(label, color)));
    }

    fn picker_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        let Some((i, picker)) = self.picker.as_mut() else { return ModalOutcome::Ignored };
        let i = *i;
        match picker.handle_event(ev, screen, app.clipboard.as_mut()) {
            PickerOutcome::Consumed => {}
            PickerOutcome::Cancelled => self.picker = None,
            PickerOutcome::Chosen(color) => {
                self.picker = None;
                let hex = color_picker::to_hex(color);
                self.hex[i].set_text(&hex);
                if let Some(c) = self.scheme.color_mut(COLOR_ROLES[i].0) {
                    *c = hex;
                }
                self.preview(app);
            }
        }
        ModalOutcome::Consumed
    }

    /// Show the edited colors on the whole window.
    fn preview(&self, app: &mut App) {
        app.theme = Theme::with_scheme(&app.settings.appearance(), self.scheme.clone());
        app.request(PlatformRequest::ThemeChanged);
    }

    fn syntax_items() -> Vec<Choice> {
        PYGMENTS_STYLES.iter().map(|s| Some((*s).to_owned())).collect()
    }

    fn syntax_index(&self) -> usize {
        PYGMENTS_STYLES.iter().position(|s| *s == self.scheme.pygments_style).unwrap_or(0)
    }

    fn set_syntax(&mut self, app: &mut App, i: usize) {
        if let Some(s) = PYGMENTS_STYLES.get(i) {
            self.scheme.pygments_style = (*s).to_owned();
            self.preview(app);
        }
    }

    /// A hex field was edited: cut what went past [`HEX_MAX`] (Qt's max
    /// length keeps only the part of an insertion that fits; it ends at the
    /// cursor) and take the color once it is one.
    fn typed(&mut self, app: &mut App, i: usize) {
        let input = &mut self.hex[i];
        let excess = input.text.chars().count().saturating_sub(HEX_MAX);
        if excess > 0 {
            let start = input.text[..input.cursor].char_indices().rev().nth(excess - 1).map_or(0, |(j, _)| j);
            input.text.replace_range(start..input.cursor, "");
            input.cursor = start;
            input.anchor = None;
            if let Some((end, _)) = input.text.char_indices().nth(HEX_MAX) {
                input.text.truncate(end);
                input.cursor = input.cursor.min(end);
            }
        }
        let text = self.hex[i].text.trim().to_owned();
        if themes::is_color(&text)
            && let Some(c) = self.scheme.color_mut(COLOR_ROLES[i].0)
        {
            *c = text.to_lowercase();
            self.preview(app);
        }
    }

    // -- layout ------------------------------------------------------------

    fn content_rows(&self) -> u16 {
        u16::from(self.builtin) + 3 + COLUMN_ROWS + 3
    }

    /// Everything's place: (outer, content, name, dark, syntax, color rows, error, buttons).
    fn rects(&self, screen: Rect) -> Layout {
        let (outer, content) = dialog_rects(screen, WIDTH, self.content_rows());
        let x = content.x;
        let mut y = content.y;
        let note = self.builtin.then(|| {
            y += 1;
            Rect::new(x, y - 1, content.w, 1)
        });
        let name = Rect::new(x + LABEL_W, y, content.w.saturating_sub(LABEL_W), 1);
        let name_label = Rect::new(x, y, LABEL_W, 1);
        y += 1;
        let dark = Rect::new(x + LABEL_W, y, 20, 1);
        // Right-aligned, but never over the dark checkbox and its own label.
        let sx = content.right().saturating_sub(SYNTAX_W).max(dark.right() + width("Syntax colors") + 2);
        let syntax = Rect::new(sx, y, content.right().saturating_sub(sx), 1);
        y += 2;
        let buttons = Rect::new(x, content.bottom().saturating_sub(1), content.w, 1);
        let error = Rect::new(x, (y + COLUMN_ROWS).min(buttons.y.saturating_sub(1)), content.w, 1);
        let col_w = content.w.saturating_sub(3) / 2;
        let mut rows = Vec::new();
        let mut headings = Vec::new();
        for (col, groups) in [&["General", "Cursor"][..], &["Files", "Messages", "Key bar"][..]].iter().enumerate() {
            let cx = x + col as u16 * (col_w + 3);
            let mut cy = y;
            for (gi, group) in groups.iter().enumerate() {
                if gi > 0 {
                    cy += 1;
                }
                // In a short window the rows that don't fit are left out
                // (they stay reachable with Tab, just not shown).
                if cy < error.y {
                    headings.push((Rect::new(cx, cy, col_w, 1), *group));
                }
                cy += 1;
                for (i, (_, _, g)) in COLOR_ROLES.iter().enumerate() {
                    if g == group && cy >= error.y {
                        cy += 1;
                    } else if g == group {
                        let hex = Rect::new((cx + col_w).saturating_sub(HEX_W), cy, HEX_W, 1);
                        let swatch = Rect::new(hex.x.saturating_sub(1 + SWATCH_W), cy, SWATCH_W, 1);
                        rows.push((i, Rect::new(cx, cy, swatch.x.saturating_sub(cx + 1), 1), swatch, hex));
                        cy += 1;
                    }
                }
            }
        }
        rows.sort_by_key(|r| r.0);
        Layout { outer, note, name_label, name, dark, syntax, headings, rows, error, buttons }
    }

    // -- results -------------------------------------------------------------

    fn close(&self, app: &mut App) -> ModalOutcome {
        app.reload_theme();
        app.state::<EditorStatus>().modal = None;
        ModalOutcome::Close
    }

    fn save(&mut self, app: &mut App) {
        let mut scheme = self.scheme.clone();
        scheme.name = self.name.text.trim().to_owned();
        scheme.dark = self.dark.checked;
        let original = self.original.clone();
        let dir = app.paths.color_schemes_dir();
        let slot = self.slot;
        app.state::<EditorStatus>().busy = true;
        app.tasks.run_in_background(
            move |_| match themes::save_user_scheme(&dir, &scheme, original.as_deref()) {
                Ok(_) => Ok((scheme.name, themes::all_schemes(&dir))),
                Err(e) => Err(e.to_string()),
            },
            move |app, result| {
                let status = app.state::<EditorStatus>();
                status.busy = false;
                match result {
                    Ok((name, all)) => {
                        status.error = None;
                        if let Some(id) = status.modal.take() {
                            app.close_modal(id);
                        }
                        app.state::<crate::theme::SchemeCache>().all = Some(all);
                        app.update_settings(move |s| slot.set(s, &name));
                        app.reload_theme();
                    }
                    Err(e) => status.error = Some(e),
                }
            },
        );
    }

    fn delete(&self, app: &mut App) {
        let Some(name) = self.original.clone() else { return };
        let dir = app.paths.color_schemes_dir();
        let question = format!("Delete '{name}'?");
        dialogs::confirm(app, "Delete color scheme", &question, "Delete", move |app| {
            app.state::<EditorStatus>().busy = true;
            app.tasks.run_in_background(
                move |_| match themes::delete_user_scheme(&dir, &name) {
                    Ok(()) => Ok((name, themes::all_schemes(&dir))),
                    Err(e) => Err(e.to_string()),
                },
                |app, result| {
                    let status = app.state::<EditorStatus>();
                    status.busy = false;
                    match result {
                        Ok((name, all)) => {
                            if let Some(id) = status.modal.take() {
                                app.close_modal(id);
                            }
                            app.state::<crate::theme::SchemeCache>().all = Some(all);
                            // Whatever used it goes back to the default.
                            app.update_settings(move |s| {
                                for slot in [Slot::Color, Slot::Light, Slot::Dark] {
                                    if slot.raw(s) == name {
                                        slot.set(s, "");
                                    }
                                }
                            });
                            app.reload_theme();
                        }
                        Err(e) => status.error = Some(e),
                    }
                },
            );
        });
    }

    fn press(&mut self, app: &mut App, i: usize) -> ModalOutcome {
        match self.buttons.buttons[i].label.as_str() {
            "Save" => {
                self.save(app);
                ModalOutcome::Consumed
            }
            "Delete" => {
                self.delete(app);
                ModalOutcome::Consumed
            }
            _ => self.close(app),
        }
    }

    fn popup_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        let items = Self::syntax_items();
        let Some(popup) = self.popup.as_mut() else { return ModalOutcome::Ignored };
        let outcome = match ev {
            Event::Key(k) => popup.handle_key(k, &items),
            Event::Mouse(m) => popup.handle_mouse(m, &items, screen),
            _ => PopupOutcome::Consumed,
        };
        match outcome {
            PopupOutcome::Chosen(i) => {
                self.popup = None;
                self.set_syntax(app, i);
            }
            PopupOutcome::Closed => self.popup = None,
            PopupOutcome::Consumed => {}
        }
        ModalOutcome::Consumed
    }
}

struct Layout {
    outer: Rect,
    note: Option<Rect>,
    name_label: Rect,
    name: Rect,
    dark: Rect,
    syntax: Rect,
    headings: Vec<(Rect, &'static str)>,
    /// (role index, label, swatch, hex field), in role order.
    rows: Vec<(usize, Rect, Rect, Rect)>,
    error: Rect,
    buttons: Rect,
}

impl Modal for SchemeEditor {
    fn title(&self) -> &str {
        TITLE
    }

    fn text(&self) -> String {
        let mut text = format!("{}\n{}\nfocus={:?}", self.name.text, self.scheme.name, self.focus);
        if let Some((_, p)) = &self.picker {
            text = format!("{text}\n{}", p.text());
        }
        text
    }

    fn wants_text_input(&self) -> bool {
        if let Some((_, p)) = &self.picker {
            return p.wants_text_input();
        }
        matches!(self.focus, Focus::Name | Focus::Hex(_)) && self.popup.is_none()
    }

    fn draw(&self, app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let l = self.rects(screen);
        draw_box(f, l.outer, TITLE, ctx);
        if let Some(r) = l.note {
            let text = format!("'{}' is built in: your changes are saved as a new color scheme.", self.scheme.name);
            form::muted(f, r.x, r.y, &text, r.w, ctx);
        }
        form::label(f, l.name_label.x, l.name_label.y, "Name", l.name_label.w, ctx);
        self.name.draw(f, l.name, ctx, self.focus == Focus::Name);
        self.dark.draw(f, l.dark, ctx, self.focus == Focus::Dark);
        let syntax_label = "Syntax colors";
        let lx = l.syntax.x.saturating_sub(width(syntax_label) + 1);
        form::label(f, lx, l.syntax.y, syntax_label, width(syntax_label), ctx);
        form::draw_combo(f, l.syntax, &self.scheme.pygments_style, self.focus == Focus::Syntax, ctx);
        for (r, text) in &l.headings {
            form::heading(f, r.x, r.y, text, r.w, ctx);
        }
        for (i, label, swatch, hex) in &l.rows {
            let (role, text, _) = COLOR_ROLES[*i];
            form::label(f, label.x, label.y, text, label.w, ctx);
            let color = self.scheme.color(role).and_then(Rgba::parse).unwrap_or(c.background);
            fill(f, *swatch, color);
            // A thin outline so a swatch in the surface color still shows (1.x: 1px muted
            // border; focused: 2px in the active border color).
            if self.focus == Focus::Swatch(*i) {
                put(f, swatch.x, swatch.y, "▕", Style::new(c.border_active, color), 1);
                put(f, swatch.right().saturating_sub(1), swatch.y, "▏", Style::new(c.border_active, color), 1);
            } else {
                put(f, swatch.x, swatch.y, "▏", Style::new(c.muted, color), 1);
            }
            self.hex[*i].draw(f, *hex, ctx, self.focus == Focus::Hex(*i));
        }
        let status = app.try_state::<EditorStatus>();
        if let Some(err) = status.and_then(|s| s.error.as_deref()) {
            put(f, l.error.x, l.error.y, err, Style::new(c.error, c.surface), l.error.w);
        } else if status.is_some_and(|s| s.busy) {
            form::muted(f, l.error.x, l.error.y, "Saving…", l.error.w, ctx);
        }
        self.buttons.draw(f, l.buttons, ctx, self.focus == Focus::Buttons);
        if let Some(p) = &self.popup {
            p.draw(f, &Self::syntax_items(), screen, ctx);
        }
        if let Some((_, p)) = &self.picker {
            p.draw(f, screen, ctx);
        }
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        if app.try_state::<EditorStatus>().is_some_and(|s| s.busy) {
            return ModalOutcome::Consumed;
        }
        if self.popup.is_some() {
            return self.popup_event(app, ev, screen);
        }
        if self.picker.is_some() {
            return self.picker_event(app, ev, screen);
        }
        match ev {
            Event::Key(k) => {
                let plain = k.mods.is_empty();
                match k.key {
                    Key::Escape => return self.close(app),
                    Key::Tab if k.mods.is_empty() || k.mods == crate::input::Mods::SHIFT => {
                        self.move_focus(if k.mods.shift { -1 } else { 1 }, true);
                        return ModalOutcome::Consumed;
                    }
                    Key::Up | Key::Down if plain => {
                        self.move_focus(if k.key == Key::Up { -1 } else { 1 }, false);
                        return ModalOutcome::Consumed;
                    }
                    _ => {}
                }
                match self.focus {
                    Focus::Buttons => {
                        if let RowOutcome::Activated(i) = self.buttons.handle_key(k, true) {
                            return self.press(app, i);
                        }
                    }
                    Focus::Syntax => match k.key {
                        Key::Enter | Key::Space if plain => {
                            let l = self.rects(screen);
                            self.popup = Some(Popup::open(0, l.syntax, &Self::syntax_items(), self.syntax_index()));
                        }
                        Key::Left | Key::Right if plain => {
                            let i = form::step(&Self::syntax_items(), self.syntax_index(), if k.key == Key::Left { -1 } else { 1 });
                            self.set_syntax(app, i);
                        }
                        _ => {}
                    },
                    Focus::Dark => {
                        if k.key == Key::Enter && plain {
                            self.save(app);
                        } else if self.dark.handle_key(k, true) {
                            self.scheme.dark = self.dark.checked;
                            self.preview(app);
                        }
                    }
                    Focus::Name => {
                        if k.key == Key::Enter && plain {
                            self.save(app);
                        } else {
                            self.name.handle_key(k, app.clipboard.as_mut());
                        }
                    }
                    Focus::Swatch(i) => match k.key {
                        Key::Space if plain => self.open_picker(i),
                        Key::Enter if plain => self.save(app),
                        _ => {}
                    },
                    Focus::Hex(i) => {
                        if k.key == Key::Enter && plain {
                            self.save(app);
                        } else if self.hex[i].handle_key(k, app.clipboard.as_mut()) {
                            self.typed(app, i);
                        }
                    }
                }
                ModalOutcome::Consumed
            }
            Event::Commit(_) | Event::Preedit(_) => {
                match self.focus {
                    Focus::Name => {
                        self.name.handle_event(ev, app.clipboard.as_mut());
                    }
                    Focus::Hex(i) => {
                        self.hex[i].handle_event(ev, app.clipboard.as_mut());
                        self.typed(app, i);
                    }
                    _ => {}
                }
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => {
                let l = self.rects(screen);
                let (col, row) = (m.col, m.row);
                match m.kind {
                    MouseKind::Down(MouseButton::Left) => {
                        if l.name.contains(col, row) {
                            self.focus = Focus::Name;
                            self.name.click(l.name, col);
                        } else if l.dark.contains(col, row) {
                            self.focus = Focus::Dark;
                            self.dark.checked = !self.dark.checked;
                            self.scheme.dark = self.dark.checked;
                            self.preview(app);
                        } else if l.syntax.contains(col, row) {
                            self.focus = Focus::Syntax;
                            self.popup = Some(Popup::open(0, l.syntax, &Self::syntax_items(), self.syntax_index()));
                        } else if let Some((i, _, _, _)) = l.rows.iter().find(|(_, _, swatch, _)| swatch.contains(col, row)) {
                            self.focus = Focus::Swatch(*i);
                            self.pressed_swatch = Some(*i);
                        } else if let Some((i, _, _, hex)) = l.rows.iter().find(|(_, _, _, hex)| hex.contains(col, row)) {
                            self.focus = Focus::Hex(*i);
                            self.hex[*i].click(*hex, col);
                        }
                    }
                    MouseKind::Up(MouseButton::Left) => {
                        if let Some(i) = self.pressed_swatch.take()
                            && l.rows.iter().any(|(j, _, swatch, _)| *j == i && swatch.contains(col, row))
                        {
                            self.open_picker(i);
                            return ModalOutcome::Consumed;
                        }
                        if let Some(i) = self.buttons.hit(l.buttons, col, row) {
                            self.focus = Focus::Buttons;
                            self.buttons.focus = i;
                            return self.press(app, i);
                        }
                    }
                    _ => {}
                }
                ModalOutcome::Consumed
            }
            _ => ModalOutcome::Consumed,
        }
    }
}
