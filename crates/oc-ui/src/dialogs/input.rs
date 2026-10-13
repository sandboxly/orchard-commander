//! Ask for one line of text (Copy to, Make directory, Go to…).

use oc_grid::{Frame, Style};

use super::{dialog_rects, draw_box, wrap};
use crate::app::App;
use crate::geom::Rect;
use crate::input::{Event, Key, MouseButton, MouseKind};
use crate::modal::{Modal, ModalOutcome};
use crate::widgets::button::RowOutcome;
use crate::widgets::{ButtonRow, DrawCtx, TextInput, put};

type OnDone = Box<dyn FnOnce(&mut App, Option<String>)>;

pub struct InputBox {
    pub title: String,
    pub message: String,
    pub input: TextInput,
    pub buttons: ButtonRow,
    /// Focus is on the buttons rather than the text field.
    pub buttons_focused: bool,
    width: u16,
    on_done: Option<OnDone>,
}

impl InputBox {
    pub fn new(title: &str, message: &str, value: &str, ok_label: &str, on_done: impl FnOnce(&mut App, Option<String>) + 'static) -> Self {
        InputBox {
            title: title.into(),
            message: message.into(),
            input: TextInput::selected_all(value),
            buttons: ButtonRow::new(&[ok_label, "Cancel"], 0),
            buttons_focused: false,
            width: 64,
            on_done: Some(Box::new(on_done)),
        }
    }

    pub fn width(mut self, cells: u16) -> Self {
        self.width = cells;
        self
    }

    fn rects(&self, screen: Rect) -> (Rect, Vec<String>, Rect, Rect) {
        let inner_w = self.width.min(screen.w.saturating_sub(2)).saturating_sub(4) as usize;
        let lines = wrap(&self.message, inner_w.max(1));
        let rows = lines.len() as u16 + 3;
        let (outer, content) = dialog_rects(screen, self.width, rows);
        let input = Rect::new(content.x, content.y + lines.len() as u16, content.w, 1);
        let buttons = Rect::new(content.x, content.bottom().saturating_sub(1), content.w, 1);
        (outer, lines, input, buttons)
    }

    fn finish(&mut self, app: &mut App, accept: bool) -> ModalOutcome {
        let text = self.input.text.trim().to_owned();
        let value = (accept && !text.is_empty()).then_some(text);
        if let Some(f) = self.on_done.take() {
            f(app, value);
        }
        ModalOutcome::Close
    }
}

impl Modal for InputBox {
    fn title(&self) -> &str {
        &self.title
    }

    fn text(&self) -> String {
        format!("{}\n{}", self.message, self.input.text)
    }

    fn wants_text_input(&self) -> bool {
        !self.buttons_focused
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let (outer, lines, input, buttons) = self.rects(screen);
        let content = draw_box(f, outer, &self.title, ctx);
        for (i, line) in lines.iter().enumerate() {
            put(f, content.x, content.y + i as u16, line, Style::new(c.foreground, c.surface), content.w);
        }
        self.input.draw(f, input, ctx, !self.buttons_focused);
        self.buttons.draw(f, buttons, ctx, self.buttons_focused);
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        match ev {
            Event::Key(k) => {
                match k.key {
                    Key::Escape => return self.finish(app, false),
                    Key::Tab => {
                        self.buttons_focused = !self.buttons_focused;
                        return ModalOutcome::Consumed;
                    }
                    Key::Enter if !self.buttons_focused => return self.finish(app, true),
                    _ => {}
                }
                if self.buttons_focused {
                    return match self.buttons.handle_key(k, true) {
                        RowOutcome::Activated(i) => self.finish(app, i == 0),
                        _ => ModalOutcome::Consumed,
                    };
                }
                if let RowOutcome::Activated(i) = self.buttons.handle_key(k, false) {
                    return self.finish(app, i == 0);
                }
                self.input.handle_key(k, app.clipboard.as_mut());
                ModalOutcome::Consumed
            }
            Event::Commit(_) | Event::Preedit(_) => {
                self.input.handle_event(ev, app.clipboard.as_mut());
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => {
                let (_, _, input, buttons) = self.rects(screen);
                match m.kind {
                    MouseKind::Down(MouseButton::Left) if input.contains(m.col, m.row) => {
                        self.buttons_focused = false;
                        self.input.click(input, m.col);
                    }
                    MouseKind::Up(MouseButton::Left) => {
                        if let Some(i) = self.buttons.hit(buttons, m.col, m.row) {
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
