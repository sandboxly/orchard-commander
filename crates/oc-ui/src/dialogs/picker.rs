//! A filterable list: type to filter (every word), ↑/↓ to move, Enter to choose
//! (command palette, location chooser, color schemes).

use orchard_grid::{Frame, Style};

use super::{dialog_rects, draw_box};
use crate::app::App;
use crate::geom::Rect;
use crate::input::{Event, Key, MouseButton, MouseKind};
use crate::modal::{Modal, ModalOutcome};
use crate::widgets::{DrawCtx, ListState, TextInput, fill, put_elided};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerItem {
    pub label: String,
    /// Shown dimmed after the label (a shortcut, a path).
    pub hint: String,
}

impl PickerItem {
    pub fn new(label: impl Into<String>, hint: impl Into<String>) -> Self {
        PickerItem { label: label.into(), hint: hint.into() }
    }
}

/// Whether an item shows for the filter `query`: every word of it appears
/// in the label or the hint, ignoring case (1.x's `Picker._apply_filter`).
pub fn matches_filter(item: &PickerItem, query: &str) -> bool {
    let hay = format!("{} {}", item.label, item.hint).to_lowercase();
    query.to_lowercase().split_whitespace().all(|word| hay.contains(word))
}

type OnChoose = Box<dyn FnOnce(&mut App, Option<usize>)>;
type OnHighlight = Box<dyn FnMut(&mut App, usize)>;

pub struct Picker {
    pub title: String,
    pub items: Vec<PickerItem>,
    pub filter: TextInput,
    /// Indices into `items` that match the filter.
    pub shown: Vec<usize>,
    pub list: ListState,
    width: u16,
    on_choose: Option<OnChoose>,
    on_highlight: Option<OnHighlight>,
    label_w: u16,
}

impl Picker {
    pub fn new(title: &str, items: Vec<PickerItem>, on_choose: impl FnOnce(&mut App, Option<usize>) + 'static) -> Self {
        let label_w = items.iter().map(|i| crate::widgets::width(&i.label)).max().unwrap_or(0).clamp(10, 40);
        let mut p = Picker {
            title: title.into(),
            shown: (0..items.len()).collect(),
            list: ListState::new(items.len()),
            items,
            filter: TextInput { placeholder: "Type to filter…".into(), ..Default::default() },
            width: 64,
            on_choose: Some(Box::new(on_choose)),
            on_highlight: None,
            label_w,
        };
        p.list.set_len(p.shown.len());
        p
    }

    pub fn width(mut self, cells: u16) -> Self {
        self.width = cells;
        self
    }

    /// Start on item `i` (e.g. the current color scheme).
    pub fn current(mut self, i: usize) -> Self {
        if let Some(pos) = self.shown.iter().position(|&s| s == i) {
            self.list.select(pos, 16);
        }
        self
    }

    /// Called whenever the highlighted item changes (live previews).
    pub fn on_highlight(mut self, f: impl FnMut(&mut App, usize) + 'static) -> Self {
        self.on_highlight = Some(Box::new(f));
        self
    }

    pub fn list_rows(&self) -> u16 {
        (self.items.len() as u16).clamp(4, 16)
    }

    fn rects(&self, screen: Rect) -> (Rect, Rect, Rect) {
        let (outer, content) = dialog_rects(screen, self.width, self.list_rows() + 1);
        let filter = Rect::new(content.x, content.y, content.w, 1);
        let list = Rect::new(content.x, content.y + 1, content.w, content.h.saturating_sub(1));
        (outer, filter, list)
    }

    fn visible(&self) -> usize {
        self.list_rows() as usize
    }

    /// Recompute `shown` for the filter text (in list order), keeping the
    /// highlighted item if it still matches, else the first match.
    pub fn apply_filter(&mut self) {
        let current = self.highlighted();
        let q = self.filter.text.clone();
        self.shown = (0..self.items.len()).filter(|&i| matches_filter(&self.items[i], &q)).collect();
        self.list.set_len(self.shown.len());
        let pos = current.and_then(|c| self.shown.iter().position(|&s| s == c));
        self.list.select(pos.unwrap_or(0), self.visible());
        self.list.top = self.list.top.min(self.list.cursor);
    }

    /// The highlighted item's index in `items`.
    pub fn highlighted(&self) -> Option<usize> {
        self.shown.get(self.list.cursor).copied()
    }

    fn highlight_changed(&mut self, app: &mut App, before: Option<usize>) {
        let now = self.highlighted();
        if now != before
            && let (Some(i), Some(f)) = (now, self.on_highlight.as_mut())
        {
            f(app, i);
        }
    }

    fn choose(&mut self, app: &mut App, accept: bool) -> ModalOutcome {
        let choice = if accept { self.highlighted() } else { None };
        if let Some(f) = self.on_choose.take() {
            f(app, choice);
        }
        ModalOutcome::Close
    }
}

impl Modal for Picker {
    fn title(&self) -> &str {
        &self.title
    }

    fn text(&self) -> String {
        self.shown.iter().map(|&i| self.items[i].label.as_str()).collect::<Vec<_>>().join("\n")
    }

    fn wants_text_input(&self) -> bool {
        true
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let (outer, filter, list) = self.rects(screen);
        draw_box(f, outer, &self.title, ctx);
        self.filter.draw(f, filter, ctx, true);
        let label_w = self.label_w.min(list.w / 2);
        self.list.draw(f, list, ctx, |f, r, i, cursor| {
            let item = &self.items[self.shown[i]];
            let (style, hint) = if cursor {
                (Style::new(c.cursor_fg, c.cursor_bg), Style::new(c.cursor_fg, c.cursor_bg))
            } else {
                (Style::new(c.foreground, c.surface), Style::new(c.muted, c.surface))
            };
            fill(f, r, style.bg);
            put_elided(f, r.x + 1, r.y, &item.label, style, label_w);
            if !item.hint.is_empty() {
                let x = r.x + 2 + label_w;
                put_elided(f, x, r.y, &item.hint, hint, r.right().saturating_sub(x + 1));
            }
        });
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        let before = self.highlighted();
        let visible = self.visible();
        let outcome = match ev {
            Event::Key(k) => match k.key {
                Key::Escape => return self.choose(app, false),
                Key::Enter => return self.choose(app, true),
                // With any modifiers, and pages of 10 entries (1.x).
                Key::Up | Key::Down | Key::PageUp | Key::PageDown => {
                    let step = match k.key {
                        Key::Up => -1,
                        Key::Down => 1,
                        Key::PageUp => -10,
                        _ => 10,
                    };
                    self.list.move_by(step, visible);
                    ModalOutcome::Consumed
                }
                _ => {
                    let old = self.filter.text.clone();
                    self.filter.handle_key(k, app.clipboard.as_mut());
                    if self.filter.text != old {
                        self.apply_filter();
                    }
                    ModalOutcome::Consumed
                }
            },
            Event::Commit(_) | Event::Preedit(_) => {
                self.filter.handle_event(ev, app.clipboard.as_mut());
                self.apply_filter();
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => {
                let (_, _, list) = self.rects(screen);
                match m.kind {
                    MouseKind::Scroll(lines) => self.list.scroll_by((-lines * 3.0).round() as isize, visible),
                    MouseKind::Down(MouseButton::Left) => {
                        if let Some(i) = self.list.hit(list, m.col, m.row) {
                            self.list.cursor = i;
                        }
                    }
                    MouseKind::Up(MouseButton::Left) if self.list.hit(list, m.col, m.row).is_some() => {
                        return self.choose(app, true);
                    }
                    _ => {}
                }
                ModalOutcome::Consumed
            }
            _ => ModalOutcome::Consumed,
        };
        self.highlight_changed(app, before);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_word_as_a_substring() {
        let item = PickerItem::new("Toggle hidden files", "⌘⇧.");
        assert!(matches_filter(&item, "hid"));
        assert!(matches_filter(&item, "FILES toggle"));
        assert!(matches_filter(&item, "⌘⇧"), "the hint counts too");
        assert!(matches_filter(&item, ""));
        assert!(!matches_filter(&item, "tgl"), "no fuzzy subsequences");
        assert!(!matches_filter(&item, "hidden xyz"));
    }
}
