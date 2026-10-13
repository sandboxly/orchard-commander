//! Modal dialogs drawn as terminal-style boxes (port of `ui/dialogs.py`):
//! message boxes (confirm, errors, the File exists question), text input,
//! progress with Cancel, and a filterable picker (command palette,
//! locations, color schemes).
//!
//! Each dialog takes a callback that receives the answer with `&mut App`.

mod input;
mod message;
mod picker;
mod progress;

pub use input::InputBox;
pub use message::MessageBox;
pub use picker::{Picker, PickerItem, fuzzy_score};
pub use progress::{ProgressBox, ProgressState};

use oc_grid::Frame;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Severity};
use crate::geom::Rect;
use crate::tasks::Reply;
use crate::widgets::{BoxFrame, DrawCtx};

/// Wrap `text` to lines of at most `width` cells (breaking long words).
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut w = 0;
        for word in para.split_word_bounds() {
            let ww = word.width();
            if w + ww > width && !line.is_empty() {
                out.push(std::mem::take(&mut line).trim_end().to_owned());
                w = 0;
                if word.trim().is_empty() {
                    continue;
                }
            }
            if ww > width {
                for g in word.graphemes(true) {
                    let gw = g.width();
                    if w + gw > width {
                        out.push(std::mem::take(&mut line));
                        w = 0;
                    }
                    line.push_str(g);
                    w += gw;
                }
                continue;
            }
            line.push_str(word);
            w += ww;
        }
        out.push(line.trim_end().to_owned());
    }
    out
}

/// The box of a dialog `width` cells wide with `content_rows` inside,
/// centered on the screen, and the content area within it.
pub fn dialog_rects(screen: Rect, width: u16, content_rows: u16) -> (Rect, Rect) {
    let w = width.min(screen.w.saturating_sub(2)).max(10.min(screen.w));
    let h = (content_rows + 2).min(screen.h);
    let outer = screen.centered(w, h);
    (outer, Rect::new(outer.x + 2, outer.y + 1, outer.w.saturating_sub(4), outer.h.saturating_sub(2)))
}

/// Draw a dialog's box and return its content area.
pub fn draw_box(f: &mut Frame, outer: Rect, title: &str, ctx: &DrawCtx) -> Rect {
    crate::widgets::cover(f, outer, ctx);
    BoxFrame { title, subtitle: "", active: true }.draw(f, outer, ctx);
    Rect::new(outer.x + 2, outer.y + 1, outer.w.saturating_sub(4), outer.h.saturating_sub(2))
}

/// Ask a yes/no question; `on_ok` runs if the user agrees.
pub fn confirm(app: &mut App, title: &str, message: &str, ok_label: &str, on_ok: impl FnOnce(&mut App) + 'static) {
    let mb = MessageBox::new(title, message, &[ok_label, "Cancel"], 0).width(52).on_choice(move |app, choice| {
        if choice == Some(0) {
            on_ok(app);
        }
    });
    app.show(mb);
}

/// Ask for a line of text (prefilled with `value`, all selected). `on_done`
/// gets the trimmed text, or None for Cancel / empty input.
pub fn ask_text(
    app: &mut App,
    title: &str,
    message: &str,
    value: &str,
    ok_label: &str,
    on_done: impl FnOnce(&mut App, Option<String>) + 'static,
) {
    app.show(InputBox::new(title, message, value, ok_label, on_done));
}

/// An error or notice that needs acknowledging.
pub fn message(app: &mut App, title: &str, text: &str, severity: Severity) {
    let mb = MessageBox::new(title, text, &["OK"], 0).severity(severity);
    app.show(mb);
}

/// The File exists question, for a worker waiting in `ask_ui`. Answers
/// (choice, apply to all); Escape cancels.
pub fn ask_conflict(app: &mut App, src: &str, dst: &str, reply: Reply<(oc_vfs::Conflict, bool)>) {
    use oc_vfs::Conflict;
    const CHOICES: [(Conflict, bool); 5] = [
        (Conflict::Overwrite, false),
        (Conflict::Overwrite, true),
        (Conflict::Skip, false),
        (Conflict::Skip, true),
        (Conflict::Cancel, false),
    ];
    let text = format!("Overwrite\n  {dst}\nwith\n  {src}?");
    let mb = MessageBox::new("File exists", &text, &["&Overwrite", "&All", "&Skip", "S&kip all", "Cancel"], 2).width(72).on_choice(
        move |_, choice| {
            reply.send(choice.map_or((Conflict::Cancel, false), |i| CHOICES[i]));
        },
    );
    app.show(mb);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("a\nb", 10), ["a", "b"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), [""]);
    }
}
