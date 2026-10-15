//! Settings (⌘, / Ctrl+,; Options → Settings…, on macOS also in the app
//! menu): port of `ui/settings_dialog.py`. Pages Appearance, Preview and
//! Terminal on the left; changes apply and are saved as they are made (no
//! OK button: the window behind is the preview), each writing only the keys
//! it changes (`App::update_settings`). The color scheme editor is in
//! [`scheme_editor`].
//!
//! The dialog keeps only UI state (page, focus, open list); every value is
//! read from the settings when drawn, so it always shows what is saved.
//! Installed fonts and their styles are looked up on a worker (scanning the
//! fonts takes a while) and kept in [`FontInfo`].
//!
//! Keys: Tab / Shift+Tab and ↑/↓ move between controls, ←/→ change a list
//! or number, Enter/Space open a list, toggle or press; Esc closes. A number
//! can also be typed (applied as soon as it is in range, like a Qt spin box)
//! or turned with the mouse wheel.

pub mod color_picker;
pub mod form;
pub mod sample;
pub mod scheme_editor;

use std::collections::{HashMap, HashSet};

use oc_config::Settings;
use oc_config::themes::{self, Appearance, FONT_SIZE_RANGE, FRAME_STYLES, LINE_SPACING_RANGE};
use orchard_grid::{Frame, Style};

use self::form::{Choice, Popup, PopupOutcome};
use crate::app::App;
use crate::commands::{Command, MenuRole, Registry};
use crate::dialogs::{dialog_rects, draw_box, wrap};
use crate::geom::Rect;
use crate::input::{Event, Key, KeyEvent, Mods, MouseButton, MouseKind};
use crate::menus::Menu;
use crate::modal::{Modal, ModalOutcome};
use crate::widgets::{ButtonRow, Checkbox, DrawCtx, TextInput, fill, put_elided, width};

pub const TITLE: &str = "Settings";
pub const PAGES: [&str; 3] = ["Appearance", "Preview", "Terminal"];

/// 1.x: `width_chars=108`.
const WIDTH: u16 = 108;
const SECTIONS_W: u16 = 16;
const PAGE_ROWS: u16 = 16;
const LABEL_W: u16 = 14;
const FIELD_W: u16 = 28;
const SPIN_W: u16 = 12;
const SPACING_STEP: f64 = 0.05;

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("app.settings", "Settings…", open)
            .help("Appearance and other settings")
            .keys(&["Primary+,"])
            .menu(Menu::Options, "settings")
            .role(MenuRole::Preferences),
    );
}

/// Open the Settings window (fonts are looked up in the background meanwhile).
pub fn open(app: &mut App) {
    load_fonts(app);
    // 1.x reads color-schemes/ whenever it shows the list: files copied in
    // by hand (or saved by another instance) are there at once.
    app.refresh_color_schemes(|app| app.redraw());
    let shell = TextInput::new(&app.settings.terminal_shell);
    app.show(SettingsDialog { page: 0, focus: Focus::Close, popup: None, all_fonts: false, shell, typed: None });
}

// ---------------------------------------------------------------------------
// Fonts (looked up on a worker)
// ---------------------------------------------------------------------------

/// Installed font families and styles, and the default shell (for the
/// Terminal page's placeholder), filled in the background.
#[derive(Default)]
pub struct FontInfo {
    /// Monospaced families.
    pub mono: Option<Vec<String>>,
    /// Every family ("Include proportional fonts").
    pub all: Option<Vec<String>>,
    /// The family "Automatic" stands for.
    pub automatic: Option<String>,
    /// Style names by family.
    pub styles: HashMap<String, Vec<String>>,
    requested: HashSet<String>,
    started: bool,
    pub default_shell: Option<String>,
}

fn load_fonts(app: &mut App) {
    if std::mem::replace(&mut app.state::<FontInfo>().started, true) {
        request_styles(app);
        return;
    }
    app.tasks.run_in_background(
        |_| {
            let lib = orchard_grid::FontLibrary::system();
            let shell = oc_term::default_shell().join(" ");
            (lib.families(true), lib.families(false), lib.resolve_family(""), shell)
        },
        |app, (mono, all, automatic, shell)| {
            let info = app.state::<FontInfo>();
            info.mono = Some(mono);
            info.all = Some(all);
            info.automatic = automatic;
            info.default_shell = Some(shell);
            request_styles(app);
            app.redraw();
        },
    );
}

/// The family the Style list is for: the chosen one if installed, else the automatic one.
fn styled_family(app: &App) -> Option<String> {
    let info = app.try_state::<FontInfo>()?;
    let chosen = &app.settings.font_family;
    if !chosen.is_empty() && info.all.as_ref().is_some_and(|all| all.contains(chosen)) {
        return Some(chosen.clone());
    }
    info.automatic.clone()
}

/// Look up the styles of the current family if not known yet.
fn request_styles(app: &mut App) {
    let Some(family) = styled_family(app) else { return };
    if !app.state::<FontInfo>().requested.insert(family.clone()) {
        return;
    }
    app.tasks.run_in_background(
        {
            let family = family.clone();
            move |_| orchard_grid::FontLibrary::system().styles(&family)
        },
        move |app, styles| {
            app.state::<FontInfo>().styles.insert(family, styles);
            app.redraw();
        },
    );
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// Which color scheme setting a row is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// `color_scheme`
    Color,
    /// `light_scheme`
    Light,
    /// `dark_scheme`
    Dark,
}

impl Slot {
    pub fn label(self) -> &'static str {
        match self {
            Slot::Color => "Scheme",
            Slot::Light => "Light mode",
            Slot::Dark => "Dark mode",
        }
    }

    /// The scheme in use for this slot (defaults applied).
    pub fn value(self, a: &Appearance) -> &str {
        match self {
            Slot::Color => &a.color_scheme,
            Slot::Light => &a.light_scheme,
            Slot::Dark => &a.dark_scheme,
        }
    }

    /// The setting as stored ("" = default).
    pub fn raw(self, s: &Settings) -> &str {
        match self {
            Slot::Color => &s.color_scheme,
            Slot::Light => &s.light_scheme,
            Slot::Dark => &s.dark_scheme,
        }
    }

    pub fn set(self, s: &mut Settings, name: &str) {
        let field = match self {
            Slot::Color => &mut s.color_scheme,
            Slot::Light => &mut s.light_scheme,
            Slot::Dark => &mut s.dark_scheme,
        };
        *field = name.to_owned();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Id {
    FollowSystem,
    Scheme(Slot),
    Edit(Slot),
    Family,
    AllFonts,
    Style,
    Size,
    Spacing,
    Frames,
    MenuBar,
    Restore,
    Autoplay,
    PlayNext,
    Shell,
    Link,
}

impl Id {
    /// The 1.x tooltip, shown under the page while the control has the focus.
    fn hint(self) -> &'static str {
        match self {
            Id::AllFonts => "Proportional fonts don't keep columns aligned",
            Id::MenuBar => "Otherwise F9 shows it",
            Id::Autoplay => "Also in the player: the [x] Autoplay button, or the A key",
            Id::PlayNext => {
                "Selects the next audio or video file in the panel and plays it. Off: videos loop, audio stops at the end."
            }
            Id::Link => "cd in the terminal moves the panel; moving the panel types a cd in the shell (once it's back at its prompt)",
            Id::Size if cfg!(target_os = "macos") => "Also ⌘= / ⌘- / ⌘0",
            Id::Size => "Also Ctrl+= / Ctrl+- / Ctrl+0",
            _ => "",
        }
    }
}

enum Kind {
    Check(Checkbox),
    /// Labels (None: separator), the setting value of each, the current one.
    Combo { items: Vec<Choice>, values: Vec<String>, current: usize },
    Spin(String),
    Button(&'static str),
    Text,
}

struct Ctl {
    id: Id,
    rect: Rect,
    kind: Kind,
}

enum TextKind {
    Heading,
    Label,
    Muted,
}

/// A page's controls and texts, placed for the current size and settings.
struct PageLayout {
    ctls: Vec<Ctl>,
    texts: Vec<(Rect, String, TextKind)>,
    /// Where the Sample goes (Appearance).
    sample: Option<Rect>,
}

impl PageLayout {
    fn ctl(&self, id: Id) -> Option<&Ctl> {
        self.ctls.iter().find(|c| c.id == id)
    }
}

/// The part of `r` inside `to` (empty, at `to`'s corner, if none).
fn clip(r: Rect, to: Rect) -> Rect {
    let (x, y) = (r.x.clamp(to.x, to.right()), r.y.clamp(to.y, to.bottom()));
    Rect::new(x, y, r.right().min(to.right()).saturating_sub(x), r.bottom().min(to.bottom()).saturating_sub(y))
}

fn check(id: Id, label: &str, on: bool, r: Rect) -> Ctl {
    Ctl { id, rect: r, kind: Kind::Check(Checkbox::new(label, on)) }
}

/// The Appearance page (1.x `AppearancePage`).
fn appearance_page(app: &App, d: &SettingsDialog, page: Rect) -> PageLayout {
    let a = app.settings.appearance();
    let mut ctls = Vec::new();
    let mut texts = Vec::new();
    let x = page.x;
    let fx = x + LABEL_W;
    let form_w = LABEL_W + FIELD_W + 1 + 9;
    let mut y = page.y;
    let heading = |texts: &mut Vec<_>, y: &mut u16, text: &str| {
        if *y > page.y {
            *y += 1;
        }
        texts.push((Rect::new(x, *y, form_w, 1), text.to_owned(), TextKind::Heading));
        *y += 1;
    };
    let labelled = |texts: &mut Vec<_>, y: u16, text: &str| texts.push((Rect::new(x, y, LABEL_W - 1, 1), text.to_owned(), TextKind::Label));

    heading(&mut texts, &mut y, "Color scheme");
    ctls.push(check(Id::FollowSystem, "Follow the system's light / dark mode", a.follow_system_appearance, Rect::new(x, y, form_w, 1)));
    y += 1;
    let schemes = app.color_schemes();
    let builtin: Vec<&str> = schemes.iter().map(|s| s.name.as_str()).filter(|n| themes::is_builtin(n)).collect();
    let custom: Vec<&str> = schemes.iter().map(|s| s.name.as_str()).filter(|n| !themes::is_builtin(n)).collect();
    let slots: &[Slot] = if a.follow_system_appearance { &[Slot::Light, Slot::Dark] } else { &[Slot::Color] };
    for &slot in slots {
        let mut items: Vec<Choice> = builtin.iter().map(|n| Some((*n).to_owned())).collect();
        if !custom.is_empty() {
            items.push(None);
            items.extend(custom.iter().map(|n| Some((*n).to_owned())));
        }
        let values: Vec<String> = items.iter().map(|i| i.clone().unwrap_or_default()).collect();
        let current = values.iter().position(|v| v == slot.value(&a)).unwrap_or(0);
        labelled(&mut texts, y, slot.label());
        ctls.push(Ctl { id: Id::Scheme(slot), rect: Rect::new(fx, y, FIELD_W, 1), kind: Kind::Combo { items, values, current } });
        ctls.push(Ctl { id: Id::Edit(slot), rect: Rect::new(fx + FIELD_W + 1, y, 9, 1), kind: Kind::Button("Edit…") });
        y += 1;
    }

    heading(&mut texts, &mut y, "Font");
    let info = app.try_state::<FontInfo>();
    let automatic = info.and_then(|i| i.automatic.clone());
    let mut families: Vec<String> = info.and_then(|i| if d.all_fonts { i.all.clone() } else { i.mono.clone() }).unwrap_or_default();
    if !a.font_family.is_empty() && !families.contains(&a.font_family) {
        // A proportional font chosen earlier (or the list isn't there yet).
        let installed = info.and_then(|i| i.all.as_ref()).is_none_or(|all| all.contains(&a.font_family));
        if installed {
            families.push(a.font_family.clone());
        }
    }
    let auto_label = match &automatic {
        Some(f) => format!("Automatic ({f})"),
        None => "Automatic".to_owned(),
    };
    let mut items: Vec<Choice> = vec![Some(auto_label)];
    let mut values = vec![String::new()];
    for f in families {
        items.push(Some(f.clone()));
        values.push(f);
    }
    let current = values.iter().position(|v| *v == a.font_family).unwrap_or(0);
    labelled(&mut texts, y, "Typeface");
    ctls.push(Ctl { id: Id::Family, rect: Rect::new(fx, y, FIELD_W, 1), kind: Kind::Combo { items, values, current } });
    y += 1;
    ctls.push(check(Id::AllFonts, "Include proportional fonts", d.all_fonts, Rect::new(fx, y, form_w - LABEL_W, 1)));
    y += 1;
    let styles = styled_family(app)
        .and_then(|f| info.and_then(|i| i.styles.get(&f).cloned()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| vec!["Regular".to_owned()]);
    let current = styles
        .iter()
        .position(|s| *s == a.font_style)
        .or_else(|| styles.iter().position(|s| s == "Regular"))
        .unwrap_or(0);
    labelled(&mut texts, y, "Style");
    ctls.push(Ctl {
        id: Id::Style,
        rect: Rect::new(fx, y, FIELD_W, 1),
        kind: Kind::Combo { items: styles.iter().cloned().map(Some).collect(), values: styles, current },
    });
    y += 1;
    // While a number is being typed it shows as typed (Qt's spin box text).
    let typed = |id: Id| d.typed.as_ref().filter(|(t, _)| *t == id).map(|(_, text)| text.clone());
    labelled(&mut texts, y, "Size");
    let size = typed(Id::Size).unwrap_or_else(|| a.font_size.to_string());
    ctls.push(Ctl { id: Id::Size, rect: Rect::new(fx, y, SPIN_W, 1), kind: Kind::Spin(format!("{size} pt")) });
    y += 1;
    labelled(&mut texts, y, "Line spacing");
    let spacing = typed(Id::Spacing).unwrap_or_else(|| format!("{:.2}", a.line_spacing));
    ctls.push(Ctl { id: Id::Spacing, rect: Rect::new(fx, y, SPIN_W, 1), kind: Kind::Spin(format!("{spacing} ×")) });
    y += 1;

    heading(&mut texts, &mut y, "Window");
    let items: Vec<Choice> = FRAME_STYLES.iter().map(|(_, l)| Some((*l).to_owned())).collect();
    let values: Vec<String> = FRAME_STYLES.iter().map(|(k, _)| (*k).to_owned()).collect();
    let current = values.iter().position(|v| *v == a.frame_style).unwrap_or(0);
    labelled(&mut texts, y, "Frames");
    ctls.push(Ctl { id: Id::Frames, rect: Rect::new(fx, y, FIELD_W, 1), kind: Kind::Combo { items, values, current } });
    y += 1;
    ctls.push(check(Id::MenuBar, "Always show the menu bar", app.settings.menu_bar_visible.unwrap_or(false), Rect::new(x, y, form_w, 1)));

    // The Sample on the right (if it fits next to the form), Restore defaults below it.
    let sample = (page.w >= form_w + 2 + sample::WIDTH).then(|| {
        let sx = page.right() - sample::WIDTH;
        texts.push((Rect::new(sx, page.y, sample::WIDTH, 1), "Sample".to_owned(), TextKind::Heading));
        Rect::new(sx, page.y + 1, sample::WIDTH, sample::HEIGHT)
    });
    let restore = "Restore defaults";
    let rw = width(restore) + 4;
    ctls.push(Ctl { id: Id::Restore, rect: Rect::new(page.right().saturating_sub(rw), page.bottom() - 1, rw, 1), kind: Kind::Button(restore) });
    PageLayout { ctls, texts, sample }
}

/// The Preview page (1.x `PreviewPage`).
fn preview_page(app: &App, page: Rect) -> PageLayout {
    let s = &app.settings;
    let (x, y, w) = (page.x, page.y, page.w);
    PageLayout {
        ctls: vec![
            check(Id::Autoplay, "Start playing as soon as a file is selected (autoplay)", s.autoplay_enabled(), Rect::new(x, y + 1, w, 1)),
            check(Id::PlayNext, "Play the next file when one ends", s.play_next_enabled(), Rect::new(x, y + 2, w, 1)),
        ],
        texts: vec![(Rect::new(x, y, w, 1), "Audio and video".to_owned(), TextKind::Heading)],
        sample: None,
    }
}

/// The Terminal page (1.x `TerminalPage`).
fn terminal_page(app: &App, page: Rect) -> PageLayout {
    let (x, y, w) = (page.x, page.y, page.w);
    PageLayout {
        ctls: vec![
            Ctl { id: Id::Shell, rect: Rect::new(x, y + 1, w, 1), kind: Kind::Text },
            check(Id::Link, "Link each side's terminal and panel folders", app.settings.terminal_link_enabled(), Rect::new(x, y + 5, w, 1)),
        ],
        texts: vec![
            (Rect::new(x, y, w, 1), "Shell".to_owned(), TextKind::Heading),
            (
                Rect::new(x, y + 2, w, 1),
                "Used for new terminal sessions. Empty: your login shell (PowerShell on Windows).".to_owned(),
                TextKind::Muted,
            ),
            (Rect::new(x, y + 4, w, 1), "Panels".to_owned(), TextKind::Heading),
        ],
        sample: None,
    }
}

// ---------------------------------------------------------------------------
// Applying changes
// ---------------------------------------------------------------------------

fn apply(app: &mut App, change: impl Fn(&mut Settings) + Send + Sync + 'static) {
    app.update_settings(change);
    request_styles(app);
}

fn toggle(app: &mut App, d: &mut SettingsDialog, id: Id) {
    let s = &app.settings;
    match id {
        Id::FollowSystem => {
            let on = !s.appearance().follow_system_appearance;
            apply(app, move |s| s.follow_system_appearance = Some(on));
        }
        Id::AllFonts => d.all_fonts = !d.all_fonts,
        Id::MenuBar => {
            let on = !s.menu_bar_visible.unwrap_or(false);
            apply(app, move |s| s.menu_bar_visible = Some(on));
        }
        Id::Autoplay => {
            let on = !s.autoplay_enabled();
            apply(app, move |s| {
                s.extra.insert("preview.media.autoplay".into(), on.into());
            });
        }
        Id::PlayNext => {
            let on = !s.play_next_enabled();
            apply(app, move |s| s.play_next = Some(on));
        }
        Id::Link => {
            let on = !s.terminal_link_enabled();
            apply(app, move |s| s.terminal_link = Some(on));
        }
        _ => {}
    }
}

/// A list entry was chosen.
fn choose(app: &mut App, id: Id, value: String) {
    let s = &app.settings;
    match id {
        Id::Scheme(slot) if slot.value(&s.appearance()) != value => apply(app, move |s| slot.set(s, &value)),
        Id::Family if s.font_family != value => apply(app, move |s| {
            s.font_family = value.clone();
            s.font_style = String::new();
        }),
        Id::Style if s.font_style != value => apply(app, move |s| s.font_style = value.clone()),
        Id::Frames if s.appearance().frame_style != value => apply(app, move |s| s.frame_style = value.clone()),
        _ => {}
    }
}

/// ←/→ on a number.
fn spin(app: &mut App, id: Id, delta: i32) {
    let a = app.settings.appearance();
    match id {
        Id::Size => {
            let (lo, hi) = FONT_SIZE_RANGE;
            let size = (a.font_size + delta as i64).clamp(lo, hi);
            if size != a.font_size {
                apply(app, move |s| s.font_size = Some(size));
            }
        }
        Id::Spacing => {
            let (lo, hi) = LINE_SPACING_RANGE;
            let v = ((a.line_spacing + delta as f64 * SPACING_STEP).clamp(lo, hi) * 100.0).round() / 100.0;
            if (v - a.line_spacing).abs() > 1e-9 {
                apply(app, move |s| s.line_spacing = Some(v));
            }
        }
        _ => {}
    }
}

/// A number typed into a spin box: the text as it may stand (Qt's validator:
/// digits, for Line spacing one decimal point and two decimals, never above
/// the maximum), and the value once it is in range.
fn typed_value(id: Id, text: &str) -> Option<(String, Option<f64>)> {
    let text = text.replace(',', ".");
    let (lo, hi) = match id {
        Id::Size => (FONT_SIZE_RANGE.0 as f64, FONT_SIZE_RANGE.1 as f64),
        Id::Spacing => LINE_SPACING_RANGE,
        _ => return None,
    };
    if text.is_empty() {
        return Some((text, None));
    }
    let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    let ok = match (id, text.split_once('.')) {
        (_, None) => digits(&text),
        (Id::Spacing, Some((int, frac))) => digits(int) && digits(frac) && frac.len() <= 2,
        _ => false,
    };
    let value: f64 = if ok { format!("0{text}").parse().ok()? } else { return None };
    if value > hi {
        return None;
    }
    Some((text, (value >= lo).then_some(value)))
}

/// Apply a typed spin box value.
fn set_typed(app: &mut App, id: Id, value: f64) {
    let a = app.settings.appearance();
    match id {
        Id::Size if value as i64 != a.font_size => {
            let size = value as i64;
            apply(app, move |s| s.font_size = Some(size));
        }
        Id::Spacing if (value - a.line_spacing).abs() > 1e-9 => {
            let v = (value * 100.0).round() / 100.0;
            apply(app, move |s| s.line_spacing = Some(v));
        }
        _ => {}
    }
}

/// 1.x `restore_defaults`: every appearance setting back to "not chosen".
fn restore_defaults(app: &mut App) {
    apply(app, |s| {
        s.color_scheme = String::new();
        s.follow_system_appearance = None;
        s.light_scheme = String::new();
        s.dark_scheme = String::new();
        s.font_family = String::new();
        s.font_style = String::new();
        s.font_size = None;
        s.line_spacing = None;
        s.frame_style = String::new();
        s.menu_bar_visible = None;
    });
}

// ---------------------------------------------------------------------------
// The dialog
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sections,
    Ctl(Id),
    Close,
}

pub struct SettingsDialog {
    pub page: usize,
    pub focus: Focus,
    /// An open list and the control it belongs to.
    popup: Option<(Id, Popup)>,
    /// "Include proportional fonts" (not a setting, like 1.x).
    all_fonts: bool,
    shell: TextInput,
    /// A number being typed into a spin box, as typed.
    typed: Option<(Id, String)>,
}

struct Frame0 {
    outer: Rect,
    sections: Rect,
    page: Rect,
    hint: Rect,
    buttons: Rect,
}

impl SettingsDialog {
    fn rects(screen: Rect) -> Frame0 {
        let (outer, content) = dialog_rects(screen, WIDTH, PAGE_ROWS + 5);
        let sections = Rect::new(content.x, content.y, SECTIONS_W, PAGE_ROWS.min(content.h));
        let page = Rect::new(content.x + SECTIONS_W + 2, content.y, content.w.saturating_sub(SECTIONS_W + 2), PAGE_ROWS.min(content.h));
        let hint = Rect::new(content.x, content.y + PAGE_ROWS + 1, content.w, 2);
        let buttons = Rect::new(content.x, content.bottom().saturating_sub(1), content.w, 1);
        Frame0 { outer, sections, page, hint, buttons }
    }

    fn layout(&self, app: &App, page: Rect) -> PageLayout {
        let mut l = match self.page {
            0 => appearance_page(app, self, page),
            1 => preview_page(app, page),
            _ => terminal_page(app, page),
        };
        // In a small window nothing may reach out of the dialog.
        for c in &mut l.ctls {
            c.rect = clip(c.rect, page);
        }
        for (r, _, _) in &mut l.texts {
            *r = clip(*r, page);
        }
        l
    }

    fn close_row() -> ButtonRow {
        ButtonRow::new(&["Close"], 0)
    }

    /// Save the shell command if it was edited (1.x: `editingFinished`).
    fn commit_shell(&mut self, app: &mut App) {
        let value = self.shell.text.trim().to_owned();
        if value != app.settings.terminal_shell {
            apply(app, move |s| s.terminal_shell = value.clone());
        }
    }

    fn set_focus(&mut self, app: &mut App, focus: Focus) {
        if self.focus == Focus::Ctl(Id::Shell) && focus != self.focus {
            self.commit_shell(app);
        }
        if focus != self.focus {
            // Leaving a spin box: it shows the value in effect again (Qt's fixup).
            self.typed = None;
        }
        self.focus = focus;
    }

    /// A key typed into a focused spin box: digits (and a decimal point),
    /// Backspace. False if it isn't one of those.
    fn type_into_spin(&mut self, app: &mut App, id: Id, k: &KeyEvent) -> bool {
        let current = match &self.typed {
            Some((t, text)) if *t == id => Some(text.clone()),
            _ => None,
        };
        let text = match (k.key, k.typed_text()) {
            (Key::Backspace, _) if k.mods.is_empty() => {
                // Without typing first, Backspace edits the number shown.
                let a = app.settings.appearance();
                let mut text = current.unwrap_or_else(|| if id == Id::Size { a.font_size.to_string() } else { format!("{:.2}", a.line_spacing) });
                text.pop();
                text
            }
            (_, Some(t)) if t.chars().all(|c| c.is_ascii_digit() || (id == Id::Spacing && matches!(c, '.' | ','))) => {
                // The first digit replaces the number (Qt selects it on focus).
                current.unwrap_or_default() + t
            }
            _ => return false,
        };
        if let Some((text, value)) = typed_value(id, &text) {
            self.typed = Some((id, text));
            if let Some(v) = value {
                set_typed(app, id, v);
            }
        }
        true
    }

    /// Sections, the page's controls, Close.
    fn order(&self, l: &PageLayout) -> Vec<Focus> {
        let mut v = vec![Focus::Sections];
        v.extend(l.ctls.iter().map(|c| Focus::Ctl(c.id)));
        v.push(Focus::Close);
        v
    }

    fn move_focus(&mut self, app: &mut App, l: &PageLayout, delta: i32, wrap: bool) {
        let order = self.order(l);
        let i = order.iter().position(|f| *f == self.focus).unwrap_or(order.len() - 1) as i32;
        let n = order.len() as i32;
        let next = if wrap { (i + delta).rem_euclid(n) } else { (i + delta).clamp(0, n - 1) };
        self.set_focus(app, order[next as usize]);
    }

    fn set_page(&mut self, app: &mut App, page: usize) {
        self.set_focus(app, Focus::Sections);
        self.page = page.min(PAGES.len() - 1);
        self.popup = None;
    }

    fn close(&mut self, app: &mut App) -> ModalOutcome {
        self.commit_shell(app);
        ModalOutcome::Close
    }

    /// Enter / Space / a click on a control.
    fn activate(&mut self, app: &mut App, ctl: &Ctl) -> ModalOutcome {
        match &ctl.kind {
            Kind::Check(_) => toggle(app, self, ctl.id),
            Kind::Combo { items, current, .. } => self.popup = Some((ctl.id, Popup::open(0, ctl.rect, items, *current))),
            Kind::Button(_) => match ctl.id {
                Id::Restore => restore_defaults(app),
                Id::Edit(slot) => scheme_editor::open(app, slot),
                _ => {}
            },
            Kind::Spin(_) | Kind::Text => {}
        }
        ModalOutcome::Consumed
    }

    fn popup_event(&mut self, app: &mut App, l: &PageLayout, ev: &Event, screen: Rect) -> ModalOutcome {
        let Some((id, popup)) = self.popup.as_mut() else { return ModalOutcome::Ignored };
        let id = *id;
        let Some(Ctl { kind: Kind::Combo { items, values, .. }, .. }) = l.ctl(id) else {
            self.popup = None;
            return ModalOutcome::Consumed;
        };
        let outcome = match ev {
            Event::Key(k) => popup.handle_key(k, items),
            Event::Mouse(m) => popup.handle_mouse(m, items, screen),
            _ => PopupOutcome::Consumed,
        };
        match outcome {
            PopupOutcome::Chosen(i) => {
                self.popup = None;
                // The list may have changed while open (fonts arriving in the background).
                if let Some(value) = values.get(i) {
                    choose(app, id, value.clone());
                }
            }
            PopupOutcome::Closed => self.popup = None,
            PopupOutcome::Consumed => {}
        }
        ModalOutcome::Consumed
    }

    fn key(&mut self, app: &mut App, l: &PageLayout, k: &KeyEvent) -> ModalOutcome {
        let plain = k.mods.is_empty();
        match k.key {
            Key::Escape => return self.close(app),
            Key::Tab if plain || k.mods == Mods::SHIFT => {
                self.move_focus(app, l, if k.mods.shift { -1 } else { 1 }, true);
                return ModalOutcome::Consumed;
            }
            _ => {}
        }
        match self.focus {
            Focus::Sections => match k.key {
                Key::Up | Key::Down if plain => {
                    let page = if k.key == Key::Up { self.page.saturating_sub(1) } else { self.page + 1 };
                    self.set_page(app, page);
                }
                Key::Home if plain => self.set_page(app, 0),
                Key::End if plain => self.set_page(app, PAGES.len() - 1),
                Key::Right | Key::Enter if plain => self.move_focus(app, l, 1, false),
                _ => {}
            },
            Focus::Close => match k.key {
                Key::Enter | Key::Space if plain => return self.close(app),
                Key::Up | Key::Left if plain => self.move_focus(app, l, -1, false),
                _ => {}
            },
            Focus::Ctl(id) => {
                let Some(ctl) = l.ctl(id) else {
                    self.focus = Focus::Sections;
                    return ModalOutcome::Consumed;
                };
                match (&ctl.kind, k.key) {
                    (_, Key::Up | Key::Down) if plain => self.move_focus(app, l, if k.key == Key::Up { -1 } else { 1 }, false),
                    (Kind::Text, Key::Enter) if plain => self.commit_shell(app),
                    (Kind::Text, _) => {
                        self.shell.handle_key(k, app.clipboard.as_mut());
                    }
                    (Kind::Combo { items, values, current }, Key::Left | Key::Right) if plain => {
                        let i = form::step(items, *current, if k.key == Key::Left { -1 } else { 1 });
                        if i != *current {
                            choose(app, id, values[i].clone());
                        }
                    }
                    (Kind::Spin(_), _) if self.type_into_spin(app, id, k) => {}
                    (Kind::Spin(_), Key::Enter) if plain => self.typed = None,
                    (Kind::Spin(_), Key::Left | Key::Right) if plain => {
                        self.typed = None;
                        spin(app, id, if k.key == Key::Left { -1 } else { 1 })
                    }
                    (Kind::Spin(_), Key::Char('-' | '+' | '=')) if k.mods.is_typing() => {
                        self.typed = None;
                        spin(app, id, if k.text.as_deref() == Some("-") { -1 } else { 1 })
                    }
                    (Kind::Spin(_), Key::PageUp | Key::PageDown) if plain => {
                        self.typed = None;
                        spin(app, id, if k.key == Key::PageDown { -5 } else { 5 })
                    }
                    (_, Key::Left) if plain => self.move_focus(app, l, -1, false),
                    (_, Key::Right) if plain => self.move_focus(app, l, 1, false),
                    (_, Key::Enter | Key::Space) if plain => return self.activate(app, ctl),
                    _ => {}
                }
            }
        }
        ModalOutcome::Consumed
    }

    fn mouse(&mut self, app: &mut App, l: &PageLayout, r: &Frame0, m: &crate::input::MouseEvent) -> ModalOutcome {
        let (col, row) = (m.col, m.row);
        match m.kind {
            MouseKind::Down(MouseButton::Left) => {
                if r.sections.contains(col, row) {
                    let i = (row - r.sections.y) as usize;
                    if i < PAGES.len() {
                        self.set_page(app, i);
                    }
                } else if let Some(ctl) = l.ctls.iter().find(|c| c.rect.contains(col, row)) {
                    self.set_focus(app, Focus::Ctl(ctl.id));
                    match &ctl.kind {
                        Kind::Spin(_) => {
                            self.typed = None;
                            spin(app, ctl.id, form::spin_hit(ctl.rect, col))
                        }
                        Kind::Text => self.shell.click(ctl.rect, col),
                        Kind::Button(_) => {}
                        _ => return self.activate(app, ctl),
                    }
                }
            }
            // The wheel over a spin box turns it (Qt: wheel focus).
            MouseKind::Scroll(lines) if lines != 0.0 => {
                if let Some(ctl) = l.ctls.iter().find(|c| c.rect.contains(col, row) && matches!(c.kind, Kind::Spin(_))) {
                    self.set_focus(app, Focus::Ctl(ctl.id));
                    self.typed = None;
                    let steps = if lines.abs() < 1.0 { lines.signum() } else { lines.round() };
                    spin(app, ctl.id, steps.clamp(-10.0, 10.0) as i32);
                }
            }
            MouseKind::Up(MouseButton::Left) => {
                if Self::close_row().hit(r.buttons, col, row).is_some() {
                    return self.close(app);
                }
                if let Some(ctl) = l.ctls.iter().find(|c| c.rect.contains(col, row) && matches!(c.kind, Kind::Button(_))) {
                    return self.activate(app, ctl);
                }
            }
            _ => {}
        }
        ModalOutcome::Consumed
    }
}

impl Modal for SettingsDialog {
    fn title(&self) -> &str {
        TITLE
    }

    fn text(&self) -> String {
        format!("page={} focus={:?} list_open={}", PAGES[self.page], self.focus, self.popup.is_some())
    }

    fn wants_text_input(&self) -> bool {
        self.focus == Focus::Ctl(Id::Shell)
    }

    fn draw(&self, app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let r = Self::rects(screen);
        draw_box(f, r.outer, TITLE, ctx);

        // Sections (1.x: a QListWidget).
        for (i, title) in PAGES.iter().enumerate() {
            let row = r.sections.row(i as u16);
            if row.is_empty() {
                break;
            }
            let style = match (i == self.page, self.focus == Focus::Sections) {
                (true, true) => Style::new(c.cursor_fg, c.cursor_bg),
                (true, false) => Style::new(c.foreground, c.cursor_inactive_bg),
                _ => Style::new(c.foreground, c.surface),
            };
            fill(f, row, style.bg);
            put_elided(f, row.x + 1, row.y, title, style, row.w.saturating_sub(2));
        }

        let l = self.layout(app, r.page);
        for (tr, text, kind) in l.texts.iter().filter(|(r, _, _)| !r.is_empty()) {
            match kind {
                TextKind::Heading => form::heading(f, tr.x, tr.y, text, tr.w, ctx),
                TextKind::Label => form::label(f, tr.x, tr.y, text, tr.w, ctx),
                TextKind::Muted => form::muted(f, tr.x, tr.y, text, tr.w, ctx),
            }
        }
        if let Some(sr) = l.sample {
            sample::draw(f, sr, ctx);
        }
        for ctl in l.ctls.iter().filter(|c| !c.rect.is_empty()) {
            let focused = self.focus == Focus::Ctl(ctl.id);
            match &ctl.kind {
                Kind::Check(cb) => cb.draw(f, ctl.rect, ctx, focused),
                Kind::Combo { items, current, .. } => {
                    let text = items.get(*current).cloned().flatten().unwrap_or_default();
                    form::draw_combo(f, ctl.rect, &text, focused, ctx);
                }
                Kind::Spin(text) => form::draw_spin(f, ctl.rect, text, focused, ctx),
                Kind::Button(label) => {
                    let row = ButtonRow::new(&[label], 0);
                    row.draw(f, ctl.rect, ctx, focused);
                }
                Kind::Text => {
                    let mut input = self.shell.clone();
                    if let Some(shell) = app.try_state::<FontInfo>().and_then(|i| i.default_shell.as_deref()) {
                        input.placeholder = format!("{shell}   (your default)");
                    }
                    input.draw(f, ctl.rect, ctx, focused);
                }
            }
        }

        // The focused control's tooltip.
        if let Focus::Ctl(id) = self.focus {
            for (i, line) in wrap(id.hint(), r.hint.w as usize).iter().take(r.hint.h as usize).enumerate() {
                form::muted(f, r.hint.x, r.hint.y + i as u16, line, r.hint.w, ctx);
            }
        }
        Self::close_row().draw(f, r.buttons, ctx, self.focus == Focus::Close);

        if let Some((id, popup)) = &self.popup
            && let Some(Ctl { kind: Kind::Combo { items, .. }, .. }) = l.ctl(*id)
        {
            popup.draw(f, items, screen, ctx);
        }
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        let r = Self::rects(screen);
        let l = self.layout(app, r.page);
        if self.popup.is_some() {
            return self.popup_event(app, &l, ev, screen);
        }
        match ev {
            Event::Key(k) => self.key(app, &l, k),
            Event::Commit(_) | Event::Preedit(_) if self.focus == Focus::Ctl(Id::Shell) => {
                self.shell.handle_event(ev, app.clipboard.as_mut());
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => self.mouse(app, &l, &r, m),
            _ => ModalOutcome::Consumed,
        }
    }
}
