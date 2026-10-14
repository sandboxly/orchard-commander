//! The Multi-Rename dialog (port of `ui/rename_ui.py`): name and extension
//! masks with Insert… menus, search & replace, case, counter, and a live
//! preview of every new name with its problems.
//!
//! The dialog's state ([`RenameDialog`]) lives in the feature's app state
//! (`RenameState::dialog`), not in the modal itself: metadata results and
//! the preview's debounce timer arrive as closures over `&mut App` and
//! update it there. [`RenameModal`] is the thin [`Modal`] on the stack that
//! draws it and feeds it events.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oc_grid::{Flags, Frame, Style};
use oc_rename::metadata::{self, Metadata};
use oc_rename::{CASES, Case, ExistingNames, Item, Plan, Renamer, Rule, needs_metadata, plan_with};

use super::{RenameState, finish_dialog};
use crate::app::App;
use crate::dialogs::{dialog_rects, draw_box};
use crate::geom::Rect;
use crate::input::{Event, Key, KeyEvent, Mods, MouseButton, MouseKind};
use crate::modal::{Modal, ModalOutcome};
use crate::tasks::CancelToken;
use crate::widgets::button::RowOutcome;
use crate::widgets::{ButtonRow, Checkbox, DrawCtx, DropdownMenu, ListState, MenuEntry, MenuOutcome, TextInput, fill, put, width};

/// The labels of [`CASES`], in the same order.
pub const CASE_LABELS: [&str; 5] = ["Unchanged", "lowercase", "UPPERCASE", "First letter uppercase", "Every Word Uppercase"];

/// (placeholder, description) for the Insert… menu; None separates groups.
pub const PLACEHOLDERS: &[Option<(&str, &str)>] = &[
    Some(("[N]", "Name (without extension)")),
    Some(("[N1-3]", "Name, characters 1 to 3")),
    Some(("[N2,5]", "Name, 5 characters from the 2nd")),
    Some(("[N3-]", "Name, from the 3rd character to the end")),
    Some(("[N-5-]", "Name, the last 5 characters")),
    Some(("[N2--5]", "Name, from the 2nd to the 5th-last character")),
    Some(("[E]", "Extension")),
    Some(("[1-5]", "Whole name with extension, characters 1 to 5")),
    None,
    Some(("[P]", "Parent folder's name")),
    Some(("[G]", "Grandparent folder's name")),
    None,
    Some(("[C]", "Counter (start, step and digits below)")),
    Some(("[C10+5:3]", "Counter starting at 10, step 5, 3 digits")),
    None,
    Some(("[Y]-[M]-[D]", "Modification date: year, month, day")),
    Some(("[h].[m].[s]", "Modification time: hours, minutes, seconds")),
    Some(("[d]", "Modification date (2026-03-07)")),
    Some(("[t]", "Modification time (09.05.04)")),
    None,
    Some(("[U]", "UPPERCASE from here on")),
    Some(("[L]", "lowercase from here on")),
    Some(("[F]", "First Letter Of Each Word uppercase from here on")),
    Some(("[n]", "Original case from here on")),
    Some(("[[]", "A literal [")),
    Some(("[]]", "A literal ]")),
];

/// The last entry of the Insert… menu.
const FALLBACK_HINT: (&str, &str) = ("[=artist|Unknown]", "A field with a fallback when the file has none");

/// Up to this many items the preview is recomputed on every keystroke;
/// above, after a short pause in typing (like 1.x's 120 ms timer).
const SYNC_PREVIEW_LIMIT: usize = 2000;
const PREVIEW_DELAY: Duration = Duration::from_millis(120);
/// Metadata results are handed to the UI at most this often.
const METADATA_INTERVAL: Duration = Duration::from_millis(100);

const WIDTH: u16 = 110;
const LABEL_W: u16 = 11;
const INSERT_W: u16 = 11;
const INSERT_LABEL: &str = "Insert… ▾";
const COUNTER_LABEL: &str = "  Counter [C]: start ";

/// The dialog's focusable controls, in Tab order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Ext,
    Search,
    Replace,
    Regex,
    CaseSensitive,
    Case,
    Start,
    Step,
    Digits,
    Buttons,
}

const FOCUS_ORDER: [Field; 11] = [
    Field::Name,
    Field::Ext,
    Field::Search,
    Field::Replace,
    Field::Regex,
    Field::CaseSensitive,
    Field::Case,
    Field::Start,
    Field::Step,
    Field::Digits,
    Field::Buttons,
];

/// A number field (Qt's QSpinBox): typing edits it, ↑/↓ step it.
#[derive(Clone, Debug)]
pub struct Spin {
    pub input: TextInput,
    pub value: i64,
    low: i64,
    high: i64,
}

impl Spin {
    fn new(value: i64, low: i64, high: i64) -> Self {
        let value = value.clamp(low, high);
        Spin { input: TextInput::new(&value.to_string()), value, low, high }
    }

    pub fn set(&mut self, value: i64) {
        self.value = value.clamp(self.low, self.high);
        self.input.set_text(&self.value.to_string());
    }

    /// Returns whether the key was used.
    fn handle_key(&mut self, k: &KeyEvent, clip: &mut dyn crate::clipboard::Clipboard) -> bool {
        let plain = k.mods.is_empty();
        match k.key {
            Key::Up if plain => self.set(self.value.saturating_add(1)),
            Key::Down if plain => self.set(self.value.saturating_sub(1)),
            _ => {
                if let Some(t) = k.typed_text()
                    && !t.chars().all(|c| c.is_ascii_digit() || c == '-')
                {
                    return true; // not a number: swallowed
                }
                let used = self.input.handle_key(k, clip);
                if let Ok(v) = self.input.text.trim().parse::<i64>() {
                    self.value = v.clamp(self.low, self.high);
                }
                return used;
            }
        }
        true
    }

    /// Leaving the field shows the value it stands for.
    fn blur(&mut self) {
        if self.input.text != self.value.to_string() {
            self.input.set_text(&self.value.to_string());
        }
    }
}

/// What choosing a menu entry does.
#[derive(Clone, Debug, PartialEq)]
enum MenuAction {
    Insert(String),
    /// Open the fields of metadata provider `i`.
    Provider(usize),
    SetCase(usize),
    Nothing,
}

/// An open dropdown (Insert… or Case), possibly a provider submenu over its parent.
struct OpenMenu {
    menu: DropdownMenu,
    actions: Vec<MenuAction>,
    /// The field an Insert menu inserts into.
    target: Field,
    parent: Option<Box<OpenMenu>>,
}

/// The metadata reader of one dialog.
struct MetaReader {
    token: CancelToken,
    /// (item index, fields, why unavailable), filled by the worker.
    results: Arc<Mutex<Vec<(usize, Metadata, String)>>>,
}

/// Where everything is, for drawing and hit testing.
struct Lay {
    outer: Rect,
    content: Rect,
    name: Rect,
    name_insert: Rect,
    ext: Rect,
    ext_insert: Rect,
    search: Rect,
    replace_label: u16,
    replace: Rect,
    regex: Rect,
    case_sensitive: Rect,
    case: Rect,
    counter_label: u16,
    start: Rect,
    step_label: u16,
    step: Rect,
    digits_label: u16,
    digits: Rect,
    header: Rect,
    table: Rect,
    summary: Rect,
    buttons: Rect,
}

/// How a key or click ended for the dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogOutcome {
    Consumed,
    Accept,
    Cancel,
}

pub struct RenameDialog {
    /// Tells this dialog's metadata results and timers from an earlier one's.
    pub id: u64,
    /// The panel the items come from.
    pub side: usize,
    pub title: String,
    /// Filled in with metadata as it arrives.
    pub items: Vec<Item>,
    exists: Arc<ExistingNames>,
    pub plan: Plan,
    pub rule: Rule,
    pub name_mask: TextInput,
    pub ext_mask: TextInput,
    pub search: TextInput,
    pub replace: TextInput,
    pub regex: Checkbox,
    pub case_sensitive: Checkbox,
    /// Index into [`CASES`].
    pub case: usize,
    pub counter_start: Spin,
    pub counter_step: Spin,
    pub counter_digits: Spin,
    pub focus: Field,
    pub buttons: ButtonRow,
    table: ListState,
    menu: Option<OpenMenu>,
    /// Width of the status column (its longest text).
    status_w: u16,
    /// The line below the table, computed with the plan (drawing stays
    /// proportional to the visible rows).
    summary: String,
    meta: Option<MetaReader>,
    /// The preview is older than the fields (large batches: a timer is due).
    dirty: bool,
    timer: Option<u64>,
}

impl RenameDialog {
    pub fn new(id: u64, side: usize, items: Vec<Item>, rule: &Rule, exists: Arc<ExistingNames>) -> Self {
        let n = items.len();
        let mut search = TextInput::new(&rule.search);
        search.placeholder = "e.g. draft or draft|old".into();
        let mut replace = TextInput::new(&rule.replace);
        replace.placeholder = "e.g. final, final|new or $1".into();
        let mut name_mask = TextInput::selected_all(&rule.name_mask);
        name_mask.placeholder = "e.g. [N]_[C]".into();
        let mut ext_mask = TextInput::new(&rule.ext_mask);
        ext_mask.placeholder = "e.g. [E]".into();
        RenameDialog {
            id,
            side,
            title: format!("Multi-rename {n} item{}", if n == 1 { "" } else { "s" }),
            table: ListState::new(n),
            items,
            exists,
            plan: Plan::default(),
            rule: rule.clone(),
            name_mask,
            ext_mask,
            search,
            replace,
            regex: Checkbox::new("Regex", rule.regex),
            case_sensitive: Checkbox::new("Case sensitive", rule.case_sensitive),
            case: CASES.iter().position(|c| *c == rule.case.as_str()).unwrap_or(0),
            counter_start: Spin::new(rule.counter_start, -999_999, 999_999),
            counter_step: Spin::new(rule.counter_step, -1000, 1000),
            counter_digits: Spin::new(rule.counter_digits, 1, 12),
            focus: Field::Name,
            buttons: ButtonRow::new(&["Rename", "Cancel"], 0),
            menu: None,
            status_w: 0,
            summary: String::new(),
            meta: None,
            dirty: false,
            timer: None,
        }
    }

    /// The rule the fields describe now.
    pub fn current_rule(&self) -> Rule {
        Rule {
            name_mask: self.name_mask.text.clone(),
            ext_mask: self.ext_mask.text.clone(),
            search: self.search.text.clone(),
            replace: self.replace.text.clone(),
            regex: self.regex.checked,
            case_sensitive: self.case_sensitive.checked,
            case: Case::parse(CASES[self.case]).unwrap_or_default(),
            counter_start: self.counter_start.value,
            counter_step: self.counter_step.value,
            counter_digits: self.counter_digits.value,
        }
    }

    /// Whether Rename may run (no problems, no metadata pending, something to do).
    pub fn can_rename(&self) -> bool {
        !self.dirty && self.plan.can_run()
    }

    /// The line below the table.
    pub fn summary(&self) -> String {
        self.summary.clone()
    }

    fn compute_summary(&self) -> String {
        let plan = &self.plan;
        if !plan.mask_error.is_empty() {
            return plan.mask_error.clone();
        }
        let total = self.items.len();
        let mut text = format!("{} of {total} will be renamed", plan.to_rename().count());
        let pending = plan.pending();
        if pending > 0 {
            text += &format!(" · reading metadata… {} of {total}", total - pending);
        }
        let skipped = plan.skipped().count();
        if skipped > 0 {
            text += &format!(" · {skipped} skipped (no value; add a fallback like [=artist|Unknown])");
        }
        let problems = plan.problems().count();
        if problems > 0 {
            text += &format!(" · {problems} problem{} to fix before renaming", if problems == 1 { "" } else { "s" });
        }
        text
    }

    /// Row `i` of the preview: (new name, status) as shown.
    pub fn row(&self, i: usize) -> (String, String) {
        let Some(change) = self.plan.changes.get(i) else {
            return (String::new(), String::new());
        };
        if !self.plan.mask_error.is_empty() {
            return (String::new(), String::new());
        }
        let status = if !change.error.is_empty() {
            change.error.clone()
        } else if !change.note.is_empty() {
            change.note.clone()
        } else if change.changed() {
            String::new()
        } else {
            "unchanged".into()
        };
        (change.new.clone(), status)
    }

    // -- preview ------------------------------------------------------------------

    /// Recompute the preview from the fields (starts reading metadata when
    /// the masks first ask for it).
    pub fn update(&mut self, app: &mut App) {
        if let Some(t) = self.timer.take() {
            app.cancel_timer(t);
        }
        self.dirty = false;
        self.rule = self.current_rule();
        if needs_metadata(&self.rule) {
            self.start_metadata(app);
        }
        let exists = self.exists.clone();
        self.plan = plan_with(&Renamer::new(&self.rule), &self.items, &|l| exists.exists(l));
        self.status_w = (0..self.items.len()).map(|i| width(&self.row(i).1)).max().unwrap_or(0);
        self.summary = self.compute_summary();
        self.buttons.buttons[0].enabled = self.plan.can_run();
        app.redraw();
    }

    /// A field changed: update now, or after a pause for big batches.
    fn changed(&mut self, app: &mut App) {
        if self.items.len() <= SYNC_PREVIEW_LIMIT {
            self.update(app);
            return;
        }
        self.dirty = true;
        self.buttons.buttons[0].enabled = false;
        if let Some(t) = self.timer.take() {
            app.cancel_timer(t);
        }
        let id = self.id;
        self.timer = Some(app.schedule(PREVIEW_DELAY, move |app| {
            with_dialog(app, id, |d, app| {
                d.timer = None;
                d.update(app);
            });
        }));
    }

    /// Read every file's metadata once, in the background; the preview fills
    /// in as results arrive.
    fn start_metadata(&mut self, app: &mut App) {
        if self.meta.is_some() {
            return;
        }
        let token = CancelToken::new();
        let results: Arc<Mutex<Vec<(usize, Metadata, String)>>> = Arc::default();
        let work: Vec<_> = self.items.iter().map(|i| (i.location.clone(), i.is_dir, i.online_only)).collect();
        let (tok, out, tasks, id) = (token.clone(), results.clone(), app.tasks.clone(), self.id);
        app.tasks.spawn(
            move || {
                let mut last = Instant::now();
                for (i, (location, is_dir, online_only)) in work.into_iter().enumerate() {
                    if tok.is_cancelled() {
                        return;
                    }
                    let (meta, error) = match metadata::read(&location, is_dir, online_only) {
                        Ok(m) => (m, String::new()),
                        Err(e) => (Metadata::new(), e.0),
                    };
                    out.lock().unwrap_or_else(|e| e.into_inner()).push((i, meta, error));
                    if last.elapsed() >= METADATA_INTERVAL {
                        last = Instant::now();
                        tasks.post(move |app| metadata_arrived(app, id));
                    }
                }
            },
            move |app, ()| metadata_arrived(app, id),
        );
        self.meta = Some(MetaReader { token, results });
    }

    fn take_metadata(&mut self) -> bool {
        let Some(m) = &self.meta else { return false };
        let arrived = std::mem::take(&mut *m.results.lock().unwrap_or_else(|e| e.into_inner()));
        let any = !arrived.is_empty();
        for (i, meta, error) in arrived {
            if let Some(item) = self.items.get_mut(i) {
                item.meta = Some(Arc::new(meta));
                item.meta_error = error;
            }
        }
        any
    }

    /// The dialog closes: stop the metadata reader and the timer.
    pub(super) fn close(&mut self, app: &mut App) {
        if let Some(m) = &self.meta {
            m.token.cancel();
        }
        if let Some(t) = self.timer.take() {
            app.cancel_timer(t);
        }
    }

    /// Try to accept (Enter / Rename): only with a runnable plan.
    fn accept(&mut self, app: &mut App) -> DialogOutcome {
        if self.dirty {
            self.update(app);
        }
        if self.plan.can_run() { DialogOutcome::Accept } else { DialogOutcome::Consumed }
    }

    // -- focus ----------------------------------------------------------------------

    fn input_mut(&mut self, field: Field) -> Option<&mut TextInput> {
        match field {
            Field::Name => Some(&mut self.name_mask),
            Field::Ext => Some(&mut self.ext_mask),
            Field::Search => Some(&mut self.search),
            Field::Replace => Some(&mut self.replace),
            Field::Start => Some(&mut self.counter_start.input),
            Field::Step => Some(&mut self.counter_step.input),
            Field::Digits => Some(&mut self.counter_digits.input),
            _ => None,
        }
    }

    fn spin_mut(&mut self, field: Field) -> Option<&mut Spin> {
        match field {
            Field::Start => Some(&mut self.counter_start),
            Field::Step => Some(&mut self.counter_step),
            Field::Digits => Some(&mut self.counter_digits),
            _ => None,
        }
    }

    pub fn set_focus(&mut self, field: Field) {
        if field != self.focus
            && let Some(spin) = self.spin_mut(self.focus)
        {
            spin.blur();
        }
        self.focus = field;
    }

    fn move_focus(&mut self, delta: isize) {
        let i = FOCUS_ORDER.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        let n = FOCUS_ORDER.len() as isize;
        let field = FOCUS_ORDER[(i + delta).rem_euclid(n) as usize];
        self.set_focus(field);
        // Like Qt: a field reached from the keyboard has its text selected.
        if let Some(input) = self.input_mut(field) {
            input.select_all();
        }
    }

    /// Insert a placeholder at the cursor of a mask field (replacing its selection).
    pub fn insert(&mut self, app: &mut App, field: Field, code: &str) {
        self.set_focus(field);
        if let Some(input) = self.input_mut(field) {
            input.insert(code);
        }
        self.changed(app);
    }

    // -- menus ----------------------------------------------------------------------

    fn open_insert_menu(&mut self, field: Field, screen: Rect) {
        let l = self.layout(screen);
        let button = if field == Field::Ext { l.ext_insert } else { l.name_insert };
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        for p in PLACEHOLDERS {
            match p {
                Some((code, description)) => {
                    entries.push(MenuEntry::item(&escape(code), description));
                    actions.push(MenuAction::Insert((*code).into()));
                }
                None => {
                    entries.push(MenuEntry::Separator);
                    actions.push(MenuAction::Nothing);
                }
            }
        }
        entries.push(MenuEntry::Separator);
        actions.push(MenuAction::Nothing);
        for (i, provider) in metadata::registry().providers().iter().enumerate() {
            entries.push(MenuEntry::item(&escape(provider.label()), "▸"));
            actions.push(MenuAction::Provider(i));
        }
        entries.push(MenuEntry::item(FALLBACK_HINT.0, FALLBACK_HINT.1));
        actions.push(MenuAction::Insert(FALLBACK_HINT.0.into()));
        let menu = DropdownMenu::new(entries, button.x, button.bottom());
        self.set_focus(field);
        self.menu = Some(OpenMenu { menu, actions, target: field, parent: None });
    }

    fn open_provider_menu(&mut self, provider: usize, screen: Rect) {
        let Some(parent) = self.menu.take() else { return };
        let registry = metadata::registry();
        let Some(p) = registry.providers().get(provider) else {
            self.menu = Some(parent);
            return;
        };
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        for field in p.fields() {
            let code = format!("[={}]", field.name);
            entries.push(MenuEntry::item(&escape(&code), &field.description));
            actions.push(MenuAction::Insert(code));
        }
        let r = parent.menu.rect(screen);
        let y = r.y + 1 + parent.menu.cursor as u16;
        let mut menu = DropdownMenu::new(entries, r.right(), y);
        // Not enough room on the right: open to the left of the parent.
        let w = menu.rect(screen).w;
        if r.right() + w > screen.right() {
            menu.x = r.x.saturating_sub(w);
        }
        let target = parent.target;
        self.menu = Some(OpenMenu { menu, actions, target, parent: Some(Box::new(parent)) });
    }

    fn open_case_menu(&mut self, screen: Rect) {
        let l = self.layout(screen);
        let entries = CASE_LABELS
            .iter()
            .enumerate()
            .map(|(i, label)| MenuEntry::Item { label: escape(label), shortcut: String::new(), checked: Some(i == self.case), enabled: true })
            .collect();
        let mut menu = DropdownMenu::new(entries, l.case.x.saturating_sub(1), l.case.bottom());
        menu.cursor = self.case;
        self.set_focus(Field::Case);
        self.menu = Some(OpenMenu { menu, actions: (0..CASES.len()).map(MenuAction::SetCase).collect(), target: Field::Case, parent: None });
    }

    fn close_menu_level(&mut self) {
        self.menu = self.menu.take().and_then(|m| m.parent.map(|p| *p));
    }

    fn menu_chose(&mut self, app: &mut App, i: usize, screen: Rect) {
        let Some(m) = &self.menu else { return };
        let target = m.target;
        match m.actions.get(i).cloned().unwrap_or(MenuAction::Nothing) {
            MenuAction::Insert(code) => {
                self.menu = None;
                self.insert(app, target, &code);
            }
            MenuAction::Provider(p) => self.open_provider_menu(p, screen),
            MenuAction::SetCase(c) => {
                self.menu = None;
                self.set_case(app, c);
            }
            MenuAction::Nothing => {}
        }
    }

    fn set_case(&mut self, app: &mut App, case: usize) {
        if case != self.case {
            self.case = case;
            self.changed(app);
        }
    }

    fn menu_key(&mut self, app: &mut App, k: &KeyEvent, screen: Rect) {
        let Some(m) = &mut self.menu else { return };
        let submenu = m.parent.is_some();
        match m.menu.handle_key(k) {
            MenuOutcome::Activated(i) => self.menu_chose(app, i, screen),
            MenuOutcome::Close => self.close_menu_level(),
            MenuOutcome::Prev if submenu => self.close_menu_level(),
            MenuOutcome::Next => {
                let i = m.menu.cursor;
                if matches!(m.actions.get(i), Some(MenuAction::Provider(_))) {
                    self.menu_chose(app, i, screen);
                }
            }
            _ => {}
        }
    }

    fn menu_mouse(&mut self, app: &mut App, ev: &crate::input::MouseEvent, screen: Rect) {
        loop {
            let Some(m) = &mut self.menu else { return };
            match m.menu.handle_mouse(ev, screen) {
                MenuOutcome::Activated(i) => return self.menu_chose(app, i, screen),
                MenuOutcome::Close => {
                    // A click outside a submenu may be on its parent.
                    let on_parent = m.parent.as_ref().is_some_and(|p| p.menu.rect(screen).contains(ev.col, ev.row));
                    self.close_menu_level();
                    if !on_parent {
                        self.menu = None;
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    // -- layout and drawing ---------------------------------------------------------

    fn table_rows(&self, screen: Rect) -> u16 {
        let want = self.items.len().clamp(4, 16) as u16;
        want.min(screen.h.saturating_sub(15)).max(1)
    }

    fn layout(&self, screen: Rect) -> Lay {
        let t = self.table_rows(screen);
        let (outer, c) = dialog_rects(screen, WIDTH, t + 11);
        let clip = |r: Rect| -> Rect {
            let x = r.x.min(c.right());
            let w = r.w.min(c.right().saturating_sub(x));
            Rect::new(x, r.y, w, if r.y < c.bottom() { r.h.min(c.bottom() - r.y) } else { 0 })
        };
        let fx = c.x + LABEL_W;
        let fw = c.w.saturating_sub(LABEL_W);
        let mask_w = fw.saturating_sub(INSERT_W + 1);
        let insert_x = c.right().saturating_sub(INSERT_W);
        let replace_label_w = width(" Replace ");
        let search_w = fw.saturating_sub(replace_label_w) / 2;
        let replace_x = fx + search_w + replace_label_w;
        let case_w = 24;
        let counter_label = fx + case_w;
        let start = Rect::new(counter_label + width(COUNTER_LABEL), c.y + 4, 9, 1);
        let step_label = start.right();
        let step = Rect::new(step_label + width(" step "), c.y + 4, 9, 1);
        let digits_label = step.right();
        let digits = Rect::new(digits_label + width(" digits "), c.y + 4, 5, 1);
        Lay {
            outer,
            content: c,
            name: clip(Rect::new(fx, c.y, mask_w, 1)),
            name_insert: clip(Rect::new(insert_x, c.y, INSERT_W, 1)),
            ext: clip(Rect::new(fx, c.y + 1, mask_w, 1)),
            ext_insert: clip(Rect::new(insert_x, c.y + 1, INSERT_W, 1)),
            search: clip(Rect::new(fx, c.y + 2, search_w, 1)),
            replace_label: fx + search_w,
            replace: clip(Rect::new(replace_x, c.y + 2, c.right().saturating_sub(replace_x), 1)),
            regex: clip(Rect::new(fx, c.y + 3, width("[ ] Regex"), 1)),
            case_sensitive: clip(Rect::new(fx + width("[ ] Regex") + 2, c.y + 3, width("[ ] Case sensitive"), 1)),
            case: clip(Rect::new(fx, c.y + 4, case_w - 2, 1)),
            counter_label,
            start: clip(start),
            step_label,
            step: clip(step),
            digits_label,
            digits: clip(digits),
            header: clip(Rect::new(c.x, c.y + 6, c.w, 1)),
            table: clip(Rect::new(c.x, c.y + 7, c.w, t)),
            summary: clip(Rect::new(c.x, c.y + 8 + t, c.w, 1)),
            buttons: Rect::new(c.x, c.bottom().saturating_sub(1), c.w, 1),
        }
    }

    /// (old, new, status) columns of the table.
    fn columns(&self, table: Rect) -> (Rect, Rect, Rect) {
        let status_w = if self.status_w == 0 { 0 } else { (self.status_w + 2).min(table.w / 3) };
        let rest = table.w.saturating_sub(status_w + 1); // the scrollbar's column
        let old_w = rest / 2;
        let old = Rect::new(table.x, table.y, old_w, table.h);
        let new = Rect::new(table.x + old_w, table.y, rest - old_w, table.h);
        let status = Rect::new(new.right(), table.y, status_w, table.h);
        (old, new, status)
    }

    pub fn draw(&self, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let l = self.layout(screen);
        draw_box(f, l.outer, &self.title, ctx);
        let label = Style::new(c.foreground, c.surface);
        let cx = l.content.x;
        let typing = self.menu.is_none();
        for (y, text) in [(0, "Name"), (1, "Extension"), (2, "Search"), (4, "Case")] {
            put(f, cx, l.content.y + y, text, label, LABEL_W);
        }
        self.name_mask.draw(f, l.name, ctx, typing && self.focus == Field::Name);
        self.ext_mask.draw(f, l.ext, ctx, typing && self.focus == Field::Ext);
        for (r, field) in [(l.name_insert, Field::Name), (l.ext_insert, Field::Ext)] {
            let open = self.menu.as_ref().is_some_and(|m| m.target == field && m.target != Field::Case);
            let style = if open { Style::new(c.surface, c.border_active).with(Flags::BOLD) } else { Style::new(c.foreground, c.border) };
            fill(f, r, style.bg);
            put(f, r.x + r.w.saturating_sub(width(INSERT_LABEL)) / 2, r.y, INSERT_LABEL, style, r.w);
        }
        self.search.draw(f, l.search, ctx, typing && self.focus == Field::Search);
        put(f, l.replace_label + 1, l.search.y, "Replace", label, l.replace.x.saturating_sub(l.replace_label + 1));
        self.replace.draw(f, l.replace, ctx, typing && self.focus == Field::Replace);
        self.regex.draw(f, l.regex, ctx, self.focus == Field::Regex);
        self.case_sensitive.draw(f, l.case_sensitive, ctx, self.focus == Field::CaseSensitive);

        // Case: a combo box.
        let case_style =
            if self.focus == Field::Case { Style::new(c.cursor_fg, c.cursor_bg) } else { Style::new(c.foreground, c.background) };
        fill(f, l.case, case_style.bg);
        put(f, l.case.x + 1, l.case.y, CASE_LABELS[self.case], case_style, l.case.w.saturating_sub(3));
        if l.case.w > 1 {
            put(f, l.case.right() - 2, l.case.y, "▾", case_style, 1);
        }
        let y = l.case.y;
        put(f, l.counter_label, y, COUNTER_LABEL, label, l.start.x.saturating_sub(l.counter_label));
        put(f, l.step_label, y, " step ", label, l.step.x.saturating_sub(l.step_label));
        put(f, l.digits_label, y, " digits ", label, l.digits.x.saturating_sub(l.digits_label));
        for (r, spin, field) in
            [(l.start, &self.counter_start, Field::Start), (l.step, &self.counter_step, Field::Step), (l.digits, &self.counter_digits, Field::Digits)]
        {
            if r.w < 2 {
                continue;
            }
            spin.input.draw(f, Rect::new(r.x, r.y, r.w - 1, 1), ctx, typing && self.focus == field);
            put(f, r.right() - 1, r.y, "↕", Style::new(c.muted, c.background), 1);
        }

        self.draw_table(f, &l, ctx);
        put(f, l.summary.x, l.summary.y, &oc_grid::elide_right(&self.summary, l.summary.w as usize), Style::new(c.muted, c.surface), l.summary.w);
        self.buttons.draw(f, l.buttons, ctx, self.focus == Field::Buttons);

        if let Some(m) = &self.menu {
            if let Some(p) = &m.parent {
                p.menu.draw(f, screen, ctx);
            }
            m.menu.draw(f, screen, ctx);
        }
    }

    fn draw_table(&self, f: &mut Frame, l: &Lay, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let (old, new, status) = self.columns(l.table);
        let head = Style::new(c.accent, c.surface).with(Flags::BOLD);
        for (r, text) in [(old, "Old name"), (new, "New name")] {
            let x = r.x + r.w.saturating_sub(width(text)) / 2;
            put(f, x, l.header.y, text, head, r.w);
        }
        let mask_error = !self.plan.mask_error.is_empty();
        self.table.draw(f, l.table, ctx, |f, row, i, _| {
            let surface = |fg| Style::new(fg, c.surface);
            let Some(item) = self.items.get(i) else { return };
            put(f, old.x + 1, row.y, &oc_grid::elide_right(&item.name, old.w.saturating_sub(2) as usize), surface(c.foreground), old.w.saturating_sub(1));
            let Some(change) = self.plan.changes.get(i).filter(|_| !mask_error) else { return };
            let (new_text, status_text) = self.row(i);
            let (new_fg, status_fg) = if !change.error.is_empty() {
                (c.error, c.error)
            } else if !change.note.is_empty() {
                (c.muted, if change.pending { c.muted } else { c.warning })
            } else if change.changed() {
                (c.foreground, c.muted)
            } else {
                (c.muted, c.muted)
            };
            put(f, new.x + 1, row.y, &oc_grid::elide_right(&new_text, new.w.saturating_sub(2) as usize), surface(new_fg), new.w.saturating_sub(1));
            if status.w > 0 {
                put(f, status.x + 1, row.y, &oc_grid::elide_right(&status_text, status.w.saturating_sub(1) as usize), surface(status_fg), status.w);
            }
        });
    }

    // -- input ----------------------------------------------------------------------

    pub fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> DialogOutcome {
        match ev {
            Event::Key(k) => self.handle_key(app, k, screen),
            Event::Commit(_) | Event::Preedit(_) => {
                if self.menu.is_none() && matches!(self.focus, Field::Name | Field::Ext | Field::Search | Field::Replace) {
                    let focus = self.focus;
                    let input = self.input_mut(focus).expect("a text field");
                    let before = input.text.clone();
                    input.handle_event(ev, app.clipboard.as_mut());
                    if input.text != before {
                        self.changed(app);
                    }
                }
                DialogOutcome::Consumed
            }
            Event::Mouse(m) => self.handle_mouse(app, m, screen),
            _ => DialogOutcome::Consumed,
        }
    }

    fn handle_key(&mut self, app: &mut App, k: &KeyEvent, screen: Rect) -> DialogOutcome {
        if self.menu.is_some() {
            self.menu_key(app, k, screen);
            return DialogOutcome::Consumed;
        }
        let m = k.mods;
        let visible = self.table_rows(screen) as usize;
        match k.key {
            Key::Escape => return DialogOutcome::Cancel,
            Key::Tab if !m.ctrl && !m.alt && !m.logo => {
                self.move_focus(if m.shift { -1 } else { 1 });
                return DialogOutcome::Consumed;
            }
            Key::Enter if m.is_empty() && self.focus != Field::Buttons => return self.accept(app),
            Key::Down if m == Mods::ALT => {
                match self.focus {
                    Field::Name | Field::Ext => self.open_insert_menu(self.focus, screen),
                    Field::Case => self.open_case_menu(screen),
                    _ => {}
                }
                return DialogOutcome::Consumed;
            }
            Key::PageUp | Key::PageDown if m.is_empty() => {
                let page = visible.saturating_sub(1).max(1) as isize;
                self.table.scroll_by(if k.key == Key::PageUp { -page } else { page }, visible);
                return DialogOutcome::Consumed;
            }
            _ => {}
        }
        match self.focus {
            Field::Name | Field::Ext | Field::Search | Field::Replace => {
                if m.is_empty() && matches!(k.key, Key::Up | Key::Down) {
                    self.move_focus(if k.key == Key::Up { -1 } else { 1 });
                    return DialogOutcome::Consumed;
                }
                let focus = self.focus;
                let input = self.input_mut(focus).expect("a text field");
                let before = input.text.clone();
                input.handle_key(k, app.clipboard.as_mut());
                if input.text != before {
                    self.changed(app);
                }
            }
            Field::Regex | Field::CaseSensitive => {
                if m.is_empty() && matches!(k.key, Key::Up | Key::Down) {
                    self.move_focus(if k.key == Key::Up { -1 } else { 1 });
                    return DialogOutcome::Consumed;
                }
                let box_ = if self.focus == Field::Regex { &mut self.regex } else { &mut self.case_sensitive };
                if box_.handle_key(k, true) {
                    self.changed(app);
                }
            }
            Field::Case => match k.key {
                Key::Up if m.is_empty() => self.set_case(app, self.case.saturating_sub(1)),
                Key::Down if m.is_empty() => self.set_case(app, (self.case + 1).min(CASES.len() - 1)),
                // The rest of a Qt combo box's keys (PageUp/PageDown scroll the table here).
                Key::Home if m.is_empty() => self.set_case(app, 0),
                Key::End if m.is_empty() => self.set_case(app, CASES.len() - 1),
                Key::Space | Key::F(4) if m.is_empty() => self.open_case_menu(screen),
                _ => {}
            },
            Field::Start | Field::Step | Field::Digits => {
                let focus = self.focus;
                let spin = self.spin_mut(focus).expect("a number field");
                let before = spin.value;
                spin.handle_key(k, app.clipboard.as_mut());
                if spin.value != before {
                    self.changed(app);
                }
            }
            Field::Buttons => match self.buttons.handle_key(k, false) {
                RowOutcome::Activated(0) => return self.accept(app),
                RowOutcome::Activated(_) => return DialogOutcome::Cancel,
                RowOutcome::Ignored if m.is_empty() && k.key == Key::Up => self.move_focus(-1),
                _ => {}
            },
        }
        DialogOutcome::Consumed
    }

    fn handle_mouse(&mut self, app: &mut App, ev: &crate::input::MouseEvent, screen: Rect) -> DialogOutcome {
        if self.menu.is_some() {
            self.menu_mouse(app, ev, screen);
            return DialogOutcome::Consumed;
        }
        let l = self.layout(screen);
        let (col, row) = (ev.col, ev.row);
        match ev.kind {
            MouseKind::Down(MouseButton::Left) => {
                let fields = [
                    (l.name, Field::Name),
                    (l.ext, Field::Ext),
                    (l.search, Field::Search),
                    (l.replace, Field::Replace),
                    (l.start, Field::Start),
                    (l.step, Field::Step),
                    (l.digits, Field::Digits),
                ];
                if let Some(&(r, field)) = fields.iter().find(|(r, _)| r.contains(col, row)) {
                    self.set_focus(field);
                    if let Some(input) = self.input_mut(field) {
                        input.click(r, col);
                    }
                } else if l.name_insert.contains(col, row) {
                    self.open_insert_menu(Field::Name, screen);
                } else if l.ext_insert.contains(col, row) {
                    self.open_insert_menu(Field::Ext, screen);
                } else if l.case.contains(col, row) {
                    self.open_case_menu(screen);
                } else if self.regex.click(l.regex, col, row) {
                    self.set_focus(Field::Regex);
                    self.changed(app);
                } else if self.case_sensitive.click(l.case_sensitive, col, row) {
                    self.set_focus(Field::CaseSensitive);
                    self.changed(app);
                }
            }
            MouseKind::Up(MouseButton::Left) => match self.buttons.hit(l.buttons, col, row) {
                Some(0) => return self.accept(app),
                Some(_) => return DialogOutcome::Cancel,
                None => {}
            },
            MouseKind::Scroll(lines) if l.table.contains(col, row) => {
                self.table.scroll_by((-lines * 3.0).round() as isize, l.table.h as usize);
            }
            _ => {}
        }
        DialogOutcome::Consumed
    }

    /// The dialog as text (tests): fields, table and summary.
    pub fn text(&self) -> String {
        let mut out = format!("Name {}\nExtension {}\n", self.name_mask.text, self.ext_mask.text);
        for (i, item) in self.items.iter().enumerate() {
            let (new, status) = self.row(i);
            out += &format!("{} -> {new} {status}\n", item.name);
        }
        out + &self.summary()
    }
}

/// `&` is a hotkey marker in menu labels.
fn escape(label: &str) -> String {
    label.replace('&', "&&")
}

/// Run `f` on the open dialog `id` (taken out of the app state meanwhile).
pub(super) fn with_dialog(app: &mut App, id: u64, f: impl FnOnce(&mut RenameDialog, &mut App)) {
    let Some(mut d) = app.state::<RenameState>().dialog.take() else { return };
    if d.id == id {
        f(&mut d, app);
    }
    app.state::<RenameState>().dialog = Some(d);
}

/// The metadata reader handed over results (or finished).
fn metadata_arrived(app: &mut App, id: u64) {
    with_dialog(app, id, |d, app| {
        if d.take_metadata() {
            d.update(app);
        }
    });
}

/// The modal on the stack; the dialog itself is in [`RenameState`].
pub struct RenameModal {
    pub title: String,
}

impl Modal for RenameModal {
    fn title(&self) -> &str {
        &self.title
    }

    fn draw(&self, app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        if let Some(d) = app.try_state::<RenameState>().and_then(|s| s.dialog.as_ref()) {
            d.draw(f, screen, ctx);
        }
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        let Some(mut d) = app.state::<RenameState>().dialog.take() else {
            return ModalOutcome::Close;
        };
        match d.handle_event(app, ev, screen) {
            DialogOutcome::Consumed => {
                app.state::<RenameState>().dialog = Some(d);
                ModalOutcome::Consumed
            }
            DialogOutcome::Accept => {
                finish_dialog(app, d, true);
                ModalOutcome::Close
            }
            DialogOutcome::Cancel => {
                finish_dialog(app, d, false);
                ModalOutcome::Close
            }
        }
    }

    fn wants_text_input(&self) -> bool {
        true
    }

    fn text(&self) -> String {
        String::new()
    }
}
