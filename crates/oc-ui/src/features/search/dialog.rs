//! The Find files dialog (Alt+F7; port of `ui/search_ui.py`).
//!
//! Three sections like 1.x: Names (masks, RegEx, case, where, depth, kinds,
//! hidden files, inside archives), Contents (text, RegEx, case, whole words,
//! NOT containing, cloud-only files) and Size and date ("500k" / "10M" /
//! "1.5G" limits, modification presets). The entries are kept for the
//! session ([`Form`]). Keys: Tab / Shift+Tab or ↑/↓ move between fields,
//! except that ↑/↓ change a choice (combo boxes in 1.x); Space toggles a
//! checkbox or opens a choice (so do F4 and Alt+↓), Enter searches, Esc
//! cancels.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use oc_grid::{Flags, Frame, Style};
use oc_search::{Kinds, NameMatcher, Query, TextMatcher};
use oc_vfs::VPath;

use crate::app::App;
use crate::dialogs::{dialog_rects, draw_box};
use crate::geom::Rect;
use crate::input::{Event, Key, KeyEvent, MouseButton, MouseKind};
use crate::modal::{Modal, ModalId, ModalOutcome};
use crate::widgets::button::RowOutcome;
use crate::widgets::{ButtonRow, Checkbox, DrawCtx, DropdownMenu, MenuEntry, MenuOutcome, TextInput, fill, put, width};

pub const TITLE: &str = "Find files";

/// Depth choices: label, `Query::max_depth`.
pub const DEPTHS: [(&str, Option<u32>); 6] = [
    ("All subfolders", None),
    ("This folder only", Some(0)),
    ("1 level", Some(1)),
    ("2 levels", Some(2)),
    ("3 levels", Some(3)),
    ("5 levels", Some(5)),
];
pub const KINDS: [(&str, Kinds); 3] = [("Files and folders", Kinds::Both), ("Files only", Kinds::Files), ("Folders only", Kinds::Dirs)];
const DAY: f64 = 86400.0;
/// Modified choices: label, newer than (days), older than (days).
pub const DATES: [(&str, Option<u32>, Option<u32>); 7] = [
    ("Any time", None, None),
    ("Today", Some(1), None),
    ("Last 7 days", Some(7), None),
    ("Last 30 days", Some(30), None),
    ("Last year", Some(365), None),
    ("Older than 30 days", None, Some(30)),
    ("Older than a year", None, Some(365)),
];

/// "10M", "1.5 GB", "200k", "42" → bytes; "" → None.
pub fn parse_size(text: &str) -> Result<Option<u64>, String> {
    let t = text.trim().to_lowercase().replace(',', ".");
    if t.is_empty() {
        return Ok(None);
    }
    let bad = || format!("Not a size: '{t}' (examples: 500k, 10M, 1.5G)");
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    // Digits, optionally "." and more digits (no leading/trailing dot).
    let valid_number = match number.split_once('.') {
        None => !number.is_empty(),
        Some((a, b)) => !a.is_empty() && !b.is_empty() && !b.contains('.'),
    };
    if !valid_number {
        return Err(bad());
    }
    let factor: u64 = match unit.trim_start() {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        _ => return Err(bad()),
    };
    let value: f64 = number.parse().map_err(|_| bad())?;
    Ok(Some((value * factor as f64) as u64))
}

/// What the dialog remembers between searches (this session).
#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    pub names: String,
    pub name_regex: bool,
    pub name_case: bool,
    /// Index into [`DEPTHS`].
    pub depth: usize,
    /// Index into [`KINDS`].
    pub kinds: usize,
    pub include_hidden: bool,
    pub search_archives: bool,
    pub text: String,
    pub text_regex: bool,
    pub text_case: bool,
    pub whole_words: bool,
    pub not_containing: bool,
    pub include_online_only: bool,
    pub min_size: String,
    pub max_size: String,
    /// Index into [`DATES`].
    pub date: usize,
}

impl Default for Form {
    fn default() -> Self {
        Form {
            names: "*".into(),
            name_regex: false,
            name_case: false,
            depth: 0,
            kinds: 0,
            include_hidden: true,
            search_archives: false,
            text: String::new(),
            text_regex: false,
            text_case: false,
            whole_words: false,
            not_containing: false,
            include_online_only: false,
            min_size: String::new(),
            max_size: String::new(),
            date: 0,
        }
    }
}

/// The gist of a regex error ("…\nerror: unclosed group" → "unclosed group",
/// "Parsing error at position 3: Invalid escape" → "Invalid escape at position 3"),
/// short enough for the dialog's error line.
fn regex_problem(message: &str) -> String {
    let last = message.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or(message);
    let last = last.strip_prefix("error: ").or_else(|| last.strip_prefix("Error compiling regex: ")).unwrap_or(last);
    if let Some((at, problem)) = last.strip_prefix("Parsing error ").and_then(|rest| rest.split_once(": ")) {
        return format!("{problem} {at}");
    }
    last.to_owned()
}

/// The query for `form` searching `roots` (`now`: seconds since the epoch,
/// for the date presets). Errors are the dialog's messages.
pub fn build_query(form: &Form, roots: Vec<VPath>, now: f64) -> Result<Query, String> {
    let min_size = parse_size(&form.min_size)?;
    let max_size = parse_size(&form.max_size)?;
    let names = match form.names.trim() {
        "" => "*".to_owned(),
        n => n.to_owned(),
    };
    if form.name_regex
        && let Err(e) = NameMatcher::new(&names, true, form.name_case)
    {
        return Err(format!("Search for: {}", regex_problem(&e.to_string())));
    }
    if form.text_regex
        && !form.text.is_empty()
        && let Err(e) = TextMatcher::new(&form.text, true, form.text_case, form.whole_words)
    {
        return Err(format!("Containing: {}", regex_problem(&e.to_string())));
    }
    if roots.is_empty() {
        return Err("Enter a folder to search in".into());
    }
    let (_, newer_days, older_days) = DATES[form.date.min(DATES.len() - 1)];
    Ok(Query {
        roots,
        names,
        name_regex: form.name_regex,
        name_case: form.name_case,
        recursive: true,
        max_depth: DEPTHS[form.depth.min(DEPTHS.len() - 1)].1,
        include_hidden: form.include_hidden,
        kinds: KINDS[form.kinds.min(KINDS.len() - 1)].1,
        text: form.text.clone(),
        text_regex: form.text_regex,
        text_case: form.text_case,
        whole_words: form.whole_words,
        not_containing: form.not_containing,
        min_size,
        max_size,
        newer_than: newer_days.map(|d| now - f64::from(d) * DAY),
        older_than: older_days.map(|d| now - f64::from(d) * DAY),
        search_archives: form.search_archives,
        include_online_only: form.include_online_only,
        archive_opener: None,
    })
}

/// "Search in" as typed: `here` itself if unchanged (also servers and
/// archives), else local folders separated by ";" (`~` expanded, relative
/// ones below `base`).
pub fn parse_roots(text: &str, here: &VPath, base: &std::path::Path) -> Vec<VPath> {
    let text = text.trim();
    if text == here.display() {
        return vec![here.clone()];
    }
    text.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let path = super::super::navigation::expand_home(p);
            let path = if path.is_absolute() { path } else { base.join(path) };
            oc_local::local(path)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The dialog
// ---------------------------------------------------------------------------

/// The dialog's focusable controls, in Tab order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Names,
    NameRegex,
    NameCase,
    Where,
    Depth,
    Kinds,
    Hidden,
    Archives,
    Text,
    TextRegex,
    TextCase,
    WholeWords,
    NotContaining,
    OnlineOnly,
    MinSize,
    MaxSize,
    Date,
    Buttons,
}

const ORDER: [Field; 18] = [
    Field::Names,
    Field::NameRegex,
    Field::NameCase,
    Field::Where,
    Field::Depth,
    Field::Kinds,
    Field::Hidden,
    Field::Archives,
    Field::Text,
    Field::TextRegex,
    Field::TextCase,
    Field::WholeWords,
    Field::NotContaining,
    Field::OnlineOnly,
    Field::MinSize,
    Field::MaxSize,
    Field::Date,
    Field::Buttons,
];

/// Width of the dialog (1.x: 84 characters).
const WIDTH: u16 = 84;
/// Content rows: three sections, the error line and the buttons.
const ROWS: u16 = 18;
/// Labels are indented under the section titles; fields start after them.
const LABEL_X: u16 = 2;
const FIELD_X: u16 = 14;
const CHOICE_W: u16 = 24;
const SIZE_W: u16 = 12;

/// Rows (relative to the content area) of the controls and labels.
mod row {
    pub const NAMES_TITLE: u16 = 0;
    pub const NAMES: u16 = 1;
    pub const NAME_OPTS: u16 = 2;
    pub const WHERE: u16 = 3;
    pub const DEPTH: u16 = 4;
    pub const KINDS: u16 = 5;
    pub const INCLUDE: u16 = 6;
    pub const CONTENTS_TITLE: u16 = 8;
    pub const TEXT: u16 = 9;
    pub const TEXT_OPTS: u16 = 10;
    pub const ONLINE: u16 = 11;
    pub const MORE_TITLE: u16 = 13;
    pub const SIZE: u16 = 14;
    pub const DATE: u16 = 15;
    pub const ERROR: u16 = 16;
    pub const BUTTONS: u16 = 17;
}

/// A choice ("combo box"): its options and the selected one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub options: Vec<&'static str>,
    pub index: usize,
}

impl Choice {
    fn new(options: Vec<&'static str>, index: usize) -> Self {
        let index = index.min(options.len().saturating_sub(1));
        Choice { options, index }
    }

    fn draw(&self, f: &mut Frame, r: Rect, ctx: &DrawCtx, focused: bool) {
        let c = &ctx.theme.colors;
        fill(f, r, c.background);
        let style = Style::new(if focused { c.border_active } else { c.foreground }, c.background);
        put(f, r.x + 1, r.y, self.options[self.index], style, r.w.saturating_sub(3));
        f.put_char(r.right().saturating_sub(2), r.y, '▾', Style::new(if focused { c.border_active } else { c.muted }, c.background));
    }
}

/// Shared with the background check of typed folders (it ends the dialog
/// or reports a missing folder).
#[derive(Default)]
pub(crate) struct Pending {
    pub id: Option<ModalId>,
    pub checking: bool,
    pub error: String,
    /// The dialog is gone (Esc, Cancel, the window closed): a check that
    /// finishes afterwards must not start the search.
    pub closed: bool,
}

pub struct FindFilesDialog {
    /// Where the search starts unless "Search in" is changed.
    pub here: VPath,
    /// The panel the results go to.
    pub side: usize,
    pub names: TextInput,
    pub name_regex: Checkbox,
    pub name_case: Checkbox,
    pub where_: TextInput,
    pub depth: Choice,
    pub kinds: Choice,
    pub include_hidden: Checkbox,
    pub search_archives: Checkbox,
    pub text: TextInput,
    pub text_regex: Checkbox,
    pub text_case: Checkbox,
    pub whole_words: Checkbox,
    pub not_containing: Checkbox,
    pub include_online_only: Checkbox,
    pub min_size: TextInput,
    pub max_size: TextInput,
    pub date: Choice,
    pub buttons: ButtonRow,
    pub focus: Field,
    /// An open choice list: which field and the list.
    popup: Option<(Field, DropdownMenu)>,
    pub(crate) pending: Arc<Mutex<Pending>>,
}

fn input(text: &str, placeholder: &str) -> TextInput {
    let mut t = TextInput::new(text);
    t.placeholder = placeholder.into();
    t
}

impl FindFilesDialog {
    pub fn new(here: VPath, side: usize, form: &Form) -> Self {
        let mut names = input(&form.names, "*.pdf;*.docx   report   *.zip|*old*   (| excludes)");
        names.select_all();
        FindFilesDialog {
            where_: input(&here.display(), ""),
            here,
            side,
            names,
            name_regex: Checkbox::new("Regex", form.name_regex),
            name_case: Checkbox::new("Case sensitive", form.name_case),
            depth: Choice::new(DEPTHS.iter().map(|d| d.0).collect(), form.depth),
            kinds: Choice::new(KINDS.iter().map(|k| k.0).collect(), form.kinds),
            include_hidden: Checkbox::new("Hidden files", form.include_hidden),
            search_archives: Checkbox::new("Inside archives", form.search_archives),
            text: input(&form.text, "optional, e.g. invoice"),
            text_regex: Checkbox::new("Regex", form.text_regex),
            text_case: Checkbox::new("Case sensitive", form.text_case),
            whole_words: Checkbox::new("Whole words", form.whole_words),
            not_containing: Checkbox::new("Not containing", form.not_containing),
            include_online_only: Checkbox::new("Also cloud-only files (downloads them)", form.include_online_only),
            min_size: input(&form.min_size, "e.g. 10M"),
            max_size: input(&form.max_size, "e.g. 1.5G"),
            date: Choice::new(DATES.iter().map(|d| d.0).collect(), form.date),
            buttons: ButtonRow::new(&["Find files", "Cancel"], 0),
            focus: Field::Names,
            popup: None,
            pending: Arc::new(Mutex::new(Pending::default())),
        }
    }

    /// What the fields say now.
    pub fn form(&self) -> Form {
        Form {
            names: self.names.text.clone(),
            name_regex: self.name_regex.checked,
            name_case: self.name_case.checked,
            depth: self.depth.index,
            kinds: self.kinds.index,
            include_hidden: self.include_hidden.checked,
            search_archives: self.search_archives.checked,
            text: self.text.text.clone(),
            text_regex: self.text_regex.checked,
            text_case: self.text_case.checked,
            whole_words: self.whole_words.checked,
            not_containing: self.not_containing.checked,
            include_online_only: self.include_online_only.checked,
            min_size: self.min_size.text.clone(),
            max_size: self.max_size.text.clone(),
            date: self.date.index,
        }
    }

    pub fn error(&self) -> String {
        lock(&self.pending).error.clone()
    }

    fn set_error(&self, e: impl Into<String>) {
        lock(&self.pending).error = e.into();
    }

    fn text_field(&mut self, f: Field) -> Option<&mut TextInput> {
        Some(match f {
            Field::Names => &mut self.names,
            Field::Where => &mut self.where_,
            Field::Text => &mut self.text,
            Field::MinSize => &mut self.min_size,
            Field::MaxSize => &mut self.max_size,
            _ => return None,
        })
    }

    fn checkbox(&mut self, f: Field) -> Option<&mut Checkbox> {
        Some(match f {
            Field::NameRegex => &mut self.name_regex,
            Field::NameCase => &mut self.name_case,
            Field::Hidden => &mut self.include_hidden,
            Field::Archives => &mut self.search_archives,
            Field::TextRegex => &mut self.text_regex,
            Field::TextCase => &mut self.text_case,
            Field::WholeWords => &mut self.whole_words,
            Field::NotContaining => &mut self.not_containing,
            Field::OnlineOnly => &mut self.include_online_only,
            _ => return None,
        })
    }

    fn choice(&mut self, f: Field) -> Option<&mut Choice> {
        Some(match f {
            Field::Depth => &mut self.depth,
            Field::Kinds => &mut self.kinds,
            Field::Date => &mut self.date,
            _ => return None,
        })
    }

    fn move_focus(&mut self, delta: isize) {
        let i = ORDER.iter().position(|&f| f == self.focus).unwrap_or(0) as isize;
        self.focus = ORDER[(i + delta).rem_euclid(ORDER.len() as isize) as usize];
        if let Some(t) = self.text_field(self.focus) {
            t.select_all();
        }
    }

    // -- layout -----------------------------------------------------------------

    fn rects(&self, screen: Rect) -> (Rect, Rect) {
        dialog_rects(screen, WIDTH, ROWS)
    }

    /// Every control's rectangle within `content`.
    fn layout(&self, content: Rect) -> Vec<(Field, Rect)> {
        let at = |row: u16, x: u16, w: u16| {
            let x = content.x + x.min(content.w);
            Rect::new(x, content.y + row, w.min(content.right().saturating_sub(x)), 1)
        };
        let full = content.w.saturating_sub(FIELD_X);
        let mut out = vec![
            (Field::Names, at(row::NAMES, FIELD_X, full)),
            (Field::Where, at(row::WHERE, FIELD_X, full)),
            (Field::Depth, at(row::DEPTH, FIELD_X, CHOICE_W)),
            (Field::Kinds, at(row::KINDS, FIELD_X, CHOICE_W)),
            (Field::Text, at(row::TEXT, FIELD_X, full)),
            (Field::Date, at(row::DATE, FIELD_X, CHOICE_W)),
            (Field::Buttons, at(row::BUTTONS, 0, content.w)),
        ];
        let mut checks = |row: u16, fields: &[(Field, &Checkbox)]| {
            let mut x = FIELD_X;
            for (field, cb) in fields {
                let w = 4 + width(&cb.label);
                out.push((*field, at(row, x, w)));
                x += w + 2;
            }
        };
        checks(row::NAME_OPTS, &[(Field::NameRegex, &self.name_regex), (Field::NameCase, &self.name_case)]);
        checks(row::INCLUDE, &[(Field::Hidden, &self.include_hidden), (Field::Archives, &self.search_archives)]);
        checks(
            row::TEXT_OPTS,
            &[
                (Field::TextRegex, &self.text_regex),
                (Field::TextCase, &self.text_case),
                (Field::WholeWords, &self.whole_words),
                (Field::NotContaining, &self.not_containing),
            ],
        );
        checks(row::ONLINE, &[(Field::OnlineOnly, &self.include_online_only)]);
        let min_x = FIELD_X + width("at least ");
        let max_x = min_x + SIZE_W + 2 + width("at most ");
        out.push((Field::MinSize, at(row::SIZE, min_x, SIZE_W)));
        out.push((Field::MaxSize, at(row::SIZE, max_x, SIZE_W)));
        out
    }

    fn rect_of(&self, content: Rect, field: Field) -> Rect {
        self.layout(content).into_iter().find(|(f, _)| *f == field).map(|(_, r)| r).unwrap_or_default()
    }

    fn open_popup(&mut self, field: Field, content: Rect) {
        let r = self.rect_of(content, field);
        let Some(choice) = self.choice(field) else { return };
        let entries = choice.options.iter().map(|o| MenuEntry::item(o, "")).collect();
        let mut menu = DropdownMenu::new(entries, r.x.saturating_sub(1), r.y + 1);
        menu.cursor = choice.index;
        self.popup = Some((field, menu));
    }

    // -- searching --------------------------------------------------------------

    /// The Search button: check the fields, then start (after checking typed
    /// folders in the background).
    fn search(&mut self, app: &mut App) -> ModalOutcome {
        if lock(&self.pending).checking {
            return ModalOutcome::Consumed;
        }
        let form = self.form();
        let roots = parse_roots(&self.where_.text, &self.here, &base_folder(&self.here));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let query = match build_query(&form, roots, now) {
            Ok(q) => q,
            Err(e) => {
                self.set_error(e);
                return ModalOutcome::Consumed;
            }
        };
        let side = self.side;
        let typed: Vec<VPath> = query.roots.iter().filter(|r| r.is_local() && **r != self.here).cloned().collect();
        if typed.is_empty() {
            super::start_from_dialog(app, query, form, side);
            return ModalOutcome::Close;
        }
        // Typed folders: is each one a folder? (Never stat on the UI thread.)
        {
            let mut p = lock(&self.pending);
            p.checking = true;
            p.error.clear();
        }
        let pending = self.pending.clone();
        app.tasks.run_in_background(
            move |_| typed.into_iter().find(|r| !r.is_dir()).map(|r| r.display()),
            move |app, missing| {
                let id = {
                    let mut p = lock(&pending);
                    p.checking = false;
                    if p.closed {
                        return; // cancelled while the folders were checked
                    }
                    if let Some(m) = &missing {
                        p.error = format!("Not a folder: {m}");
                    }
                    p.id
                };
                if missing.is_some() {
                    app.redraw();
                    return;
                }
                if let Some(id) = id {
                    app.close_modal(id);
                }
                super::start_from_dialog(app, query, form, side);
            },
        );
        ModalOutcome::Consumed
    }

    fn key(&mut self, app: &mut App, k: &KeyEvent, screen: Rect) -> ModalOutcome {
        let content = self.rects(screen).1;
        if let Some((field, menu)) = &mut self.popup {
            match menu.handle_key(k) {
                MenuOutcome::Activated(i) => {
                    let field = *field;
                    self.popup = None;
                    if let Some(c) = self.choice(field) {
                        c.index = i;
                    }
                }
                MenuOutcome::Close => self.popup = None,
                _ => {}
            }
            return ModalOutcome::Consumed;
        }
        let m = k.mods;
        let plain = m.is_empty();
        match k.key {
            Key::Escape => return ModalOutcome::Close,
            Key::Tab if plain => {
                self.move_focus(1);
                return ModalOutcome::Consumed;
            }
            Key::Tab if m == crate::input::Mods::SHIFT => {
                self.move_focus(-1);
                return ModalOutcome::Consumed;
            }
            // Up/Down change a choice (1.x: combo boxes), else move the focus.
            Key::Up if plain && self.choice(self.focus).is_none() => {
                self.move_focus(-1);
                return ModalOutcome::Consumed;
            }
            Key::Down if plain && self.choice(self.focus).is_none() => {
                self.move_focus(1);
                return ModalOutcome::Consumed;
            }
            Key::Down if m.alt && self.choice(self.focus).is_some() => {
                self.open_popup(self.focus, content);
                return ModalOutcome::Consumed;
            }
            Key::Enter if plain && self.focus != Field::Buttons => return self.search(app),
            _ => {}
        }
        let focus = self.focus;
        if focus == Field::Buttons {
            return match self.buttons.handle_key(k, true) {
                RowOutcome::Activated(0) => self.search(app),
                RowOutcome::Activated(_) => ModalOutcome::Close,
                _ => ModalOutcome::Consumed,
            };
        }
        if let Some(cb) = self.checkbox(focus) {
            if !cb.handle_key(k, true) && plain {
                match k.key {
                    Key::Left => self.move_focus(-1),
                    Key::Right => self.move_focus(1),
                    _ => {}
                }
            }
            return ModalOutcome::Consumed;
        }
        if let Some(c) = self.choice(focus) {
            // Like a Qt combo box: no wrapping around, Left/Right do nothing.
            let last = c.options.len().saturating_sub(1);
            match k.key {
                Key::Up | Key::PageUp if plain => c.index = c.index.saturating_sub(1),
                Key::Down | Key::PageDown if plain => c.index = (c.index + 1).min(last),
                Key::Home if plain => c.index = 0,
                Key::End if plain => c.index = last,
                Key::Space | Key::F(4) if plain => self.open_popup(focus, content),
                _ => {}
            }
            return ModalOutcome::Consumed;
        }
        let clip = app.clipboard.as_mut();
        if let Some(t) = self.text_field(focus) {
            t.handle_key(k, clip);
        }
        ModalOutcome::Consumed
    }

    fn mouse(&mut self, app: &mut App, m: &crate::input::MouseEvent, screen: Rect) -> ModalOutcome {
        let content = self.rects(screen).1;
        if let Some((field, menu)) = &mut self.popup {
            match menu.handle_mouse(m, screen) {
                MenuOutcome::Activated(i) => {
                    let field = *field;
                    self.popup = None;
                    if let Some(c) = self.choice(field) {
                        c.index = i;
                    }
                }
                MenuOutcome::Close => self.popup = None,
                _ => {}
            }
            return ModalOutcome::Consumed;
        }
        let hit = self.layout(content).into_iter().find(|(_, r)| r.contains(m.col, m.row));
        match (m.kind, hit) {
            (MouseKind::Down(MouseButton::Left), Some((Field::Buttons, _))) => self.focus = Field::Buttons,
            (MouseKind::Up(MouseButton::Left), Some((Field::Buttons, r))) => {
                return match self.buttons.hit(r, m.col, m.row) {
                    Some(0) => self.search(app),
                    Some(_) => ModalOutcome::Close,
                    None => ModalOutcome::Consumed,
                };
            }
            (MouseKind::Down(MouseButton::Left), Some((field, r))) => {
                self.focus = field;
                if let Some(cb) = self.checkbox(field) {
                    cb.checked = !cb.checked;
                } else if self.choice(field).is_some() {
                    self.open_popup(field, content);
                } else if let Some(t) = self.text_field(field) {
                    t.click(r, m.col);
                }
            }
            _ => {}
        }
        ModalOutcome::Consumed
    }
}

impl Drop for FindFilesDialog {
    fn drop(&mut self) {
        lock(&self.pending).closed = true;
    }
}

pub(crate) fn lock(p: &Mutex<Pending>) -> std::sync::MutexGuard<'_, Pending> {
    p.lock().unwrap_or_else(|e| e.into_inner())
}

impl Modal for FindFilesDialog {
    fn title(&self) -> &str {
        TITLE
    }

    fn text(&self) -> String {
        let f = self.form();
        format!(
            "Search for: {}\nSearch in: {}\nContaining: {}\nSize: {} - {}\n{}",
            f.names,
            self.where_.text,
            f.text,
            f.min_size,
            f.max_size,
            self.error()
        )
    }

    fn wants_text_input(&self) -> bool {
        self.popup.is_none() && matches!(self.focus, Field::Names | Field::Where | Field::Text | Field::MinSize | Field::MaxSize)
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let (outer, _) = self.rects(screen);
        let content = draw_box(f, outer, TITLE, ctx);
        let label = Style::new(c.foreground, c.surface);
        let title = Style::new(c.accent, c.surface).with(Flags::BOLD);
        let at = |f: &mut Frame, row: u16, x: u16, text: &str, style: Style| {
            if row < content.h {
                put(f, content.x + x, content.y + row, text, style, content.w.saturating_sub(x));
            }
        };
        at(f, row::NAMES_TITLE, 0, "Names", title);
        at(f, row::NAMES, LABEL_X, "Search for", label);
        at(f, row::WHERE, LABEL_X, "Search in", label);
        at(f, row::DEPTH, LABEL_X, "Depth", label);
        at(f, row::KINDS, LABEL_X, "Find", label);
        at(f, row::INCLUDE, LABEL_X, "Include", label);
        at(f, row::CONTENTS_TITLE, 0, "Contents", title);
        at(f, row::TEXT, LABEL_X, "Containing", label);
        at(f, row::MORE_TITLE, 0, "Size and date", title);
        at(f, row::SIZE, LABEL_X, "Size", label);
        at(f, row::SIZE, FIELD_X, "at least", label);
        at(f, row::SIZE, FIELD_X + width("at least ") + SIZE_W + 2, "at most", label);
        at(f, row::DATE, LABEL_X, "Modified", label);
        let (error, checking) = {
            let p = lock(&self.pending);
            (p.error.clone(), p.checking)
        };
        let status = if checking { "Checking the folders…".to_owned() } else { error };
        at(f, row::ERROR, LABEL_X, &status, Style::new(c.muted, c.surface));
        for (field, r) in self.layout(content) {
            if r.y >= content.bottom() {
                continue;
            }
            let focused = self.focus == field && self.popup.is_none();
            match field {
                Field::Names => self.names.draw(f, r, ctx, focused),
                Field::Where => self.where_.draw(f, r, ctx, focused),
                Field::Text => self.text.draw(f, r, ctx, focused),
                Field::MinSize => self.min_size.draw(f, r, ctx, focused),
                Field::MaxSize => self.max_size.draw(f, r, ctx, focused),
                Field::Depth => self.depth.draw(f, r, ctx, focused),
                Field::Kinds => self.kinds.draw(f, r, ctx, focused),
                Field::Date => self.date.draw(f, r, ctx, focused),
                Field::Buttons => self.buttons.draw(f, r, ctx, focused),
                Field::NameRegex => self.name_regex.draw(f, r, ctx, focused),
                Field::NameCase => self.name_case.draw(f, r, ctx, focused),
                Field::Hidden => self.include_hidden.draw(f, r, ctx, focused),
                Field::Archives => self.search_archives.draw(f, r, ctx, focused),
                Field::TextRegex => self.text_regex.draw(f, r, ctx, focused),
                Field::TextCase => self.text_case.draw(f, r, ctx, focused),
                Field::WholeWords => self.whole_words.draw(f, r, ctx, focused),
                Field::NotContaining => self.not_containing.draw(f, r, ctx, focused),
                Field::OnlineOnly => self.include_online_only.draw(f, r, ctx, focused),
            }
        }
        if let Some((_, menu)) = &self.popup {
            menu.draw(f, screen, ctx);
        }
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        match ev {
            Event::Key(k) => self.key(app, k, screen),
            Event::Commit(_) | Event::Preedit(_) => {
                let clip = app.clipboard.as_mut();
                if self.popup.is_none()
                    && let Some(t) = self.text_field(self.focus)
                {
                    t.handle_event(ev, clip);
                }
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => self.mouse(app, m, screen),
            Event::Focus(_) => ModalOutcome::Consumed,
        }
    }
}

/// A local folder for relative "Search in" entries.
pub fn base_folder(here: &VPath) -> PathBuf {
    crate::panel::local_anchor(here).unwrap_or_else(oc_local::home_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size(""), Ok(None));
        assert_eq!(parse_size("42"), Ok(Some(42)));
        assert_eq!(parse_size("500k"), Ok(Some(500 * 1024)));
        assert_eq!(parse_size("10M"), Ok(Some(10 * 1024 * 1024)));
        assert_eq!(parse_size("1.5G"), Ok(Some((1.5 * 1024f64.powi(3)) as u64)));
        assert_eq!(parse_size("1,5 GB"), Ok(Some((1.5 * 1024f64.powi(3)) as u64)));
        assert_eq!(parse_size(" 200 kb "), Ok(Some(200 * 1024)));
        assert_eq!(parse_size("7b"), Ok(Some(7)));
        let err = parse_size("lots").unwrap_err();
        assert!(err.starts_with("Not a size: 'lots'"), "{err}");
        assert!(parse_size("1.").is_err() && parse_size(".5k").is_err() && parse_size("10T").is_err() && parse_size("1.2.3").is_err());
    }

    #[test]
    fn query_from_form() {
        let root = oc_local::local(oc_local::home_dir());
        let form = Form {
            names: "*.txt|*one*".into(),
            text: "needle".into(),
            not_containing: true,
            min_size: "1k".into(),
            depth: 1,
            kinds: 1,
            date: 5,
            ..Form::default()
        };
        let q = build_query(&form, vec![root.clone()], 1_000_000.0).unwrap();
        assert_eq!((q.names.as_str(), q.text.as_str(), q.not_containing), ("*.txt|*one*", "needle", true));
        assert_eq!((q.min_size, q.max_size, q.max_depth, q.kinds), (Some(1024), None, Some(0), Kinds::Files));
        assert_eq!((q.newer_than, q.older_than), (None, Some(1_000_000.0 - 30.0 * DAY)));
        assert!(q.include_hidden && q.recursive && q.roots == vec![root.clone()]);
        let today = build_query(&Form { date: 1, names: "  ".into(), ..Form::default() }, vec![root.clone()], 100_000.0).unwrap();
        assert_eq!((today.newer_than, today.names.as_str()), (Some(100_000.0 - DAY), "*"));

        let bad = |form: Form| build_query(&form, vec![root.clone()], 0.0).unwrap_err();
        assert!(bad(Form { max_size: "lots".into(), ..Form::default() }).contains("Not a size"));
        let unclosed = bad(Form { names: "(?<=x".into(), name_regex: true, ..Form::default() });
        assert_eq!(unclosed, "Search for: Opening parenthesis without closing parenthesis at position 5");
        assert!(bad(Form { text: "[".into(), text_regex: true, ..Form::default() }).starts_with("Containing:"));
        // Python `re` features (1.x): look-around and back-references.
        let python = Form { names: "^(?!report).*pdf".into(), name_regex: true, text: r"(\w)\1".into(), text_regex: true, ..Form::default() };
        assert!(build_query(&python, vec![root.clone()], 0.0).is_ok());
        assert_eq!(build_query(&Form::default(), vec![], 0.0).unwrap_err(), "Enter a folder to search in");
    }

    #[test]
    fn roots_as_typed() {
        let base = oc_local::home_dir();
        let here = oc_local::local(&base);
        assert_eq!(parse_roots(&here.display(), &here, &base), vec![here.clone()]);
        let sep = std::path::MAIN_SEPARATOR;
        let typed = parse_roots(&format!("a ; ~{sep}b;;"), &here, &base);
        assert_eq!(typed, vec![oc_local::local(base.join("a")), oc_local::local(base.join("b"))]);
        assert!(parse_roots("  ", &here, &base).is_empty());
    }
}
