//! Drawing the editor window: the frame (path on top, status line below),
//! line numbers, the text with syntax colors, selection, the current line,
//! the cursor, the scrollbar and the find bar. Only visible lines are
//! touched, and of those only the visible columns.

use oc_grid::{Cursor, CursorShape, Flags, Frame, Rgba, Style};
use oc_preview::data::SyntaxRole;
use oc_preview::highlight::palette;

use super::buffer::{advance, char_width};
use super::window::{EditorWindow, Focus, REPLACE_ALL};
use crate::app::App;
use crate::geom::Rect;
use crate::widgets::{BoxFrame, DrawCtx, Scrollbar, fill, put, put_right};

pub fn draw(w: &EditorWindow, _app: &App, f: &mut Frame, ctx: &DrawCtx) {
    let screen = Rect::screen(f.cols(), f.rows());
    // BoxFrame elides titles to the frame width minus 8 cells; keep the ● mark.
    let title = w.frame_title_within((screen.w as usize).saturating_sub(8));
    let status = w.status();
    BoxFrame { title: &title, subtitle: &status, active: true }.draw(f, screen, ctx);
    let l = w.layout(screen);
    draw_text(w, f, &l, ctx);
    if let Some(bar) = l.bar {
        draw_bar(w, f, bar, ctx);
    }
}

fn role_styles(ctx: &DrawCtx) -> [Option<(Rgba, Flags)>; SyntaxRole::COUNT] {
    let p = palette(&ctx.theme.scheme.pygments_style);
    let mut out = [None; SyntaxRole::COUNT];
    for role in SyntaxRole::ALL {
        let rs = p.style(role);
        let mut flags = Flags::NONE;
        if rs.bold {
            flags = flags.union(Flags::BOLD);
        }
        if rs.italic {
            flags = flags.union(Flags::ITALIC);
        }
        out[role as usize] =
            rs.color.map(|c| (Rgba::rgb(c.r, c.g, c.b), flags)).or((flags != Flags::NONE).then_some((ctx.theme.colors.foreground, flags)));
    }
    out
}

fn draw_text(w: &EditorWindow, f: &mut Frame, l: &super::window::Layout, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    let buf = &w.buf;
    let roles = role_styles(ctx);
    let sel = buf.selection();
    let cursor_line = buf.line_of(buf.cursor);
    let lines = buf.len_lines();
    let code = l.code;
    let right_edge = Rect::new(code.right(), code.y, 1, code.h);
    fill(f, l.gutter, c.surface);
    fill(f, right_edge, c.surface);
    let width = code.w as usize;
    for row in 0..code.h {
        let y = code.y + row;
        let line = w.top + row as usize;
        let current = line == cursor_line && w.loaded;
        let bg = if current { c.cursor_inactive_bg } else { c.surface };
        fill(f, Rect::new(code.x, y, code.w, 1), bg);
        if line >= lines {
            continue;
        }
        // Line number, right-aligned, one blank column before the text.
        let num = (line + 1).to_string();
        let fg = if current { c.foreground } else { c.muted };
        put_right(f, l.gutter.right().saturating_sub(1), y, &num, Style::new(fg, c.surface), l.gutter.x);

        let start = buf.line_start(line);
        let spans = w.hl.spans(line);
        let mut span = 0;
        let (mut col, mut byte) = (0usize, 0usize);
        let mut pos = start;
        let end_col = w.left + width;
        for ch in buf.line(line).chars() {
            if col >= end_col {
                break;
            }
            let next = advance(col, ch);
            if next > w.left {
                while span < spans.len() && spans[span].end as usize <= byte {
                    span += 1;
                }
                let role = spans.get(span).filter(|s| s.start as usize <= byte).map(|s| s.role);
                let selected = sel.as_ref().is_some_and(|r| r.contains(&pos));
                let style = if selected {
                    Style::new(c.cursor_fg, c.cursor_bg)
                } else {
                    match role.and_then(|r| roles[r as usize]) {
                        Some((fg, flags)) => Style::new(fg, bg).with(flags),
                        None => Style::new(c.foreground, bg),
                    }
                };
                let x = code.x + (col.max(w.left) - w.left) as u16;
                let visible_end = next.min(end_col);
                if ch == '\t' || col < w.left || (char_width(ch) > 1 && next > end_col) {
                    // Tabs, and wide chars cut by the view's edge, as blanks.
                    let n = (visible_end - col.max(w.left)) as u16;
                    fill(f, Rect::new(x, y, n, 1), style.bg);
                } else if ch.is_control() {
                    f.put_char(x, y, '\u{fffd}', Style { fg: c.muted, ..style });
                } else {
                    f.put_str_clipped(x, y, ch.encode_utf8(&mut [0; 4]), style, code.right());
                }
            }
            col = next;
            byte += ch.len_utf8();
            pos += 1;
        }
        // A selected line break shows as one selected cell after the text.
        if col < end_col && col >= w.left && line + 1 < lines && sel.as_ref().is_some_and(|r| r.contains(&pos)) {
            let x = code.x + (col - w.left) as u16;
            fill(f, Rect::new(x, y, 1, 1), c.cursor_bg);
        }
    }
    // The cursor (and text an input method is composing).
    if w.focus == Focus::Text && !w.read_only && w.loaded && ctx.window_focused {
        let line = cursor_line;
        if line >= w.top && line < w.top + code.h as usize {
            let col = buf.display_col(buf.cursor);
            if col >= w.left && col < w.left + width {
                let x = code.x + (col - w.left) as u16;
                let y = code.y + (line - w.top) as u16;
                let mut cx = x;
                if !w.preedit.is_empty() {
                    cx = f.put_str_clipped(
                        x,
                        y,
                        &w.preedit,
                        Style::new(c.foreground, c.cursor_inactive_bg).with(Flags::UNDERLINE),
                        code.right(),
                    );
                }
                f.cursor = Some(Cursor { col: cx.min(code.right()), row: y, shape: CursorShape::Beam, color: c.foreground, text: None });
            }
        }
    }
    Scrollbar { total: lines, visible: code.h as usize, top: w.top }.draw(f, Rect::new(code.x, code.y, code.w + 1, code.h), ctx);
}

fn draw_bar(w: &EditorWindow, f: &mut Frame, bar: Rect, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    fill(f, bar, c.surface);
    let bl = w.bar_layout(bar);
    let focused = ctx.window_focused;
    w.bar.find.draw(f, bl.find, ctx, focused && w.focus == Focus::Find);
    if let Some(r) = bl.with {
        w.bar.with.draw(f, r, ctx, focused && w.focus == Focus::Replace);
    }
    if let Some(r) = bl.replace_all {
        put(f, r.x, r.y, REPLACE_ALL, Style::new(c.foreground, c.border), r.w);
    }
    w.bar.match_case.draw(f, bl.match_case, ctx, w.focus == Focus::MatchCase);
    put(f, bl.status.x, bl.status.y, &w.bar.status, Style::new(c.muted, c.surface), bl.status.w);
}
