//! Dialogs for server connections (port of `ui/connect_dialogs.py`): the
//! manager (⌘K), the connection editor (its form comes from the connection
//! type's fields, so add-on types get one too), the password prompt and the
//! providers' yes/no questions (host keys, certificates).

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use oc_config::Connection;
use oc_grid::{Frame, Style};
use oc_provider::{Field, FieldKind, FileSystemProvider, Params, Value, param_str};
use unicode_segmentation::UnicodeSegmentation;

use super::{connect_in_panel, delete_connection, description, has_password, label, provider_of, save_connection, sorted_saved, url};
use crate::app::App;
use crate::dialogs::{MessageBox, dialog_rects, draw_box, wrap};
use crate::geom::Rect;
use crate::input::{Event, Key, KeyEvent, MouseButton, MouseKind};
use crate::modal::{Modal, ModalId, ModalOutcome};
use crate::tasks::Reply;
use crate::widgets::button::RowOutcome;
use crate::widgets::{ButtonRow, Checkbox, DrawCtx, DropdownMenu, ListState, MenuEntry, MenuOutcome, TextInput, fill, put, put_elided, width};

/// What the editor or the password prompt answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub password: Option<String>,
    pub remember: bool,
}

const REMEMBER: &str = "Remember in the system keychain";
pub(crate) const MANAGER_TITLE: &str = "Connect to server";

// ---------------------------------------------------------------------------
// Secret fields: a TextInput shown as dots, never copied out
// ---------------------------------------------------------------------------

pub(super) fn masked(input: &TextInput) -> TextInput {
    let dots = |s: &str| "•".repeat(s.graphemes(true).count());
    let at = |offset: usize| input.text.get(..offset).map_or(0, |s| s.graphemes(true).count()) * '•'.len_utf8();
    TextInput {
        text: dots(&input.text),
        cursor: at(input.cursor),
        anchor: input.anchor.map(at),
        preedit: dots(&input.preedit),
        placeholder: input.placeholder.clone(),
    }
}

fn draw_secret(input: &TextInput, f: &mut Frame, r: Rect, ctx: &DrawCtx, focused: bool) {
    masked(input).draw(f, r, ctx, focused);
}

/// Keys for a secret field: no copying or cutting the password out.
fn secret_key(input: &mut TextInput, k: &KeyEvent, app: &mut App) {
    if k.mods.has_primary() && matches!(k.key, Key::Char('c' | 'x')) {
        return;
    }
    input.handle_key(k, app.clipboard.as_mut());
}

fn is_back_tab(k: &KeyEvent) -> bool {
    k.key == Key::Tab && k.mods == crate::input::Mods::SHIFT
}

// ---------------------------------------------------------------------------
// Questions from providers (host keys, certificates)
// ---------------------------------------------------------------------------

/// A yes/no question from a connection type, for a worker waiting in
/// `ask_ui`. Cancel is the default, so agreeing is always deliberate.
pub(super) fn ask_question(app: &mut App, title: &str, message: &str, ok_label: &str, reply: Reply<bool>) {
    let mb = MessageBox::new(title, message, &[ok_label, "Cancel"], 1).width(72).on_choice(move |_, choice| reply.send(choice == Some(0)));
    app.show(mb);
}

// ---------------------------------------------------------------------------
// Password prompt
// ---------------------------------------------------------------------------

type OnCredentials = Box<dyn FnOnce(&mut App, Option<Credentials>)>;

/// Ask for the password (or secret key) of `conn`; `reason` says why again.
pub(super) fn ask_password(app: &mut App, conn: &Connection, reason: &str, on_done: impl FnOnce(&mut App, Option<Credentials>) + 'static) {
    let secret_label = provider_of(conn).map_or("Password", |p| p.secret_label());
    app.show(PasswordBox {
        title: secret_label.to_string(),
        reason: reason.to_string(),
        prompt: format!("{secret_label} for {}:", description(conn)),
        input: TextInput::default(),
        remember: Checkbox::new(REMEMBER, false),
        buttons: ButtonRow::new(&["Connect", "Cancel"], 0),
        focus: 0,
        on_done: Some(Box::new(on_done)),
    });
}

pub struct PasswordBox {
    title: String,
    reason: String,
    prompt: String,
    pub input: TextInput,
    pub remember: Checkbox,
    buttons: ButtonRow,
    /// 0: the field, 1: the checkbox, 2: the buttons.
    focus: usize,
    on_done: Option<OnCredentials>,
}

const PASSWORD_WIDTH: u16 = 56;

struct PasswordRects {
    outer: Rect,
    reason: Vec<String>,
    prompt: Vec<String>,
    input: Rect,
    remember: Rect,
    buttons: Rect,
}

impl PasswordBox {
    fn rects(&self, screen: Rect) -> PasswordRects {
        let inner_w = PASSWORD_WIDTH.min(screen.w.saturating_sub(2)).saturating_sub(4) as usize;
        let reason = if self.reason.is_empty() { Vec::new() } else { wrap(&self.reason, inner_w.max(1)) };
        let prompt = wrap(&self.prompt, inner_w.max(1));
        let text_rows = (reason.len() + prompt.len()) as u16;
        let (outer, content) = dialog_rects(screen, PASSWORD_WIDTH, text_rows + 4);
        let row = |i: u16| Rect::new(content.x, content.y + i, content.w, 1);
        PasswordRects {
            outer,
            input: row(text_rows),
            remember: row(text_rows + 1),
            buttons: row(text_rows + 3),
            reason,
            prompt,
        }
    }

    fn finish(&mut self, app: &mut App, accept: bool) -> ModalOutcome {
        let creds = accept.then(|| Credentials { password: Some(self.input.text.clone()), remember: self.remember.checked });
        if let Some(f) = self.on_done.take() {
            f(app, creds);
        }
        ModalOutcome::Close
    }
}

impl Modal for PasswordBox {
    fn title(&self) -> &str {
        &self.title
    }

    fn text(&self) -> String {
        [self.reason.as_str(), self.prompt.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("\n")
    }

    fn wants_text_input(&self) -> bool {
        self.focus == 0
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let r = self.rects(screen);
        let content = draw_box(f, r.outer, &self.title, ctx);
        let mut y = content.y;
        for line in &r.reason {
            put(f, content.x, y, line, Style::new(c.muted, c.surface), content.w);
            y += 1;
        }
        for line in &r.prompt {
            put(f, content.x, y, line, Style::new(c.foreground, c.surface), content.w);
            y += 1;
        }
        draw_secret(&self.input, f, r.input, ctx, self.focus == 0);
        self.remember.draw(f, r.remember, ctx, self.focus == 1);
        self.buttons.draw(f, r.buttons, ctx, self.focus == 2);
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        match ev {
            Event::Key(k) => {
                match k.key {
                    Key::Escape => return self.finish(app, false),
                    Key::Tab if is_back_tab(k) => self.focus = (self.focus + 2) % 3,
                    Key::Tab => self.focus = (self.focus + 1) % 3,
                    _ if self.focus == 2 => {
                        if let RowOutcome::Activated(i) = self.buttons.handle_key(k, true) {
                            return self.finish(app, i == 0);
                        }
                    }
                    Key::Enter => return self.finish(app, true),
                    _ if self.focus == 1 => {
                        self.remember.handle_key(k, true);
                    }
                    _ => secret_key(&mut self.input, k, app),
                }
                ModalOutcome::Consumed
            }
            Event::Commit(_) | Event::Preedit(_) => {
                if self.focus == 0 {
                    self.input.handle_event(ev, app.clipboard.as_mut());
                }
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => {
                let r = self.rects(screen);
                match m.kind {
                    MouseKind::Down(MouseButton::Left) if r.input.contains(m.col, m.row) => {
                        self.focus = 0;
                        self.input.click(r.input, m.col);
                    }
                    MouseKind::Down(MouseButton::Left) if r.remember.contains(m.col, m.row) => {
                        self.focus = 1;
                        self.remember.click(r.remember, m.col, m.row);
                    }
                    MouseKind::Up(MouseButton::Left) => {
                        if let Some(i) = self.buttons.hit(r.buttons, m.col, m.row) {
                            return self.finish(app, i == 0);
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

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

type OnEdited = Box<dyn FnOnce(&mut App, Option<(Connection, Credentials)>)>;

/// Create (`conn` None) or edit a connection; `on_done` gets the result
/// (nothing is saved here). `has_password`: one is in the keychain.
pub(super) fn edit_connection(
    app: &mut App,
    conn: Option<Connection>,
    has_password: bool,
    on_done: impl FnOnce(&mut App, Option<(Connection, Credentials)>) + 'static,
) {
    app.show(ConnectionEditor::new(conn, has_password, Box::new(on_done)));
}

pub struct ConnectionEditor {
    title: String,
    providers: Vec<Arc<dyn FileSystemProvider>>,
    /// The chosen connection type (index into `providers`).
    pub kind: usize,
    pub name: TextInput,
    /// What was typed per param key, kept across type changes (e.g. the server name).
    values: HashMap<String, String>,
    pub fields: Vec<(Field, TextInput)>,
    pub password: TextInput,
    pub remember: Checkbox,
    pub error: String,
    buttons: ButtonRow,
    /// Row with the focus: 0 type, 1 name, 2.. fields, then password, remember, buttons.
    pub focus: usize,
    /// The type list, while open.
    menu: Option<DropdownMenu>,
    original: Option<Connection>,
    on_done: Option<OnEdited>,
}

const EDITOR_WIDTH: u16 = 64;

impl ConnectionEditor {
    fn new(conn: Option<Connection>, has_password: bool, on_done: OnEdited) -> Self {
        let providers = oc_provider::providers();
        let kind = conn.as_ref().and_then(|c| providers.iter().position(|p| p.scheme() == c.kind)).unwrap_or(0);
        let values = conn.as_ref().map(|c| c.params.keys().map(|k| (k.clone(), param_str(&c.params, k))).collect()).unwrap_or_default();
        let mut e = ConnectionEditor {
            title: if conn.is_some() { "Edit connection" } else { "New connection" }.to_string(),
            providers,
            kind,
            name: TextInput { placeholder: "optional".into(), ..TextInput::new(conn.as_ref().map_or("", |c| c.name.as_str())) },
            values,
            fields: Vec::new(),
            password: TextInput::default(),
            remember: Checkbox::new(REMEMBER, has_password),
            error: String::new(),
            buttons: ButtonRow::new(&["Save", "Cancel"], 0),
            focus: 0,
            menu: None,
            original: conn,
            on_done: Some(on_done),
        };
        e.kind_changed();
        e.focus = if e.fields.is_empty() { 1 } else { 2 };
        e
    }

    fn provider(&self) -> Option<&Arc<dyn FileSystemProvider>> {
        self.providers.get(self.kind)
    }

    /// Rebuild the type's fields, keeping what was typed.
    fn kind_changed(&mut self) {
        for (f, input) in &self.fields {
            self.values.insert(f.key.to_string(), input.text.clone());
        }
        let fields = self.provider().map(|p| p.fields()).unwrap_or_default();
        self.fields = fields
            .into_iter()
            .map(|f| {
                let default = f.default.as_ref().map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                });
                let text = self.values.get(f.key).cloned().or(default).unwrap_or_default();
                let input = TextInput { placeholder: f.placeholder.to_string(), ..TextInput::new(&text) };
                (f, input)
            })
            .collect();
        self.password.placeholder = self.provider().map_or("", |p| p.secret_hint()).to_string();
        self.error.clear();
    }

    fn set_kind(&mut self, kind: usize) {
        if kind != self.kind && kind < self.providers.len() {
            self.kind = kind;
            self.kind_changed();
        }
    }

    fn secret_label(&self) -> &'static str {
        self.provider().map_or("Password", |p| p.secret_label())
    }

    fn password_row(&self) -> usize {
        2 + self.fields.len()
    }

    fn rows(&self) -> usize {
        self.password_row() + 3 // password, remember, buttons
    }

    fn buttons_row(&self) -> usize {
        self.rows() - 1
    }

    fn label_width(&self) -> u16 {
        let labels = ["Type", "Name", self.secret_label()].into_iter().chain(self.fields.iter().map(|(f, _)| f.label));
        labels.map(width).max().unwrap_or(4) + 2
    }

    /// The dialog box and its content; row `i` of the form is content row `form_y(i)`.
    fn frame(&self, screen: Rect) -> (Rect, Rect) {
        // form rows, then error, blank, buttons
        let rows = self.password_row() as u16 + 2 + 3;
        dialog_rects(screen, EDITOR_WIDTH, rows)
    }

    /// (label, value) cells of form row `i` (not the buttons).
    fn row_rects(&self, content: Rect, i: usize) -> (Rect, Rect) {
        let lw = self.label_width().min(content.w / 2);
        let y = content.y + i as u16;
        (Rect::new(content.x, y, lw, 1), Rect::new(content.x + lw, y, content.w.saturating_sub(lw), 1))
    }

    fn error_rect(&self, content: Rect) -> Rect {
        content.row(self.password_row() as u16 + 2)
    }

    fn buttons_rect(&self, content: Rect) -> Rect {
        content.row(content.h.saturating_sub(1))
    }

    fn input_mut(&mut self, row: usize) -> Option<&mut TextInput> {
        match row {
            1 => Some(&mut self.name),
            r if r >= 2 && r < self.password_row() => self.fields.get_mut(r - 2).map(|(_, i)| i),
            r if r == self.password_row() => Some(&mut self.password),
            _ => None,
        }
    }

    fn open_menu(&mut self, screen: Rect) {
        let (_, content) = self.frame(screen);
        let (_, value) = self.row_rects(content, 0);
        let entries = self.providers.iter().map(|p| MenuEntry::item(p.title(), "")).collect();
        let mut menu = DropdownMenu::new(entries, value.x.saturating_sub(1), value.y + 1);
        menu.cursor = self.kind;
        self.menu = Some(menu);
    }

    /// The params as typed: numbers for number fields, empty ones left out.
    fn params(&self) -> Params {
        let mut params = Params::new();
        for (f, input) in &self.fields {
            let value = input.text.trim();
            if f.kind == FieldKind::Int
                && !value.is_empty()
                && value.chars().all(|c| c.is_ascii_digit())
                && let Ok(n) = value.parse::<i64>()
            {
                params.insert(f.key.to_string(), Value::from(n));
            } else if !value.is_empty() {
                params.insert(f.key.to_string(), Value::String(value.to_string()));
            }
        }
        params
    }

    fn save(&mut self, app: &mut App) -> ModalOutcome {
        let Some(provider) = self.provider().cloned() else {
            return ModalOutcome::Consumed;
        };
        let params = self.params();
        if let Some(error) = provider.validate(&params) {
            self.error = error;
            return ModalOutcome::Consumed;
        }
        let name = self.name.text.trim().to_string();
        let conn = match &self.original {
            Some(o) => Connection { kind: provider.scheme().to_string(), params, name, id: o.id.clone() },
            None => Connection::new(provider.scheme(), params, name),
        };
        let password = Some(self.password.text.clone()).filter(|p| !p.is_empty());
        let creds = Credentials { password, remember: self.remember.checked };
        if let Some(f) = self.on_done.take() {
            f(app, Some((conn, creds)));
        }
        ModalOutcome::Close
    }

    fn cancel(&mut self, app: &mut App) -> ModalOutcome {
        if let Some(f) = self.on_done.take() {
            f(app, None);
        }
        ModalOutcome::Close
    }

    fn move_focus(&mut self, delta: isize) {
        let n = self.rows() as isize;
        self.focus = (self.focus as isize + delta).rem_euclid(n) as usize;
    }

    fn key(&mut self, app: &mut App, k: &KeyEvent, screen: Rect) -> ModalOutcome {
        if let Some(menu) = &mut self.menu {
            match menu.handle_key(k) {
                MenuOutcome::Activated(i) => {
                    self.menu = None;
                    self.set_kind(i);
                }
                MenuOutcome::Close => self.menu = None,
                _ => {}
            }
            return ModalOutcome::Consumed;
        }
        let plain = k.mods.is_empty();
        match k.key {
            Key::Escape => return self.cancel(app),
            Key::Tab if is_back_tab(k) => self.move_focus(-1),
            Key::Tab => self.move_focus(1),
            Key::Up if plain => self.move_focus(-1),
            Key::Down if plain && self.focus != self.buttons_row() => self.move_focus(1),
            _ if self.focus == self.buttons_row() => {
                if let RowOutcome::Activated(i) = self.buttons.handle_key(k, true) {
                    return if i == 0 { self.save(app) } else { self.cancel(app) };
                }
            }
            _ if self.focus == 0 => match k.key {
                Key::Left if plain => self.set_kind((self.kind + self.providers.len().max(1) - 1) % self.providers.len().max(1)),
                Key::Right if plain => self.set_kind((self.kind + 1) % self.providers.len().max(1)),
                Key::Space | Key::Enter => self.open_menu(screen),
                Key::Down if k.mods.alt => self.open_menu(screen),
                _ => {}
            },
            Key::Enter => return self.save(app),
            _ if self.focus == self.password_row() + 1 => {
                self.remember.handle_key(k, true);
            }
            _ if self.focus == self.password_row() => secret_key(&mut self.password, k, app),
            _ => {
                let row = self.focus;
                if let Some(input) = self.input_mut(row) {
                    input.handle_key(k, app.clipboard.as_mut());
                }
            }
        }
        ModalOutcome::Consumed
    }

    fn mouse(&mut self, app: &mut App, m: &crate::input::MouseEvent, screen: Rect) -> ModalOutcome {
        if let Some(menu) = &mut self.menu {
            match menu.handle_mouse(m, screen) {
                MenuOutcome::Activated(i) => {
                    self.menu = None;
                    self.set_kind(i);
                }
                MenuOutcome::Close => self.menu = None,
                _ => {}
            }
            return ModalOutcome::Consumed;
        }
        let (_, content) = self.frame(screen);
        match m.kind {
            MouseKind::Down(MouseButton::Left) => {
                for row in 0..self.buttons_row() {
                    let (l, v) = self.row_rects(content, row);
                    if !(l.contains(m.col, m.row) || v.contains(m.col, m.row)) {
                        continue;
                    }
                    self.focus = row;
                    if row == 0 {
                        self.open_menu(screen);
                    } else if row == self.password_row() + 1 {
                        self.remember.checked = !self.remember.checked;
                    } else if let Some(input) = self.input_mut(row) {
                        input.click(v, m.col);
                    }
                }
            }
            MouseKind::Up(MouseButton::Left) => {
                if let Some(i) = self.buttons.hit(self.buttons_rect(content), m.col, m.row) {
                    return if i == 0 { self.save(app) } else { self.cancel(app) };
                }
            }
            _ => {}
        }
        ModalOutcome::Consumed
    }
}

impl Modal for ConnectionEditor {
    fn title(&self) -> &str {
        &self.title
    }

    fn text(&self) -> String {
        let mut lines = vec![format!("Type: {}", self.provider().map_or("", |p| p.title())), format!("Name: {}", self.name.text)];
        lines.extend(self.fields.iter().map(|(f, i)| format!("{}: {}", f.label, i.text)));
        lines.push(format!("{}: {}", self.secret_label(), "•".repeat(self.password.text.chars().count())));
        lines.push(format!("[{}] {REMEMBER}", if self.remember.checked { "x" } else { " " }));
        if !self.error.is_empty() {
            lines.push(self.error.clone());
        }
        lines.join("\n")
    }

    fn wants_text_input(&self) -> bool {
        self.menu.is_none() && self.focus >= 1 && self.focus <= self.password_row()
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let (outer, _) = self.frame(screen);
        let content = draw_box(f, outer, &self.title, ctx);
        let label_style = Style::new(c.foreground, c.surface);
        // Type
        let (l, v) = self.row_rects(content, 0);
        put(f, l.x, l.y, "Type", label_style, l.w);
        let focused = self.focus == 0 && self.menu.is_none();
        let style = if focused { Style::new(c.cursor_fg, c.cursor_bg) } else { Style::new(c.foreground, c.background) };
        fill(f, v, style.bg);
        put_elided(f, v.x, v.y, self.provider().map_or("", |p| p.title()), style, v.w.saturating_sub(2));
        put(f, v.right().saturating_sub(1), v.y, "▾", Style::new(if focused { c.cursor_fg } else { c.accent }, style.bg), 1);
        // Name, the type's fields, the secret
        let mut texts: Vec<(&str, &TextInput)> = vec![("Name", &self.name)];
        texts.extend(self.fields.iter().map(|(f, i)| (f.label, i)));
        for (k, (name, input)) in texts.into_iter().enumerate() {
            let row = k + 1;
            let (l, v) = self.row_rects(content, row);
            put(f, l.x, l.y, name, label_style, l.w);
            input.draw(f, v, ctx, self.focus == row && self.menu.is_none());
        }
        let row = self.password_row();
        let (l, v) = self.row_rects(content, row);
        put(f, l.x, l.y, self.secret_label(), label_style, l.w);
        draw_secret(&self.password, f, v, ctx, self.focus == row && self.menu.is_none());
        let (_, v) = self.row_rects(content, row + 1);
        self.remember.draw(f, v, ctx, self.focus == row + 1);
        if !self.error.is_empty() {
            let r = self.error_rect(content);
            put_elided(f, r.x, r.y, &self.error, Style::new(c.muted, c.surface), r.w);
        }
        self.buttons.draw(f, self.buttons_rect(content), ctx, self.focus == self.buttons_row());
        if let Some(menu) = &self.menu {
            menu.draw(f, screen, ctx);
        }
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        match ev {
            Event::Key(k) => self.key(app, k, screen),
            Event::Commit(_) | Event::Preedit(_) => {
                let row = self.focus;
                if self.menu.is_none()
                    && let Some(input) = self.input_mut(row)
                {
                    input.handle_event(ev, app.clipboard.as_mut());
                }
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => self.mouse(app, m, screen),
            _ => ModalOutcome::Consumed,
        }
    }
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// ⌘K: saved connections — connect, new, edit, duplicate, delete.
pub(super) fn show_manager(app: &mut App) {
    let me = Rc::new(Cell::new(None));
    let has = !sorted_saved(app).is_empty();
    let goto = app.registry.get("nav.goto").map(|c| c.shortcut_text()).unwrap_or_default();
    let hint = format!("Saved connections — or use Go to ({goto}) with an address: {}", oc_provider::address_examples());
    let manager = ConnectionManager {
        me: me.clone(),
        hint,
        list_focused: has,
        buttons: ButtonRow::new(&["Connect", "New…", "Edit…", "Duplicate", "Delete", "Close"], if has { 0 } else { 1 }),
        top: Cell::new(0),
    };
    let id = app.show(manager);
    me.set(Some(id));
}

pub struct ConnectionManager {
    /// Its own id once shown (an editor's Save closes it to connect).
    me: Rc<Cell<Option<ModalId>>>,
    hint: String,
    pub list_focused: bool,
    buttons: ButtonRow,
    top: Cell<usize>,
}

const MANAGER_WIDTH: u16 = 72;
const MANAGER_ROWS: (u16, u16) = (8, 14);

const CONNECT: usize = 0;
const NEW: usize = 1;
const EDIT: usize = 2;
const DUPLICATE: usize = 3;
const DELETE: usize = 4;
const CLOSE: usize = 5;

fn item_text(conn: &Connection) -> String {
    if conn.name.is_empty() {
        return url(conn);
    }
    let name = label(conn);
    let pad = 24usize.saturating_sub(width(&name) as usize);
    format!("{name}{}  {}", " ".repeat(pad), url(conn))
}

impl ConnectionManager {
    fn rects(&self, screen: Rect, n: usize) -> (Rect, Vec<String>, Rect, Rect) {
        let w = MANAGER_WIDTH.max(self.buttons.cells() + 6);
        let inner_w = w.min(screen.w.saturating_sub(2)).saturating_sub(4) as usize;
        let hint = wrap(&self.hint, inner_w.max(1));
        let list_rows = (n as u16).clamp(MANAGER_ROWS.0, MANAGER_ROWS.1);
        let (outer, content) = dialog_rects(screen, w, hint.len() as u16 + 1 + list_rows + 2);
        let list = Rect::new(content.x, content.y + hint.len() as u16 + 1, content.w, list_rows.min(content.h.saturating_sub(hint.len() as u16 + 3)));
        let buttons = content.row(content.h.saturating_sub(1));
        (outer, hint, list, buttons)
    }

    /// The highlighted row of `items` (by the remembered id).
    fn selected(app: &App, items: &[Connection]) -> usize {
        let id = app.try_state::<super::Connections>().and_then(|s| s.manager_selected.clone());
        id.and_then(|id| items.iter().position(|c| c.id == id)).unwrap_or(0)
    }

    fn select(app: &mut App, items: &[Connection], row: usize) {
        app.state::<super::Connections>().manager_selected = items.get(row).map(|c| c.id.clone());
    }

    fn list_state(&self, len: usize, cursor: usize, visible: usize) -> ListState {
        let mut l = ListState { len, cursor, top: self.top.get() };
        l.ensure_visible(visible);
        l
    }

    fn enabled_buttons(&self, has: bool) -> ButtonRow {
        let mut b = self.buttons.clone();
        for i in [CONNECT, EDIT, DUPLICATE, DELETE] {
            b.buttons[i].enabled = has;
        }
        if !b.buttons[b.focus].enabled {
            b.focus = NEW;
        }
        b
    }

    fn action(&mut self, app: &mut App, i: usize) -> ModalOutcome {
        let items = sorted_saved(app);
        let current = items.get(Self::selected(app, &items)).cloned();
        match (i, current) {
            (CONNECT, Some(conn)) => {
                let side = app.main.files_side();
                connect_in_panel(app, side, conn, None);
                return ModalOutcome::Close;
            }
            (NEW, _) => {
                let me = self.me.clone();
                edit_connection(app, None, false, move |app, edited| {
                    let Some((conn, creds)) = edited else { return };
                    save_connection(app, conn.clone(), Some(creds.clone()));
                    // A new connection is usually wanted right away.
                    if let Some(id) = me.get() {
                        app.close_modal(id);
                    }
                    let side = app.main.files_side();
                    connect_in_panel(app, side, conn, creds.password);
                });
            }
            (EDIT, Some(conn)) => {
                has_password(app, &conn.clone(), move |app, has| {
                    if !app.modal_titles().iter().any(|t| t == MANAGER_TITLE) {
                        return;
                    }
                    edit_connection(app, Some(conn), has, |app, edited| {
                        if let Some((conn, creds)) = edited {
                            save_connection(app, conn, Some(creds));
                        }
                    });
                });
            }
            (DUPLICATE, Some(conn)) => {
                let name = if conn.name.is_empty() { String::new() } else { format!("{} (copy)", conn.name) };
                save_connection(app, Connection::new(conn.kind.clone(), conn.params.clone(), name), None);
            }
            (DELETE, Some(conn)) => {
                let text = format!("Delete '{}'?", label(&conn));
                crate::dialogs::confirm(app, "Delete connection", &text, "Delete", move |app| delete_connection(app, conn));
            }
            (CLOSE, _) => return ModalOutcome::Close,
            _ => {}
        }
        ModalOutcome::Consumed
    }
}

impl Modal for ConnectionManager {
    fn title(&self) -> &str {
        MANAGER_TITLE
    }

    fn text(&self) -> String {
        self.hint.clone()
    }

    fn draw(&self, app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let items = sorted_saved(app);
        let (outer, hint, list, buttons) = self.rects(screen, items.len());
        let content = draw_box(f, outer, MANAGER_TITLE, ctx);
        for (i, line) in hint.iter().enumerate() {
            put(f, content.x, content.y + i as u16, line, Style::new(c.muted, c.surface), content.w);
        }
        let cursor = Self::selected(app, &items);
        let state = self.list_state(items.len(), cursor, list.h as usize);
        state.draw(f, list, ctx, |f, r, i, is_cursor| {
            let style = match (is_cursor, self.list_focused) {
                (true, true) => Style::new(c.cursor_fg, c.cursor_bg),
                (true, false) => Style::new(c.foreground, c.cursor_inactive_bg),
                _ => Style::new(c.foreground, c.surface),
            };
            fill(f, r, style.bg);
            put_elided(f, r.x + 1, r.y, &item_text(&items[i]), style, r.w.saturating_sub(2));
        });
        self.enabled_buttons(!items.is_empty()).draw(f, buttons, ctx, !self.list_focused);
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        let items = sorted_saved(app);
        let has = !items.is_empty();
        self.buttons = self.enabled_buttons(has);
        if !has {
            self.list_focused = false;
        }
        let (_, _, list, buttons) = self.rects(screen, items.len());
        let visible = list.h as usize;
        let cursor = Self::selected(app, &items);
        match ev {
            Event::Key(k) => {
                if k.key == Key::Escape {
                    return ModalOutcome::Close;
                }
                if k.key == Key::Tab {
                    self.list_focused = has && !self.list_focused;
                    return ModalOutcome::Consumed;
                }
                if !self.list_focused {
                    return match self.buttons.handle_key(k, true) {
                        RowOutcome::Activated(i) => self.action(app, i),
                        _ => ModalOutcome::Consumed,
                    };
                }
                match k.key {
                    Key::Enter => return self.action(app, CONNECT),
                    Key::Insert | Key::F(7) => return self.action(app, NEW),
                    Key::F(4) => return self.action(app, EDIT),
                    Key::Delete | Key::F(8) => return self.action(app, DELETE),
                    _ => {
                        let mut l = self.list_state(items.len(), cursor, visible);
                        if l.handle_key(k, visible) {
                            self.top.set(l.top);
                            Self::select(app, &items, l.cursor);
                        }
                    }
                }
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => {
                match m.kind {
                    MouseKind::Scroll(lines) => {
                        let mut l = self.list_state(items.len(), cursor, visible);
                        l.scroll_by((-lines * 3.0).round() as isize, visible);
                        self.top.set(l.top);
                    }
                    MouseKind::Down(MouseButton::Left) | MouseKind::DoubleClick(MouseButton::Left) => {
                        let l = self.list_state(items.len(), cursor, visible);
                        if let Some(i) = l.hit(list, m.col, m.row) {
                            self.list_focused = true;
                            Self::select(app, &items, i);
                            if matches!(m.kind, MouseKind::DoubleClick(_)) {
                                return self.action(app, CONNECT);
                            }
                        }
                    }
                    MouseKind::Up(MouseButton::Left) => {
                        if let Some(i) = self.buttons.hit(buttons, m.col, m.row) {
                            return self.action(app, i);
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
