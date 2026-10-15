//! "Add bookmark": title (an `&` sets the hotkey), the folder, where to put
//! it, and optionally the other panel's folder (port of `AddBookmarkDialog`).

use oc_config::Spec;
use orchard_grid::{Frame, Style};

use super::form::{Choice, cycle_focus};
use crate::app::App;
use crate::dialogs::{dialog_rects, draw_box, wrap};
use crate::geom::Rect;
use crate::input::{Event, Key, MouseButton, MouseKind};
use crate::modal::{Modal, ModalOutcome};
use crate::widgets::button::RowOutcome;
use crate::widgets::{ButtonRow, Checkbox, DrawCtx, TextInput, put};

pub const TIP: &str = "Tip: an & in the title sets its hotkey, e.g. “&Projects” → P";
const WIDTH: u16 = 70;
const LABEL_W: u16 = 8;

/// (title, folder id, also open the other panel)
pub type Answer = (String, Option<String>, bool);
type OnDone = Box<dyn FnOnce(&mut App, Option<Answer>)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Title = 0,
    PutIn = 1,
    Target = 2,
    Buttons = 3,
}

const FOCUS: [Focus; 4] = [Focus::Title, Focus::PutIn, Focus::Target, Focus::Buttons];

pub struct AddBookmark {
    pub title: TextInput,
    folder_text: String,
    folder: Choice,
    folder_ids: Vec<Option<String>>,
    with_target: Checkbox,
    has_target: bool,
    buttons: ButtonRow,
    focus: Focus,
    on_done: Option<OnDone>,
}

struct Rects {
    outer: Rect,
    title: Rect,
    folder: Rect,
    put_in: Rect,
    target: Rect,
    tip: Vec<String>,
    tip_at: Rect,
    buttons: Rect,
}

impl AddBookmark {
    pub fn new(
        location: Spec,
        target: Option<Spec>,
        choices: Vec<(String, Option<String>)>,
        on_done: impl FnOnce(&mut App, Option<Answer>) + 'static,
    ) -> Self {
        let label = match &target {
            // Fits the dialog: a long path is shortened at the front.
            Some(t) => format!("Also open the other panel at {}", orchard_grid::elide_left(&t.describe(), 33)),
            None => "Also open the other panel".to_owned(),
        };
        let (labels, ids): (Vec<String>, Vec<Option<String>>) = choices.into_iter().unzip();
        AddBookmark {
            title: TextInput::selected_all(&location.name()),
            folder_text: location.describe(),
            folder: Choice::new(labels),
            folder_ids: ids,
            // No "&" hotkey here: the label is the 1.x text (and may contain a path).
            with_target: Checkbox::new(&label.replace('&', "&&"), false),
            has_target: target.is_some(),
            buttons: ButtonRow::new(&["Add bookmark", "Cancel"], 0),
            focus: Focus::Title,
            on_done: Some(Box::new(on_done)),
        }
    }

    fn rects(&self, screen: Rect) -> Rects {
        let inner = WIDTH.min(screen.w.saturating_sub(2)).saturating_sub(4);
        let tip = wrap(TIP, inner.max(1) as usize);
        let rows = 4 + tip.len() as u16 + 2;
        let (outer, c) = dialog_rects(screen, WIDTH, rows);
        let field = |row: u16| Rect::new(c.x + LABEL_W, c.y + row, c.w.saturating_sub(LABEL_W), 1);
        Rects {
            outer,
            title: field(0),
            folder: field(1),
            put_in: field(2),
            target: Rect::new(c.x, c.y + 3, c.w, 1),
            tip_at: Rect::new(c.x, c.y + 4, c.w, tip.len() as u16),
            tip,
            buttons: Rect::new(c.x, c.bottom().saturating_sub(1), c.w, 1),
        }
    }

    fn finish(&mut self, app: &mut App, accept: bool) -> ModalOutcome {
        let answer = accept.then(|| {
            let title = self.title.text.trim();
            let title = if title.is_empty() { "Bookmark".to_owned() } else { title.to_owned() };
            let folder = self.folder_ids.get(self.folder.selected).cloned().flatten();
            (title, folder, self.has_target && self.with_target.checked)
        });
        if let Some(f) = self.on_done.take() {
            f(app, answer);
        }
        ModalOutcome::Close
    }

    fn focusable(&self, i: usize) -> bool {
        FOCUS[i] != Focus::Target || self.has_target
    }
}

impl Modal for AddBookmark {
    fn title(&self) -> &str {
        "Add bookmark"
    }

    fn text(&self) -> String {
        let place = self.folder.options.get(self.folder.selected).cloned().unwrap_or_default();
        format!("{}\n{}\n{}\n{}", self.title.text, self.folder_text, place, self.with_target.label)
    }

    fn wants_text_input(&self) -> bool {
        self.focus == Focus::Title
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let r = self.rects(screen);
        let content = draw_box(f, r.outer, "Add bookmark", ctx);
        let label = Style::new(c.foreground, c.surface);
        for (row, text) in [(r.title, "Title"), (r.folder, "Folder"), (r.put_in, "Put in")] {
            put(f, content.x, row.y, text, label, LABEL_W);
        }
        self.title.draw(f, r.title, ctx, self.focus == Focus::Title);
        put(f, r.folder.x, r.folder.y, &orchard_grid::elide_left(&self.folder_text, r.folder.w as usize), label, r.folder.w);
        self.folder.draw(f, r.put_in, ctx, self.focus == Focus::PutIn);
        if self.has_target {
            self.with_target.draw(f, r.target, ctx, self.focus == Focus::Target);
        } else {
            let muted = Style::new(c.muted, c.surface);
            put(f, r.target.x, r.target.y, &format!("[ ] {}", self.with_target.label), muted, r.target.w);
        }
        for (i, line) in r.tip.iter().enumerate() {
            put(f, r.tip_at.x, r.tip_at.y + i as u16, line, Style::new(c.muted, c.surface), r.tip_at.w);
        }
        self.buttons.draw(f, r.buttons, ctx, self.focus == Focus::Buttons);
        self.folder.draw_popup(f, screen, ctx);
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        let r = self.rects(screen);
        match ev {
            Event::Key(k) => {
                if self.folder.open.is_some() {
                    self.folder.handle_key(k, r.put_in);
                    return ModalOutcome::Consumed;
                }
                match k.key {
                    Key::Escape => return self.finish(app, false),
                    Key::Tab if k.mods.is_empty() || k.mods.shift => {
                        let i = cycle_focus(self.focus as usize, FOCUS.len(), k.mods.shift, |i| self.focusable(i));
                        self.focus = FOCUS[i];
                        return ModalOutcome::Consumed;
                    }
                    Key::Enter if self.focus != Focus::Buttons => return self.finish(app, true),
                    _ => {}
                }
                match self.focus {
                    Focus::Title => {
                        self.title.handle_key(k, app.clipboard.as_mut());
                    }
                    Focus::PutIn => {
                        self.folder.handle_key(k, r.put_in);
                    }
                    Focus::Target => {
                        self.with_target.handle_key(k, true);
                    }
                    Focus::Buttons => {
                        if let RowOutcome::Activated(i) = self.buttons.handle_key(k, true) {
                            return self.finish(app, i == 0);
                        }
                    }
                }
                ModalOutcome::Consumed
            }
            Event::Commit(_) | Event::Preedit(_) if self.focus == Focus::Title => {
                self.title.handle_event(ev, app.clipboard.as_mut());
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => {
                if self.folder.handle_mouse(m, r.put_in, screen) {
                    self.focus = Focus::PutIn;
                    return ModalOutcome::Consumed;
                }
                match m.kind {
                    MouseKind::Down(MouseButton::Left) if r.title.contains(m.col, m.row) => {
                        self.focus = Focus::Title;
                        self.title.click(r.title, m.col);
                    }
                    MouseKind::Down(MouseButton::Left) if self.has_target && r.target.contains(m.col, m.row) => {
                        self.focus = Focus::Target;
                        self.with_target.click(r.target, m.col, m.row);
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
