//! Progress of a copy/move with a Cancel button. The worker writes into a
//! shared [`ProgressState`]; the dialog only reads it when drawn.

use std::sync::{Arc, Mutex};

use oc_grid::{Frame, Style};
use oc_vfs::{Progress, human_size};

use super::{dialog_rects, draw_box};
use crate::app::App;
use crate::geom::Rect;
use crate::input::{Event, Key, MouseButton, MouseKind};
use crate::modal::{Modal, ModalOutcome};
use crate::tasks::CancelToken;
use crate::widgets::button::RowOutcome;
use crate::widgets::{ButtonRow, DrawCtx, put};

#[derive(Clone, Debug, Default)]
pub struct ProgressState {
    pub progress: Progress,
}

pub struct ProgressBox {
    pub title: String,
    pub state: Arc<Mutex<ProgressState>>,
    pub token: CancelToken,
    buttons: ButtonRow,
}

impl ProgressBox {
    pub fn new(title: &str, state: Arc<Mutex<ProgressState>>, token: CancelToken) -> Self {
        ProgressBox { title: title.into(), state, token, buttons: ButtonRow::new(&["Cancel"], 0) }
    }

    fn rects(&self, screen: Rect) -> (Rect, Rect) {
        let (outer, content) = dialog_rects(screen, 72, 5);
        (outer, Rect::new(content.x, content.bottom().saturating_sub(1), content.w, 1))
    }

    fn cancel(&mut self) -> ModalOutcome {
        self.token.cancel();
        ModalOutcome::Consumed
    }
}

/// Eighths of a cell for smooth bars.
const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

/// A horizontal bar `fraction` full.
pub fn draw_bar(f: &mut Frame, r: Rect, fraction: f64, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    let style = Style::new(c.border_active, c.background);
    let eighths = (fraction.clamp(0.0, 1.0) * r.w as f64 * 8.0).round() as u32;
    for i in 0..r.w {
        let cell = eighths.saturating_sub(i as u32 * 8).min(8);
        let ch = if cell == 8 { '█' } else { PARTIAL[cell as usize] };
        f.put_char(r.x + i, r.y, ch, style);
    }
}

impl Modal for ProgressBox {
    fn title(&self) -> &str {
        &self.title
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let (outer, buttons) = self.rects(screen);
        let content = draw_box(f, outer, &self.title, ctx);
        let p = self.state.lock().unwrap_or_else(|e| e.into_inner()).progress.clone();
        let current = if self.token.is_cancelled() {
            "Cancelling…".to_owned()
        } else if p.current.is_empty() {
            "Preparing…".to_owned()
        } else {
            p.current.clone()
        };
        let text = oc_grid::elide_left(&current, content.w as usize);
        put(f, content.x, content.y, &text, Style::new(c.foreground, c.surface), content.w);
        let fraction = if p.bytes_total > 0 { p.bytes_done as f64 / p.bytes_total as f64 } else { 0.0 };
        draw_bar(f, Rect::new(content.x, content.y + 1, content.w, 1), fraction, ctx);
        let counts = format!("{}/{} files · {} / {}", p.files_done, p.files_total, human_size(p.bytes_done), human_size(p.bytes_total));
        put(f, content.x, content.y + 2, &counts, Style::new(c.muted, c.surface), content.w);
        self.buttons.draw(f, buttons, ctx, true);
    }

    fn handle_event(&mut self, _app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        match ev {
            Event::Key(k) if k.key == Key::Escape => self.cancel(),
            Event::Key(k) => match self.buttons.handle_key(k, true) {
                RowOutcome::Activated(_) => self.cancel(),
                _ => ModalOutcome::Consumed,
            },
            Event::Mouse(m) if matches!(m.kind, MouseKind::Up(MouseButton::Left)) => {
                let (_, row) = self.rects(screen);
                if self.buttons.hit(row, m.col, m.row).is_some() {
                    return self.cancel();
                }
                ModalOutcome::Consumed
            }
            _ => ModalOutcome::Consumed,
        }
    }
}
