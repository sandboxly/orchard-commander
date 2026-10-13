//! "Organize bookmarks": the bookmark tree with Title /
//! Folder / Other panel columns; reorder by dragging (into folders, too) or
//! ▲ ▼, add folders and separators, edit, delete. Changes apply only with
//! OK (port of `BookmarkOrganizer` and `EditBookmarkDialog`).
//!
//! Keys besides the buttons: ↑/↓ move, ←/→ collapse / expand a folder,
//! ⌥↑ / ⌥↓ move the entry, F2 edit, F7 new folder, Delete delete, Tab
//! moves between the tree and the button rows.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use oc_config::Spec;
use oc_config::bookmarks::{Bookmark, Folder, Item};
use oc_grid::{Flags, Frame, Style};

use super::form::cycle_focus;
use super::locations::parse_location;
use super::menu_text;
use crate::app::App;
use crate::dialogs::{self, dialog_rects, draw_box};
use crate::geom::Rect;
use crate::input::{Event, Key, KeyEvent, MouseButton, MouseKind};
use crate::modal::{Modal, ModalOutcome};
use crate::widgets::button::RowOutcome;
use crate::widgets::{ButtonRow, DrawCtx, ListState, Scrollbar, TextInput, fill, put, put_elided};

const WIDTH: u16 = 96;
/// Tree rows (Python: at least 16 lines).
const TREE_ROWS: u16 = 16;
pub const HINT: &str = "Drag entries to reorder them or to move them into folders.";
const ACTIONS: [&str; 6] = ["New folder", "Separator", "Edit…", "Delete", "▲", "▼"];

// ---------------------------------------------------------------------------
// The tree being edited
// ---------------------------------------------------------------------------

/// A visible row: where its item is (indices from the top level down).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    path: Vec<usize>,
}

/// The organizer's working copy; shared with the dialogs it opens.
#[derive(Default)]
pub struct Tree {
    pub items: Vec<Item>,
    /// Folders shown closed (by id); all are open at first, like `expandAll`.
    collapsed: HashSet<String>,
    rows: Vec<Row>,
    pub list: ListState,
    visible: usize,
}

fn item_at<'a>(items: &'a [Item], path: &[usize]) -> &'a Item {
    let (first, rest) = path.split_first().expect("a path");
    match rest {
        [] => &items[*first],
        _ => match &items[*first] {
            Item::Folder(f) => item_at(&f.items, rest),
            _ => unreachable!("paths go through folders"),
        },
    }
}

/// The list holding the item at `path` (its parent's children).
fn list_of<'a>(items: &'a mut Vec<Item>, path: &[usize]) -> &'a mut Vec<Item> {
    match path {
        [] | [_] => items,
        [first, rest @ ..] => match &mut items[*first] {
            Item::Folder(f) => list_of(&mut f.items, rest),
            _ => unreachable!("paths go through folders"),
        },
    }
}

fn path_of(items: &[Item], id: &str) -> Option<Vec<usize>> {
    for (i, item) in items.iter().enumerate() {
        if item.id() == id {
            return Some(vec![i]);
        }
        if let Item::Folder(f) = item
            && let Some(mut p) = path_of(&f.items, id)
        {
            p.insert(0, i);
            return Some(p);
        }
    }
    None
}

impl Tree {
    pub fn new(items: Vec<Item>) -> Self {
        let mut t = Tree { items, visible: TREE_ROWS as usize, ..Default::default() };
        t.rebuild();
        t
    }

    fn rebuild(&mut self) {
        fn walk(items: &[Item], prefix: &mut Vec<usize>, collapsed: &HashSet<String>, out: &mut Vec<Row>) {
            for (i, item) in items.iter().enumerate() {
                prefix.push(i);
                out.push(Row { path: prefix.clone() });
                if let Item::Folder(f) = item
                    && !collapsed.contains(&f.id)
                {
                    walk(&f.items, prefix, collapsed, out);
                }
                prefix.pop();
            }
        }
        let mut rows = Vec::new();
        walk(&self.items, &mut Vec::new(), &self.collapsed, &mut rows);
        self.rows = rows;
        self.list.set_len(self.rows.len());
        self.list.ensure_visible(self.visible);
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn row_item(&self, row: usize) -> Option<&Item> {
        self.rows.get(row).map(|r| item_at(&self.items, &r.path))
    }

    /// The item under the cursor.
    pub fn current(&self) -> Option<&Item> {
        self.row_item(self.list.cursor)
    }

    fn select_id(&mut self, id: &str) {
        if let Some(row) = self.rows.iter().position(|r| item_at(&self.items, &r.path).id() == id) {
            self.list.select(row, self.visible);
        }
    }

    /// Put the cursor on the row of `title`'s item (tests).
    pub fn select_title(&mut self, title: &str) -> bool {
        let found = self.rows.iter().position(|r| match item_at(&self.items, &r.path) {
            Item::Bookmark(b) => b.title == title,
            Item::Folder(f) => f.title == title,
            Item::Separator { .. } => false,
        });
        if let Some(row) = found {
            self.list.select(row, self.visible);
        }
        found.is_some()
    }

    /// Insert after the cursor's entry, or into it if it is a folder (`_insert`).
    pub fn insert(&mut self, item: Item) {
        let id = item.id().to_owned();
        match self.rows.get(self.list.cursor).map(|r| r.path.clone()) {
            Some(path) => {
                if let Item::Folder(f) = item_at(&self.items, &path) {
                    let fid = f.id.clone();
                    if let Item::Folder(f) = &mut list_of(&mut self.items, &path)[*path.last().expect("a path")] {
                        f.items.push(item);
                    }
                    self.collapsed.remove(&fid);
                } else {
                    let at = path.last().expect("a path") + 1;
                    list_of(&mut self.items, &path).insert(at, item);
                }
            }
            None => self.items.push(item),
        }
        self.rebuild();
        self.select_id(&id);
    }

    pub fn delete(&mut self) {
        let Some(path) = self.rows.get(self.list.cursor).map(|r| r.path.clone()) else { return };
        list_of(&mut self.items, &path).remove(*path.last().expect("a path"));
        self.rebuild();
    }

    /// ▲ / ▼: swap with the neighbour in the same folder.
    pub fn move_by(&mut self, delta: isize) {
        let Some(path) = self.rows.get(self.list.cursor).map(|r| r.path.clone()) else { return };
        let i = *path.last().expect("a path");
        let siblings = list_of(&mut self.items, &path);
        let new = i as isize + delta;
        if new < 0 || new as usize >= siblings.len() {
            return;
        }
        let id = siblings[i].id().to_owned();
        let item = siblings.remove(i);
        siblings.insert(new as usize, item);
        self.rebuild();
        self.select_id(&id);
    }

    /// Replace the item with `item`'s id.
    pub fn replace(&mut self, item: Item) {
        if let Some(path) = path_of(&self.items, item.id()) {
            let i = *path.last().expect("a path");
            list_of(&mut self.items, &path)[i] = item;
            self.rebuild();
        }
    }

    /// Drag and drop: row `from` onto row `to` — into it if it is a folder,
    /// before it otherwise. False if not possible (onto itself or inside).
    pub fn drop_onto(&mut self, from: usize, to: usize) -> bool {
        let (Some(f), Some(t)) = (self.rows.get(from), self.rows.get(to)) else { return false };
        let (fp, tp) = (f.path.clone(), t.path.clone());
        if tp.starts_with(&fp) {
            return false;
        }
        let target = item_at(&self.items, &tp);
        let (target_id, into) = (target.id().to_owned(), matches!(target, Item::Folder(_)));
        let moving = list_of(&mut self.items, &fp).remove(*fp.last().expect("a path"));
        let id = moving.id().to_owned();
        let tp = path_of(&self.items, &target_id).expect("the target is still there");
        let i = *tp.last().expect("a path");
        let list = list_of(&mut self.items, &tp);
        if into {
            if let Item::Folder(folder) = &mut list[i] {
                folder.items.push(moving);
            }
            self.collapsed.remove(&target_id);
        } else {
            list.insert(i, moving);
        }
        self.rebuild();
        self.select_id(&id);
        true
    }

    /// ← / →: close or open the folder under the cursor (← on an entry goes to its folder).
    fn collapse(&mut self, open: bool) {
        let Some(row) = self.rows.get(self.list.cursor).cloned() else { return };
        match item_at(&self.items, &row.path) {
            Item::Folder(f) if open == self.collapsed.contains(&f.id) => {
                let id = f.id.clone();
                if open {
                    self.collapsed.remove(&id);
                } else {
                    self.collapsed.insert(id.clone());
                }
                self.rebuild();
                self.select_id(&id);
            }
            _ if !open && row.path.len() > 1 => {
                let parent = &row.path[..row.path.len() - 1];
                let id = item_at(&self.items, parent).id().to_owned();
                self.select_id(&id);
            }
            _ => {}
        }
    }

    fn toggle(&mut self, row: usize) {
        if let Some(Item::Folder(f)) = self.row_item(row) {
            let id = f.id.clone();
            if !self.collapsed.remove(&id) {
                self.collapsed.insert(id.clone());
            }
            self.rebuild();
            self.select_id(&id);
        }
    }

    /// The text of the three columns for row `row`.
    fn columns(&self, row: usize) -> (String, String, String) {
        let r = &self.rows[row];
        let indent = "  ".repeat(r.path.len() - 1);
        match item_at(&self.items, &r.path) {
            Item::Separator { .. } => (format!("{indent}  ────────"), String::new(), String::new()),
            Item::Folder(f) => {
                let mark = if self.collapsed.contains(&f.id) { "▸" } else { "▾" };
                (format!("{indent}{mark} {}", menu_text(&f.title, None)), String::new(), String::new())
            }
            Item::Bookmark(b) => (
                format!("{indent}  {}", menu_text(&b.title, None)),
                b.location.describe(),
                b.target.as_ref().map(Spec::describe).unwrap_or_default(),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// The organizer dialog
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Tree = 0,
    Actions = 1,
    Buttons = 2,
}

const FOCUS: [Focus; 3] = [Focus::Tree, Focus::Actions, Focus::Buttons];

type OnDone = Box<dyn FnOnce(&mut App, Option<Vec<Item>>)>;

pub struct Organizer {
    pub tree: Rc<RefCell<Tree>>,
    active: Option<Spec>,
    other: Option<Spec>,
    actions: ButtonRow,
    buttons: ButtonRow,
    focus: Focus,
    /// Dragging: (row pressed, row under the pointer).
    drag: Option<(usize, Option<usize>)>,
    on_done: Option<OnDone>,
}

struct Rects {
    outer: Rect,
    header: Rect,
    tree: Rect,
    actions: Rect,
    hint: Rect,
    buttons: Rect,
}

/// Column widths (title, folder, other) for `w` cells, from the contents
/// (Qt's `resizeColumnToContents`).
fn column_widths(tree: &Tree, w: u16) -> (u16, u16, u16) {
    let (mut tw, mut ow) = (5u16, 11u16);
    for row in 0..tree.len() {
        let (t, _, o) = tree.columns(row);
        tw = tw.max(crate::widgets::width(&t));
        ow = ow.max(crate::widgets::width(&o));
    }
    let title = (tw + 3).clamp(12.min(w), (w * 2 / 5).max(12.min(w)));
    let rest = w.saturating_sub(title);
    let other = (ow + 2).min(rest / 3);
    (title, rest.saturating_sub(other), other)
}

impl Organizer {
    pub fn new(
        items: Vec<Item>,
        active: Option<Spec>,
        other: Option<Spec>,
        on_done: impl FnOnce(&mut App, Option<Vec<Item>>) + 'static,
    ) -> Self {
        Organizer {
            tree: Rc::new(RefCell::new(Tree::new(items))),
            active,
            other,
            actions: ButtonRow::new(&ACTIONS, 0),
            buttons: ButtonRow::new(&["OK", "Cancel"], 0),
            focus: Focus::Tree,
            drag: None,
            on_done: Some(Box::new(on_done)),
        }
    }

    fn rects(&self, screen: Rect) -> Rects {
        // header + tree + actions + hint + gap + buttons
        let tree_rows = TREE_ROWS.min(screen.h.saturating_sub(2 + 5)).max(3);
        let (outer, c) = dialog_rects(screen, WIDTH, tree_rows + 5);
        let tree_h = c.h.saturating_sub(5).max(1);
        Rects {
            outer,
            header: Rect::new(c.x, c.y, c.w, 1),
            tree: Rect::new(c.x, c.y + 1, c.w, tree_h),
            actions: Rect::new(c.x, c.y + 1 + tree_h, self.actions.cells().min(c.w), 1),
            hint: Rect::new(c.x, c.y + 2 + tree_h, c.w, 1),
            buttons: Rect::new(c.x, c.bottom().saturating_sub(1), c.w, 1),
        }
    }

    fn finish(&mut self, app: &mut App, accept: bool) -> ModalOutcome {
        let items = accept.then(|| std::mem::take(&mut self.tree.borrow_mut().items));
        if let Some(f) = self.on_done.take() {
            f(app, items);
        }
        ModalOutcome::Close
    }

    /// One of the action buttons (`ACTIONS`).
    fn action(&mut self, app: &mut App, i: usize) {
        match i {
            0 => self.add_folder(app),
            1 => self.tree.borrow_mut().insert(Item::separator()),
            2 => self.edit(app),
            3 => self.tree.borrow_mut().delete(),
            4 => self.tree.borrow_mut().move_by(-1),
            5 => self.tree.borrow_mut().move_by(1),
            _ => {}
        }
    }

    fn add_folder(&mut self, app: &mut App) {
        let tree = self.tree.clone();
        dialogs::ask_text(app, "New folder", "Folder title (an & sets its hotkey):", "", "Create", move |_, title| {
            if let Some(title) = title {
                tree.borrow_mut().insert(Folder::new(title, Vec::new()).into());
            }
        });
    }

    fn edit(&mut self, app: &mut App) {
        let current = self.tree.borrow().current().cloned();
        let tree = self.tree.clone();
        match current {
            Some(Item::Folder(f)) => {
                dialogs::ask_text(app, "Rename folder", "Folder title:", &f.title, "Save", move |_, title| {
                    if let Some(title) = title {
                        tree.borrow_mut().replace(Item::Folder(Folder { title, ..f }));
                    }
                });
            }
            Some(Item::Bookmark(b)) => {
                let dialog = EditBookmark::new(b, self.active.clone(), self.other.clone(), move |_, edited| {
                    if let Some(b) = edited {
                        tree.borrow_mut().replace(Item::Bookmark(b));
                    }
                });
                app.show(dialog);
            }
            _ => {}
        }
    }

    fn tree_key(&mut self, app: &mut App, k: &KeyEvent) {
        let visible = self.tree.borrow().visible;
        let mut tree = self.tree.borrow_mut();
        match k.key {
            Key::Up | Key::Down if k.mods.alt && !k.mods.has_primary() => {
                tree.move_by(if k.key == Key::Up { -1 } else { 1 });
            }
            Key::Left if k.mods.is_empty() => tree.collapse(false),
            Key::Right if k.mods.is_empty() => tree.collapse(true),
            Key::Delete | Key::F(8) if k.mods.is_empty() => tree.delete(),
            Key::F(2) | Key::F(4) if k.mods.is_empty() => {
                drop(tree);
                self.edit(app);
            }
            Key::F(7) if k.mods.is_empty() => {
                drop(tree);
                self.add_folder(app);
            }
            _ => {
                tree.list.handle_key(k, visible);
            }
        }
    }

    fn mouse(&mut self, app: &mut App, m: &crate::input::MouseEvent, r: &Rects) -> ModalOutcome {
        let hit = {
            let tree = self.tree.borrow();
            tree.list.hit(r.tree, m.col, m.row)
        };
        match m.kind {
            MouseKind::Scroll(lines) => {
                let mut tree = self.tree.borrow_mut();
                let visible = tree.visible;
                tree.list.scroll_by((-lines * 3.0).round() as isize, visible);
            }
            MouseKind::Down(MouseButton::Left) | MouseKind::DoubleClick(MouseButton::Left) => {
                if let Some(row) = hit {
                    self.focus = Focus::Tree;
                    let mut tree = self.tree.borrow_mut();
                    tree.list.cursor = row;
                    // A click on a folder's ▸/▾ opens or closes it (drawn
                    // one cell in, after the indent: see `draw`), also
                    // when clicked twice quickly.
                    let depth = tree.rows[row].path.len() as u16 - 1;
                    if m.col == r.tree.x + 1 + 2 * depth && matches!(tree.row_item(row), Some(Item::Folder(_))) {
                        self.drag = None;
                        tree.toggle(row);
                    } else if matches!(m.kind, MouseKind::DoubleClick(_)) {
                        self.drag = None;
                        drop(tree);
                        self.edit(app);
                    } else {
                        self.drag = Some((row, None));
                    }
                }
            }
            MouseKind::Move => {
                if let Some((from, _)) = self.drag {
                    self.drag = Some((from, hit.filter(|&h| h != from)));
                }
            }
            MouseKind::Up(MouseButton::Left) => {
                if let Some((from, over)) = self.drag.take() {
                    if let Some(to) = over.or(hit.filter(|&h| h != from)) {
                        self.tree.borrow_mut().drop_onto(from, to);
                    }
                    return ModalOutcome::Consumed;
                }
                if let Some(i) = self.actions.hit(r.actions, m.col, m.row) {
                    self.focus = Focus::Actions;
                    self.actions.focus = i;
                    self.action(app, i);
                } else if let Some(i) = self.buttons.hit(r.buttons, m.col, m.row) {
                    return self.finish(app, i == 0);
                }
            }
            _ => {}
        }
        ModalOutcome::Consumed
    }
}

impl Modal for Organizer {
    fn title(&self) -> &str {
        "Organize bookmarks"
    }

    /// One line per visible row: the three columns, tab-separated.
    fn text(&self) -> String {
        let tree = self.tree.borrow();
        (0..tree.len())
            .map(|i| {
                let (t, f, o) = tree.columns(i);
                format!("{}\t{f}\t{o}", t.trim_start())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let r = self.rects(screen);
        draw_box(f, r.outer, "Organize bookmarks", ctx);
        let tree = self.tree.borrow();
        let (tw, fw, _) = column_widths(&tree, r.tree.w.saturating_sub(1));
        let head = Style::new(c.accent, c.surface).with(Flags::BOLD);
        put(f, r.header.x + 1, r.header.y, "Title", head, tw);
        put(f, r.header.x + 1 + tw, r.header.y, "Folder", head, fw);
        put(f, r.header.x + 1 + tw + fw, r.header.y, "Other panel", head, r.header.w.saturating_sub(1 + tw + fw));

        fill(f, r.tree, c.background);
        let visible = r.tree.h as usize;
        let top = tree.list.first_visible(visible);
        let focused = self.focus == Focus::Tree;
        let over = self.drag.and_then(|(_, o)| o);
        for n in 0..visible {
            let row = top + n;
            if row >= tree.len() {
                break;
            }
            let line = r.tree.row(n as u16);
            let (bg, fg) = if row == tree.list.cursor && focused {
                (c.cursor_bg, c.cursor_fg)
            } else if row == tree.list.cursor || over == Some(row) {
                (c.border, c.foreground)
            } else {
                (c.background, c.foreground)
            };
            fill(f, line, bg);
            let (t, fo, o) = tree.columns(row);
            let muted = if bg == c.background { c.muted } else { fg };
            put_elided(f, line.x + 1, line.y, &t, Style::new(fg, bg), tw.saturating_sub(1));
            // Paths are shortened at the front ("…/bm/tree/docs").
            let fmax = fw.saturating_sub(1);
            put(f, line.x + 1 + tw, line.y, &oc_grid::elide_left(&fo, fmax as usize), Style::new(fg, bg), fmax);
            let ox = line.x + 1 + tw + fw;
            let omax = line.right().saturating_sub(ox + 1);
            put(f, ox, line.y, &oc_grid::elide_left(&o, omax as usize), Style::new(muted, bg), omax);
            if over == Some(row) {
                f.put_char(line.x, line.y, '▶', Style::new(c.border_active, bg));
            }
        }
        Scrollbar { total: tree.len(), visible, top }.draw(f, r.tree, ctx);

        self.actions.draw(f, r.actions, ctx, self.focus == Focus::Actions);
        put(f, r.hint.x, r.hint.y, HINT, Style::new(c.muted, c.surface), r.hint.w);
        self.buttons.draw(f, r.buttons, ctx, self.focus == Focus::Buttons);
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        let r = self.rects(screen);
        self.tree.borrow_mut().visible = r.tree.h as usize;
        match ev {
            Event::Key(k) => {
                match k.key {
                    Key::Escape => return self.finish(app, false),
                    Key::Tab if k.mods.is_empty() || k.mods.shift => {
                        self.focus = FOCUS[cycle_focus(self.focus as usize, FOCUS.len(), k.mods.shift, |_| true)];
                        return ModalOutcome::Consumed;
                    }
                    _ => {}
                }
                match self.focus {
                    Focus::Tree if k.key == Key::Enter && k.mods.is_empty() => return self.finish(app, true),
                    Focus::Tree => self.tree_key(app, k),
                    Focus::Actions => {
                        if let RowOutcome::Activated(i) = self.actions.handle_key(k, false) {
                            self.action(app, i);
                        }
                    }
                    Focus::Buttons => {
                        if let RowOutcome::Activated(i) = self.buttons.handle_key(k, false) {
                            return self.finish(app, i == 0);
                        }
                    }
                }
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => self.mouse(app, m, &r),
            _ => ModalOutcome::Consumed,
        }
    }
}

// ---------------------------------------------------------------------------
// Edit bookmark
// ---------------------------------------------------------------------------

const EDIT_WIDTH: u16 = 76;
const EDIT_LABEL_W: u16 = 13;
const USE_W: u16 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditFocus {
    Title = 0,
    Folder = 1,
    UseActive = 2,
    Other = 3,
    UseOther = 4,
    Buttons = 5,
}

const EDIT_FOCUS: [EditFocus; 6] =
    [EditFocus::Title, EditFocus::Folder, EditFocus::UseActive, EditFocus::Other, EditFocus::UseOther, EditFocus::Buttons];

type OnEdited = Box<dyn FnOnce(&mut App, Option<Bookmark>)>;

/// Title, Folder and Other panel of one bookmark, with "Use active panel" /
/// "Use other panel" buttons that fill in what the panels show.
pub struct EditBookmark {
    bookmark: Bookmark,
    pub title: TextInput,
    pub location: TextInput,
    pub target: TextInput,
    /// What the Use buttons put in (Folder, Other panel): kept while the text is unchanged.
    picked: [Option<Spec>; 2],
    /// (active panel, other panel)
    panels: [Option<Spec>; 2],
    use_buttons: [ButtonRow; 2],
    buttons: ButtonRow,
    pub error: String,
    focus: EditFocus,
    on_done: Option<OnEdited>,
}

struct EditRects {
    outer: Rect,
    title: Rect,
    location: Rect,
    use_active: Rect,
    target: Rect,
    use_other: Rect,
    error: Rect,
    buttons: Rect,
}

impl EditBookmark {
    pub fn new(
        bookmark: Bookmark,
        active: Option<Spec>,
        other: Option<Spec>,
        on_done: impl FnOnce(&mut App, Option<Bookmark>) + 'static,
    ) -> Self {
        let mut target = TextInput::new(&bookmark.target.as_ref().map(Spec::describe).unwrap_or_default());
        target.placeholder = "optional: a folder to open in the other panel".into();
        let use_button = |label: &str, spec: &Option<Spec>| {
            let mut row = ButtonRow::new(&[label], 0);
            row.buttons[0].enabled = spec.is_some();
            row
        };
        EditBookmark {
            title: TextInput::new(&bookmark.title),
            location: TextInput::new(&bookmark.location.describe()),
            target,
            picked: [None, None],
            use_buttons: [use_button("Use active panel", &active), use_button("Use other panel", &other)],
            panels: [active, other],
            buttons: ButtonRow::new(&["Save", "Cancel"], 0),
            error: String::new(),
            bookmark,
            focus: EditFocus::Title,
            on_done: Some(Box::new(on_done)),
        }
    }

    fn rects(&self, screen: Rect) -> EditRects {
        let (outer, c) = dialog_rects(screen, EDIT_WIDTH, 6);
        let field_w = c.w.saturating_sub(EDIT_LABEL_W);
        let input_w = field_w.saturating_sub(USE_W + 1);
        let row = |y: u16, w: u16| Rect::new(c.x + EDIT_LABEL_W, c.y + y, w, 1);
        // Left-aligned in their column (each button as wide as its label needs).
        let button = |y: u16| {
            let w = self.use_buttons[usize::from(y == 2)].cells().min(USE_W);
            Rect::new(c.right().saturating_sub(USE_W), c.y + y, w, 1)
        };
        EditRects {
            outer,
            title: row(0, field_w),
            location: row(1, input_w),
            use_active: button(1),
            target: row(2, input_w),
            use_other: button(2),
            error: Rect::new(c.x, c.y + 3, c.w, 1),
            buttons: Rect::new(c.x, c.bottom().saturating_sub(1), c.w, 1),
        }
    }

    /// "Use active panel" (0) / "Use other panel" (1).
    fn use_panel(&mut self, which: usize) {
        if let Some(spec) = self.panels[which].clone() {
            let field = if which == 0 { &mut self.location } else { &mut self.target };
            field.set_text(&spec.describe());
            self.picked[which] = Some(spec);
        }
    }

    fn spec(&self, which: usize) -> Result<Option<Spec>, String> {
        let (field, original) = if which == 0 {
            (&self.location, Some(&self.bookmark.location))
        } else {
            (&self.target, self.bookmark.target.as_ref())
        };
        if let Some(p) = &self.picked[which]
            && field.text.trim() == p.describe()
        {
            return Ok(Some(p.clone()));
        }
        parse_location(&field.text, original)
    }

    fn save(&mut self, app: &mut App) -> ModalOutcome {
        let (location, target) = match (self.spec(0), self.spec(1)) {
            (Ok(l), Ok(t)) => (l, t),
            (Err(e), _) | (_, Err(e)) => {
                self.error = e;
                return ModalOutcome::Consumed;
            }
        };
        let Some(location) = location else {
            self.error = "Enter a folder.".into();
            return ModalOutcome::Consumed;
        };
        let title = self.title.text.trim();
        let edited = Bookmark {
            title: if title.is_empty() { "Bookmark".into() } else { title.into() },
            location,
            target,
            id: self.bookmark.id.clone(),
        };
        if let Some(f) = self.on_done.take() {
            f(app, Some(edited));
        }
        ModalOutcome::Close
    }

    fn cancel(&mut self, app: &mut App) -> ModalOutcome {
        if let Some(f) = self.on_done.take() {
            f(app, None);
        }
        ModalOutcome::Close
    }

    fn focused_input(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            EditFocus::Title => Some(&mut self.title),
            EditFocus::Folder => Some(&mut self.location),
            EditFocus::Other => Some(&mut self.target),
            _ => None,
        }
    }

    fn focusable(&self, i: usize) -> bool {
        match EDIT_FOCUS[i] {
            EditFocus::UseActive => self.panels[0].is_some(),
            EditFocus::UseOther => self.panels[1].is_some(),
            _ => true,
        }
    }
}

impl Modal for EditBookmark {
    fn title(&self) -> &str {
        "Edit bookmark"
    }

    fn text(&self) -> String {
        format!("{}\n{}\n{}\n{}", self.title.text, self.location.text, self.target.text, self.error)
    }

    fn wants_text_input(&self) -> bool {
        matches!(self.focus, EditFocus::Title | EditFocus::Folder | EditFocus::Other)
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let r = self.rects(screen);
        let content = draw_box(f, r.outer, "Edit bookmark", ctx);
        let label = Style::new(c.foreground, c.surface);
        for (row, text) in [(r.title, "Title"), (r.location, "Folder"), (r.target, "Other panel")] {
            put(f, content.x, row.y, text, label, EDIT_LABEL_W);
        }
        self.title.draw(f, r.title, ctx, self.focus == EditFocus::Title);
        self.location.draw(f, r.location, ctx, self.focus == EditFocus::Folder);
        self.target.draw(f, r.target, ctx, self.focus == EditFocus::Other);
        self.use_buttons[0].draw(f, r.use_active, ctx, self.focus == EditFocus::UseActive);
        self.use_buttons[1].draw(f, r.use_other, ctx, self.focus == EditFocus::UseOther);
        put(f, r.error.x, r.error.y, &self.error, Style::new(c.muted, c.surface), r.error.w);
        self.buttons.draw(f, r.buttons, ctx, self.focus == EditFocus::Buttons);
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        let r = self.rects(screen);
        match ev {
            Event::Key(k) => {
                match k.key {
                    Key::Escape => return self.cancel(app),
                    Key::Tab if k.mods.is_empty() || k.mods.shift => {
                        let i = cycle_focus(self.focus as usize, EDIT_FOCUS.len(), k.mods.shift, |i| self.focusable(i));
                        self.focus = EDIT_FOCUS[i];
                        return ModalOutcome::Consumed;
                    }
                    _ => {}
                }
                match self.focus {
                    EditFocus::UseActive | EditFocus::UseOther => {
                        let which = usize::from(self.focus == EditFocus::UseOther);
                        if matches!(k.key, Key::Enter | Key::Space) && k.mods.is_empty() {
                            self.use_panel(which);
                        }
                    }
                    EditFocus::Buttons => {
                        if let RowOutcome::Activated(i) = self.buttons.handle_key(k, true) {
                            return if i == 0 { self.save(app) } else { self.cancel(app) };
                        }
                    }
                    _ => {
                        if k.key == Key::Enter && k.mods.is_empty() {
                            return self.save(app);
                        }
                        let clip = app.clipboard.as_mut();
                        if let Some(input) = self.focused_input() {
                            input.handle_key(k, clip);
                        }
                    }
                }
                ModalOutcome::Consumed
            }
            Event::Commit(_) | Event::Preedit(_) => {
                let clip = app.clipboard.as_mut();
                if let Some(input) = self.focused_input() {
                    input.handle_event(ev, clip);
                }
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => {
                match m.kind {
                    MouseKind::Down(MouseButton::Left) => {
                        for (rect, focus) in
                            [(r.title, EditFocus::Title), (r.location, EditFocus::Folder), (r.target, EditFocus::Other)]
                        {
                            if rect.contains(m.col, m.row) {
                                self.focus = focus;
                                if let Some(input) = self.focused_input() {
                                    input.click(rect, m.col);
                                }
                            }
                        }
                    }
                    MouseKind::Up(MouseButton::Left) => {
                        if self.use_buttons[0].hit(r.use_active, m.col, m.row).is_some() {
                            self.use_panel(0);
                        } else if self.use_buttons[1].hit(r.use_other, m.col, m.row).is_some() {
                            self.use_panel(1);
                        } else if let Some(i) = self.buttons.hit(r.buttons, m.col, m.row) {
                            return if i == 0 { self.save(app) } else { self.cancel(app) };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(title: &str, path: &str) -> Bookmark {
        Bookmark::new(title, Spec::local(path), None)
    }

    fn kinds(items: &[Item]) -> Vec<&'static str> {
        items
            .iter()
            .map(|i| match i {
                Item::Bookmark(_) => "Bookmark",
                Item::Folder(_) => "Folder",
                Item::Separator { .. } => "Separator",
            })
            .collect()
    }

    /// Port of `test_organizer_folders_order_delete`.
    #[test]
    fn folders_order_delete() {
        let (a, b) = (bm("&A", "/t"), bm("&B", "/t/src"));
        let mut t = Tree::new(vec![a.clone().into(), b.clone().into()]);
        t.list.cursor = 1; // B
        t.move_by(-1); // B above A
        assert_eq!(t.current().map(Item::id), Some(b.id.as_str()));
        t.insert(Folder::new("&Group", vec![]).into()); // after B
        t.insert(Item::separator()); // the current entry is the new folder: goes inside it
        assert!(t.select_title("&A"));
        t.delete();
        assert_eq!(kinds(&t.items), ["Bookmark", "Folder"]);
        assert_eq!(t.items[0].id(), b.id);
        let Item::Folder(g) = &t.items[1] else { panic!() };
        assert_eq!((g.title.as_str(), kinds(&g.items)), ("&Group", vec!["Separator"]));
    }

    #[test]
    fn dragging_into_folders_and_between_entries() {
        let folder = Folder::new("&Work", vec![bm("&In", "/in").into()]);
        let mut t = Tree::new(vec![bm("&A", "/a").into(), folder.into(), bm("&B", "/b").into()]);
        // Rows: A, Work, In, B. Drop A onto Work: into it, at the end.
        assert!(t.drop_onto(0, 1));
        let Item::Folder(w) = &t.items[0] else { panic!() };
        assert_eq!(w.items.len(), 2);
        assert_eq!(t.current().map(|i| matches!(i, Item::Bookmark(b) if b.title == "&A")), Some(true));
        // Rows: Work, In, A, B. Drop B onto In: before it, inside the folder.
        assert!(t.drop_onto(3, 1));
        let Item::Folder(w) = &t.items[0] else { panic!() };
        assert_eq!(kinds(&w.items), ["Bookmark", "Bookmark", "Bookmark"]);
        assert_eq!(t.items.len(), 1);
        // A folder can't go into itself.
        assert!(!t.drop_onto(0, 1));
        // Collapsing hides the children; expanding shows them again.
        t.list.cursor = 0;
        t.collapse(false);
        assert_eq!(t.len(), 1);
        t.collapse(true);
        assert_eq!(t.len(), 4);
    }

    #[test]
    fn edit_parses_typed_locations() {
        let inside = Spec::archive(Spec::local("/t/a.zip"), "docs");
        let b = Bookmark::new("&Docs", inside.clone(), None);
        let mut d = EditBookmark::new(b.clone(), Some(Spec::local("/act")), None, |_, _| {});
        assert_eq!(d.spec(0), Ok(Some(inside)), "unchanged text keeps the archive spec");
        assert_eq!(d.spec(1), Ok(None));
        d.use_panel(0);
        assert_eq!(d.spec(0), Ok(Some(Spec::local("/act"))));
        d.use_panel(1); // no other panel: nothing happens
        assert_eq!(d.target.text, "");
    }
}
