//! A message with buttons: confirmations, errors, the File exists question.

use oc_grid::{Frame, Style};

use super::{dialog_rects, draw_box, wrap};
use crate::app::{App, Severity};
use crate::geom::Rect;
use crate::input::{Event, Key, MouseKind};
use crate::modal::{Modal, ModalOutcome};
use crate::widgets::button::RowOutcome;
use crate::widgets::{ButtonRow, DrawCtx, put};

type OnChoice = Box<dyn FnOnce(&mut App, Option<usize>)>;

pub struct MessageBox {
    pub title: String,
    pub text: String,
    pub buttons: ButtonRow,
    pub severity: Severity,
    width: u16,
    on_choice: Option<OnChoice>,
}

impl MessageBox {
    /// Buttons may mark hotkeys with `&`; `focus` is the button Enter presses.
    pub fn new(title: &str, text: &str, buttons: &[&str], focus: usize) -> Self {
        MessageBox {
            title: title.into(),
            text: text.into(),
            buttons: ButtonRow::new(buttons, focus),
            severity: Severity::Info,
            width: 64,
            on_choice: None,
        }
    }

    pub fn width(mut self, cells: u16) -> Self {
        self.width = cells;
        self
    }

    pub fn severity(mut self, s: Severity) -> Self {
        self.severity = s;
        self
    }

    /// Called with the pressed button's index, or None for Escape.
    pub fn on_choice(mut self, f: impl FnOnce(&mut App, Option<usize>) + 'static) -> Self {
        self.on_choice = Some(Box::new(f));
        self
    }

    fn rects(&self, screen: Rect) -> (Rect, Rect, Vec<String>, Rect) {
        let inner_w = self.width.min(screen.w.saturating_sub(2)).saturating_sub(4) as usize;
        let lines = wrap(&self.text, inner_w.max(1));
        let rows = lines.len() as u16 + 2;
        let (outer, content) = dialog_rects(screen, self.width.max(self.buttons.cells() + 6), rows);
        let buttons = Rect::new(content.x, content.bottom().saturating_sub(1), content.w, 1);
        (outer, content, lines, buttons)
    }

    fn finish(&mut self, app: &mut App, choice: Option<usize>) -> ModalOutcome {
        if let Some(f) = self.on_choice.take() {
            f(app, choice);
        }
        ModalOutcome::Close
    }
}

impl Modal for MessageBox {
    fn title(&self) -> &str {
        &self.title
    }

    fn text(&self) -> String {
        self.text.clone()
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let (outer, _, lines, buttons) = self.rects(screen);
        let content = draw_box(f, outer, &self.title, ctx);
        let fg = match self.severity {
            Severity::Error => c.error,
            Severity::Warning => c.warning,
            Severity::Info => c.foreground,
        };
        for (i, line) in lines.iter().enumerate() {
            put(f, content.x, content.y + i as u16, line, Style::new(fg, c.surface), content.w);
        }
        self.buttons.draw(f, buttons, ctx, true);
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        match ev {
            Event::Key(k) if k.key == Key::Escape => self.finish(app, None),
            Event::Key(k) => match self.buttons.handle_key(k, true) {
                RowOutcome::Activated(i) => self.finish(app, Some(i)),
                RowOutcome::Consumed => ModalOutcome::Consumed,
                RowOutcome::Ignored if k.key == Key::Tab => {
                    self.buttons.handle_key(&crate::input::KeyEvent::plain(Key::Right), true);
                    ModalOutcome::Consumed
                }
                RowOutcome::Ignored => ModalOutcome::Consumed,
            },
            Event::Mouse(m) if matches!(m.kind, MouseKind::Up(crate::input::MouseButton::Left)) => {
                let (_, _, _, row) = self.rects(screen);
                match self.buttons.hit(row, m.col, m.row) {
                    Some(i) => self.finish(app, Some(i)),
                    None => ModalOutcome::Consumed,
                }
            }
            _ => ModalOutcome::Consumed,
        }
    }
}
