//! A message in place of a preview ("No file selected", errors, "Downloading
//! … from the cloud"), optionally with a button ("Download and preview",
//! "Try again") — the message page of 1.x's `PreviewPane`.

use std::any::Any;
use std::time::Instant;

use orchard_grid::{Frame, Style};

use super::views::{ContentView, Handle};
use crate::app::{App, Outcome};
use crate::geom::Rect;
use crate::input::{Event, Key, MouseButton, MouseKind};
use crate::widgets::{ButtonRow, DrawCtx, put, width};

pub struct MessageView {
    pub message: String,
    pub button: Option<ButtonRow>,
    /// Waiting since (a download): "N s" is added from 2 s on.
    pub busy_since: Option<Instant>,
    /// The button was pressed (the pane acts on it).
    pub clicked: bool,
}

impl MessageView {
    pub fn new(message: impl Into<String>, button: Option<&str>) -> Self {
        MessageView { message: message.into(), button: button.map(|b| ButtonRow::new(&[b], 0)), busy_since: None, clicked: false }
    }

    /// A message for something that is being waited for.
    pub fn busy(message: impl Into<String>) -> Self {
        MessageView { busy_since: Some(Instant::now()), ..MessageView::new(message, None) }
    }

    /// The text with the seconds waited (like 1.x's ticker).
    pub fn shown_text(&self) -> String {
        match self.busy_since.map(|t| t.elapsed().as_secs()) {
            Some(s) if s >= 2 => format!("{}\n{s} s", self.message),
            _ => self.message.clone(),
        }
    }

    /// The text lines and the button row, centered in `area`.
    fn layout(&self, area: Rect) -> (Vec<String>, Rect, Option<Rect>) {
        let lines = crate::dialogs::wrap(&self.shown_text(), area.w.saturating_sub(4).max(1) as usize);
        let extra = if self.button.is_some() { 2 } else { 0 };
        let h = (lines.len() as u16 + extra).min(area.h);
        let block = area.centered(area.w, h);
        let button = self.button.as_ref().map(|_| Rect::new(area.x, block.bottom().saturating_sub(1), area.w, 1));
        (lines, block, button)
    }
}

impl ContentView for MessageView {
    fn kind(&self) -> &'static str {
        "message"
    }

    fn draw(&self, _app: &App, f: &mut Frame, area: Rect, ctx: &DrawCtx, focused: bool) {
        let (lines, block, button) = self.layout(area);
        let style = Style::new(ctx.theme.colors.muted, ctx.theme.colors.surface);
        for (i, line) in lines.iter().enumerate().take(block.h as usize) {
            let w = width(line).min(area.w);
            put(f, area.x + (area.w - w) / 2, block.y + i as u16, line, style, area.w);
        }
        if let (Some(row), Some(b)) = (button, &self.button) {
            b.draw(f, row, ctx, focused);
        }
    }

    fn handle_event(&mut self, _app: &mut App, _h: Handle, ev: &Event, area: Rect) -> Outcome {
        let (_, _, row) = self.layout(area);
        let (Some(row), Some(b)) = (row, &self.button) else {
            return Outcome::Ignored;
        };
        match ev {
            Event::Mouse(m) if m.kind == MouseKind::Down(MouseButton::Left) && b.hit(row, m.col, m.row).is_some() => {
                self.clicked = true;
                Outcome::Consumed
            }
            Event::Key(k) if matches!(k.key, Key::Enter | Key::Space) && k.mods.is_empty() => {
                self.clicked = true;
                Outcome::Consumed
            }
            _ => Outcome::Ignored,
        }
    }

    fn text(&self) -> Option<String> {
        Some(self.shown_text())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
