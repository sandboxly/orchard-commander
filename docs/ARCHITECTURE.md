# UI architecture (oc-ui + the app binary)

This is the guide for adding features to Orchard Commander 2.x. Read it before
touching `crates/oc-ui` or `app/`. The Python 1.x code
(`../fcmd/src/orchard_commander/ui/`) is the behavioural reference; this
document says where each kind of thing goes in Rust.

## The pieces

```
app/ (binary "orchard-commander")        crates/oc-ui (all UI state and logic)
├── main.rs      args, --self-test,       ├── app.rs          App: settings, theme, windows, dialogs,
│                --screenshot             │                   tasks, timers, command execution
├── platform.rs  winit event loop,        ├── tasks.rs        run_in_background / spawn / post / ask_ui
│                GPU surfaces, input      ├── commands.rs     Command + Registry (ids, shortcuts, F-keys, menus)
│                conversion, wake-ups     ├── menus.rs        NC menus model, in-window strip (F9)
├── macos.rs     native menu (muda),      ├── main_window/    layout, drawing, input routing, side views
│                Dock icon                ├── panel/          FilePanel state, listing, watching, drawing
├── selftest.rs                           ├── dialogs/        MessageBox, InputBox, ProgressBox, Picker
└── screenshot.rs                         ├── widgets/        BoxFrame, tabs, buttons, text input, checkbox,
                                          │                   list, scrollbar, dropdown menu
                                          ├── features/       one module per feature + ALL (the registry list)
                                          ├── connections/    servers: sessions, ⌘K dialogs, api (resolve_server)
                                          ├── modal.rs        Modal trait (dialog stack per window)
                                          ├── input.rs        Key/Mods/Event, winit conversion
                                          ├── shortcut.rs     "Primary+Shift+." parsing and display
                                          ├── theme.rs        ColorScheme → grid colors, frame style, font
                                          └── testing.rs      headless App for tests and screenshots
```

Lower crates (`oc-vfs`, `oc-local`, `oc-archive`, `oc-config`, `oc-preview`,
`oc-search`, `oc-rename`, `oc-term`, `oc-grid`) never depend on `oc-ui`.

### Ownership and flow

`oc_ui::App` owns **all** UI state. The binary owns only window-system
objects (winit windows, wgpu surfaces, the muda menu). Per platform event:

```
winit event ─► App::handle_event(win, Event) ─► top dialog of that window
                                              └► else main window / other window
worker done ─► EventLoopProxy(Wake) ─► App::process_tasks() ─► on_done(&mut App, result)
timer due   ─► App::tick(now)
afterwards  ─► App::take_requests()  (open/close windows, quit, theme changed, menus changed)
            ─► App::take_redraw()    → request_redraw → App::draw(win, &mut Frame, cell_px)
```

* **Redraw only on change.** Handlers that change state make the app dirty
  (any consumed event, any task result, any timer, `app.redraw()`); the loop
  then sleeps (`ControlFlow::Wait` / `WaitUntil(next timer)`).
* **Drawing is a pure function** `draw(&state, &mut Frame, Rect, &DrawCtx)`
  into an `oc_grid::Frame` per window. Never mutate state while drawing; if
  drawing needs something computed (scroll offsets for the current size),
  compute it locally (see `panel::draw::first_row`).
* **Layout is recomputed, never stored**: `main_window::layout(app)` and each
  widget's `rects(...)` are used by both drawing and hit testing.
* **Never block the UI thread** on file systems, network or processes.
  Everything that may touch a disk or server goes through `app.tasks`.

## Background work: `tasks.rs`

```rust
// Result back on the UI thread unless cancelled meanwhile:
let token = app.tasks.run_in_background(
    move |token| location.list_dir(opts),            // worker thread
    move |app, result| { /* &mut App, UI thread */ },
);
token.cancel();                                       // on_done is skipped

app.tasks.spawn(job, on_done);                        // always delivers (ops with their own cancel)
app.tasks.post(|app| ...);                            // from any thread: run on the UI thread

// From a worker: ask the user and wait (conflicts, host keys, passwords).
let answer: Option<(Conflict, bool)> = tasks.ask_ui(move |app, reply| {
    dialogs::ask_conflict(app, &src, &dst, reply);    // reply.send(..) when the dialog closes
});                                                   // None if the dialog was torn down
```

Each job gets its own thread (a stuck cloud placeholder or server must not
starve other work). `Tasks` is `Clone + Send`; keep a clone in workers.
Throttle progress posts (see `features/fileops.rs`: shared state in a
`Mutex`, a `post(|app| app.redraw())` at most every 50 ms).

Timers: `app.schedule(Duration, |app| ...) -> id`, `app.cancel_timer(id)`.
Deferred work after the current event: `app.later(|app| ...)`.

## Commands: `commands.rs`

A command = id, title, help, shortcuts, handler `Fn(&mut App)`, optional
F-key label, menu placement, check/enabled predicates, macOS menu role.

```rust
reg.add(
    Command::new("files.copy", "Copy", |app| transfer(app, Kind::Copy))
        .help("Copy marked files to the other panel")
        .keys(&["F5"])                      // every platform
        .mac_keys(&["Cmd+Alt+C"])           // macOS only   (example)
        .other_keys(&["Ctrl+Insert"])       // Windows/Linux only (example)
        .fkey("F5", "Copy")                 // F-key bar
        .menu(Menu::Files, "ops")           // NC menu + section (see menus.rs)
        .menu_label("Copy…")                // if the menu text differs from the title
        .checked(|app| ...)                 // check mark in menus
        .enabled(|app| ...),                // greyed out / not run when false
);
```

* `Primary` is ⌘ on macOS and Ctrl elsewhere (Qt's `ControlModifier`); `Ctrl`
  is the Control key itself (⌃ on macOS). Take shortcuts from
  `../fcmd/docs/manual/13-keyboard-reference.md` and `main_window.py`.
* Key matching uses the key **without** modifiers applied (⌥⌘A is `a`, not
  `å`; ⇧⌘. is `.` + Shift). Typed text (for `+`, `*`, quick search, inputs)
  is in `KeyEvent::text`.
* A shortcut belongs to one command; the test
  `every_shortcut_belongs_to_one_command` fails on duplicates.
* Dispatch order in the main window: open dialog → command shortcuts → `Tab`
  (switch side) → the active side (file panel keys or the side view).
* The same registry feeds: key dispatch, the native macOS menu bar (each
  item shows the command's first shortcut, `Command::menu_shortcut`, as
  1.x's Qt menus did), the in-window strip (F9), the F-key bar (`.fkey`),
  the right end of the bar (`.footer_right("Commands")`) and the command
  palette (⌘P, fuzzy filter, 1.x's order; `.hidden_from_palette()` to leave
  one out). `.alias_keys` adds keys that run a command without being shown.
* Native menu items: a click is `App::menu_activated`, a key equivalent
  (macOS gives those keys to the menu, not the window; the binary tells them
  apart by `NSApp.currentEvent`) is `App::menu_key_equivalent`. One without
  ⌘ (F5, ⌥F7, ⌃`) goes to the focused window as the key it is, so the
  terminal still gets it. While a
  dialog is open a click does nothing (like Qt's menus under a modal
  dialog) and a key equivalent goes to the dialog as its key (so ⌘A selects
  text in a field). `.global()` commands (Quit) run anyway.
* A command whose `.enabled` predicate says false doesn't take its key: the
  key goes on to the panel / side view (e.g. `search.stop` owns Esc only
  while a search runs).

## Adding a feature module (the usual case)

1. Create `crates/oc-ui/src/features/<name>.rs` with
   `pub fn register(reg: &mut Registry)`.
2. Add `pub mod <name>;` and one line `<name>::register,` to `ALL` in
   `features/mod.rs`. That is the only shared edit.
3. In `register`: `reg.add(Command…)`, `reg.add_side_view(factory)`,
   `reg.on_start(fn)`, `reg.on_settings_changed(fn)`,
   `reg.on_location_changed(fn(app, side))` (a panel arrived in another
   folder), `reg.on_resized(fn)` (the main window's grid size changed),
   `reg.on_files_changed(fn)`,
   `reg.on_files_moved(fn(&mut App, &HashMap<VPath, VPath>))` as needed. A feature that changes files
   (file operations, multi-rename, editor saves…) calls
   `commands::files_changed(app)` afterwards (`fileops::finished` does).
   A feature that renames or moves files calls
   `commands::files_moved(app, &old_to_new)` before reloading the panels
   (multi-rename does); one that shows files by location (search results:
   `SearchResultsFS::relocate`) listens with `on_files_moved`.
   Also:
   * `reg.add_location_source(fn(&App, side) -> Vec<LocationItem>)`: more
     entries in the ⌥F1/⌥F2 chooser after the drives (`LocationItem { label,
     hint, open: Box<dyn FnOnce(&mut App, side)> }` from
     `features::navigation`). Called when the chooser opens, on the UI
     thread: return what you have in memory, no I/O. Servers add "⇄ name"
     (saved connections, then open sessions); bookmarks add "★ title".
   * `reg.on_load_error(fn(&mut App, side, &VPath, &VfsError) -> bool)`: a
     panel listing failed; return true to take over (the connections feature
     reconnects a dropped session and lists again) instead of the error toast.
   * `reg.on_shutdown(fn(&mut App))`: in `App::shutdown`; keep it bounded.
4. Keep the feature's state in its own types. Where it must live in the app:
   * per side: a `SideView` (below);
   * a window: a `UiWindow` (below);
   * app-wide (bookmark list, connection sessions, last search results, open
     editors…): a `#[derive(Default)]` struct in your module, reached with
     `app.state::<MyState>()` (`&mut`, created on first use),
     `app.try_state::<MyState>()` (`&`, for drawing and menu predicates), or
     `take_state` / `put_state` when you need it and `&mut App` together.
     Don't add fields to `App`: that is the file every agent would edit.
5. Persist settings through `app.update_settings(move |s| ...)` (applied now,
   written in the background on top of the file's current contents, like
   Python's `Settings.update`). Feature data goes in `s.extra` under a
   prefixed key (`"preview.<id>.<key>"`, `"terminal_height"`…) — the keys
   1.x writes must keep their meaning.
6. Tests: unit tests next to the code; app-level tests in
   `crates/oc-ui/tests/` using `oc_ui::testing::{headless_app, settle,
   wait_until, press, press_with, type_text}`.

Menu sections are listed in `Menu::sections()` (`menus.rs`). Use an existing
section; add a new name there only if a new group is really needed.

Other registry hooks:

* `reg.on_panel_title_click(fn(&mut App, side, col, row))`: a left click on
  a file panel's path (its frame title). Bookmarks use it for their menu
  (1.x: the panel's `title_clicked`). One handler.
* `reg.add_location_source(fn(&App, side) -> Vec<LocationChoice>)`: extra
  entries after the drives in the drive / location list (⌥F1 / ⌥F2); each
  has a label, a dimmed hint and an `open(app, side)` closure. Bookmarks add
  "★ Title"; connections add "⇄ Name".
* `App::persists()`: false in `--screenshot` mode; features writing their
  own files (bookmarks.json…) skip writing then.
* `reg.on_shutdown(fn(&mut App))`: runs in `App::shutdown` (quitting),
  before settings are flushed. Features that write their own files in the
  background make sure the latest state is on disk here (bookmarks do).

### Bookmarks and location specs (`features/bookmarks/`)

* State: `app.try_state::<bookmarks::Bookmarks>()` (`bookmarks::items(app)`);
  `bookmarks.json` is read in the background at start (commands that come
  earlier wait for it) and written in the background after each change
  (and once more on quit if a write is still pending).
* `bookmarks::locations::spec_of(app, &vpath)` describes a folder as an
  `oc_config::Spec` (archives nest; servers by connection account);
  `bookmarks::locations::resolve(app, &spec, Box::new(|app, Result<VPath,
  String>| …))` goes back, never blocking: archives are opened on a worker,
  servers through `connections::api::resolve_server(app, account, path,
  on_done)`.
* The connections feature tells which account a server file system belongs
  to with `bookmarks::locations::register_account_of(app, fn(&App,
  &dyn FileSystem) -> Option<String>)` (call it from its `on_start`);
  without it, server folders can't be bookmarked (opening server bookmarks
  works regardless).
* `bookmarks::navigate(app, &spec, side, focus)` / `open_bookmark` for other
  features that want to go to a saved place.

## Side views (Preview, terminal-in-a-side, …): `main_window/side.rs`

Each side shows its file panel ("Files" tab) or one of the registered side
views (one tab each, after "Files"). Register a factory; each side gets its
own instance:

```rust
reg.add_side_view(|| Box::new(MyView::default()));

impl SideView for MyView {
    fn id(&self) -> &'static str { "preview" }       // stored as extra["left_mode"]
    fn title(&self) -> String { "Preview".into() }   // tab label
    fn draw(&self, app: &App, side: usize, f: &mut Frame, frame: Rect, ctx: &DrawCtx, active: bool) { … }
    fn handle_event(&mut self, app: &mut App, side: usize, ev: &Event, frame: Rect) -> Outcome { … }
    fn follows_other_panel(&self) -> bool { true }   // Preview semantics (focus stays on the other panel)
    fn target_changed(&mut self, app: &mut App, side: usize, target: Option<VPath>) { … }
    fn deactivated(&mut self, app: &mut App, side: usize) { … }
    fn resized(&mut self, app: &mut App, side: usize, frame: Rect) { … }   // window/font size changed
    fn as_any(&self) -> Option<&dyn Any> { Some(self) }                    // opt in to with_view_as / view_as
    fn as_any_mut(&mut self) -> Option<&mut dyn Any> { Some(self) }
}
```

* Switch with `main_window::set_mode(app, side, SideMode::View(i))` or
  `toggle_view(app, side, "preview")`; modes persist as `left_mode` /
  `right_mode` like 1.x.
* A view that follows the other panel gets `target_changed` whenever that
  panel's cursor moves (and when it is shown). Load content in the
  background (`app.tasks`), keep a request counter or `CancelToken` to drop
  stale results, call `app.redraw()` when done.
* Timers and task results reach their view with
  `main_window::with_view_as::<MyView, _>(app, side, "id", |v, app| …)`
  (the view is taken out of the app meanwhile); drawing helpers and tests
  read it with `main_window::view_as::<MyView>(&app, side, "id")`.
* The bottom pane is the terminal (`features/terminal/`): it sets
  `app.main.bottom_rows` (`Layout::bottom`) and is drawn / routed by one call
  each in `main_window::draw` and `main_window::handle_event`
  (`terminal::pre_event` runs before command shortcuts so a focused terminal
  gets Esc, Tab and F-keys; it returns `Ignored` for the toggle and macOS ⌘
  shortcuts). While it has the focus, `terminal::has_focus(app)` makes both
  panels draw inactive.

## The terminal: `features/terminal/`

* `mod.rs`: state (`app.state::<Terminal>()`), ⌘J / ⌃` toggle, one
  `oc_term::Session` per side started ~50 ms after showing at the final size
  (`Session::start` runs on a worker), keys/mouse/copy/paste, the top border
  drag, `terminal_visible` / `terminal_height` (1.x, logical px) +
  `terminal_rows` (2.x, exact).
* `view.rs`: `Snapshot` → grid cells (theme palette via
  `oc_term::Palette::for_theme(foreground, surface, dark)`), cursor shapes,
  key encoding. The grid renderer does bold/italic/underline/dim, wide
  chars, box/powerline glyphs and the Nerd Font fallback.
* `link.rs`: linked folders. A "terminal link" thread probes the shells
  every 300 ms and posts only when something changed (no idle wake-ups);
  paths are resolved (symlinks) there or in a background task, never on the
  UI thread.
* Session output wakes the UI only for the session on screen, coalesced to
  one post per wake-up. Tests: `terminal::record_input` / `take_recorded`
  capture what would be sent to the shell (`crates/oc-ui/tests/terminal.rs`).

## Images on screen: `images.rs`

The UI never touches the GPU. A feature puts straight RGBA8 pixels into the
app-wide image store and draws a placement naming them; the binary uploads
what a frame shows (only when the pixels changed) into each window's
renderer (`ImageSync`, in `platform.rs` and `screenshot.rs`) and frees the
textures of removed images.

```rust
let key = images::store(app).insert(w, h, Arc::new(rgba));    // Arc<dyn Pixels>: Vec<u8>, PDF pages…
f.add_image(ImagePlacement::new(key.id(), PxRect::new(x, y, dw, dh)));   // in draw
images::store(app).set(key, w, h, next_frame);                 // animation / video frame: same key
images::store(app).remove(key);                                // when the view goes away
```

`key.id()` is an `oc_grid::ImageId::external(..)`; `GridRenderer::set_image`
stores pixels under such caller-chosen ids. `ctx.cell_px` / `app.cell_px`
are physical pixels, so `cells × cell_px` already includes the display
scale: render PDF pages and fit images to that.

## The Preview: `features/preview/`

`mod.rs` is the pane (`PreviewView`, a `SideView`): debounced loading
through `oc_preview::Registry::global()` on a worker with cancellation, the
previous content kept until the next one is ready, "View as" chips
(Shift+F3; the choice per extension lasts until quit), title = file name,
subtitle = the previewer's info, cloud placeholders ("Downloading … from
the cloud"; above 25 MB "Download and preview"; "Try again"). Reload panels
loads it again. `PreviewOptions` (an app state) holds the cloud fetch
timeout (tests shorten it).

What shows below the chips is a `views::ContentView`, one per kind of
`PreviewData`: `text.rs` (highlighted text, hex, listings, plus the shared
painting and scrolling helpers), `markdown.rs`, `image.rs` (right-click
Open/Copy via `dialogs::PopupMenu`, `Clipboard::set_image`), `pdf.rs`,
`media.rs` (the audio/video player, below), `message.rs`.

```rust
pub trait ContentView: Any {
    fn kind(&self) -> &'static str;                                 // "pdf", "media", …
    fn draw(&self, app: &App, f: &mut Frame, area: Rect, ctx: &DrawCtx, focused: bool);
    fn handle_event(&mut self, app: &mut App, h: Handle, ev: &Event, area: Rect) -> Outcome;  // keys after Tab; mouse
    fn shown(&mut self, app: &mut App, h: Handle, area: Rect);     // upload images, start timers / playback
    fn resized(&mut self, app: &mut App, h: Handle, area: Rect);
    fn leaving(&mut self, app: &mut App);                          // another file is loading: stop the sound
    fn play(&mut self, app: &mut App, h: Handle);                  // play-next moved on to this file
    fn clear(&mut self, app: &mut App);                            // remove images, cancel timers, stop playback
    fn text(&self) -> Option<String>;                              // tests
    fn as_any(&self) -> &dyn Any;  fn as_any_mut(&mut self) -> &mut dyn Any;
}
// From a timer, task result or player event — the view, if it is still the one shown:
h.with::<MyContent, _>(app, |view, app, area| …);
```

Loading is two steps: `views::prepare(PreviewData) -> Prepared` runs on the
worker (put expensive preparation there), `views::make(Prepared, &VPath)`
builds the view on the UI thread. Two more `ContentView` hooks, both with
no-op defaults: `leaving(app)` (the pane started loading another file while
this view stays shown: stop the sound) and `play(app, h)` (start playback).

**The media player: `media.rs`** (+ `media_paint.rs`, pixel painting with
fill rects). `shown` opens an `oc_media::Player` (and, for audio, a
`WaveformAnalyzer`) with a waker that `tasks.post`s one wake-up at a time to
`pump` (events, latest video frame). Timers run only while something moves:
the spectrum at 60 fps while playing or falling, the video frame timer at
`Player::next_frame_in` while playing. Video frames go into the image store
under one key (`impl Pixels for oc_media::VideoFrame`, no copy). `clear`
drops player and analyzer (neither waits for its threads); a worker result
nobody takes (`Arrived` in `mod.rs`) closes streamed media data on a thread
of its own, wherever it is dropped. Whether the screen is video or audio
comes from the probe in `MediaData.info` (the extension only guesses).
Mute/autoplay
live in `Settings.extra` `preview.media.muted` / `preview.media.autoplay`;
`media::settings_changed` (an `on_settings_changed` hook) makes shown
players follow mute and `play_next` (looping). At the end of a file the view
calls `preview::finished_playing` (`app.later`), which moves the other
panel's cursor to the next file the same previewer scores and sets the
pane's `play_when_shown`; the pane calls `play` once that file is shown.
`MediaOptions` (an app state) picks the audio output: tests and
`--screenshot` call `media::silence(app)`.

## Dialogs: `modal.rs`, `dialogs/`

A dialog is a `Modal` pushed on a window's stack; the top one gets all input
for that window (Esc/Enter are its own business). It reports results through
a callback that gets `&mut App`.

```rust
dialogs::confirm(app, "Move to trash", "Move 'x' to the trash?", "Trash", |app| …);
dialogs::ask_text(app, "Make directory", "Create in …:", "", "Create", |app, text| …);
dialogs::message(app, "Copying", &error, Severity::Error);
let id = app.show(ProgressBox::new("Copying", state, token));   app.close_modal(id);
app.show(Picker::new("Go to", items, |app, choice| …).on_highlight(|app, i| …));
app.show(PopupMenu::new(vec![MenuEntry::item("&Open", "")], col, row, |app, i| …));   // context menu
app.show(MessageBox::new("File exists", text, &["&Overwrite", "&All", "&Skip", "S&kip all", "Cancel"], 2)
    .on_choice(|app, idx| …));
```

Write a new dialog by implementing `Modal` (`title`, `draw`, `handle_event`,
optionally `wants_text_input` for IME and `text` for tests). Use
`dialogs::dialog_rects` + `dialogs::draw_box` (it calls `widgets::cover`, so
pixel overlays of what's below don't show through). `&` in button labels marks
hotkeys (plain letter when no text field has focus, Alt+letter otherwise).

## Other windows (editor, …): `UiWindow`

```rust
let id = app.open_window(Box::new(EditorWindow::new(…)), 100, 40);   // platform opens it
impl UiWindow for EditorWindow {
    fn title(&self) -> String;
    fn draw(&self, app: &App, f: &mut Frame, ctx: &DrawCtx);
    fn handle_event(&mut self, app: &mut App, id: WinId, ev: &Event) -> Outcome;
    fn close_requested(&mut self, app: &mut App, id: WinId) -> bool;   // false: e.g. ask to save first
    fn wants_text_input(&self) -> bool;
    fn menus(&self) -> Vec<WindowMenu>;                                  // own native menus (macOS), optional
    fn as_any(&self) -> &dyn Any;  fn as_any_mut(&mut self) -> &mut dyn Any;
}
app.window_mut::<EditorWindow>(app.focused)   // from a command acting on the focused window
app.window::<EditorWindow>(id)                // read-only (tests, predicates)
let w = app.take_window(id)?; …; app.put_window(id, w);   // the window together with &mut App
app.request(PlatformRequest::RaiseWindow(id)) // bring it to the front (already open, asking)
```

* Task results and dialog answers reach a window by id: take it out, use it
  with `&mut App`, put it back (`features::editor::window::with_editor` wraps
  that). Don't close a window from inside its own `handle_event` (it is taken
  out meanwhile): `app.later(move |app| app.close_requested(id))`.
* Dialogs can be pushed on any window (`app.push_modal(id, …)`).
* Quitting asks the other windows first, one at a time: `App::quit` sets
  `app.quit_pending` and stops at the first window that refuses. A window
  that refused to ask the user calls `app.quit()` again once it has closed
  (or clears `quit_pending` on Cancel); see `features::editor::window::close_window`.
* Own menus (macOS): `menus()` returns `WindowMenu`s whose items carry a
  shortcut. While such a window is focused the binary swaps in a menu bar
  with the application menu, these menus and Window
  (`app/src/macos.rs: WindowMenuBar`), so the main menus' ⌘ keys don't
  fire; choosing an item sends its shortcut to the window as a key
  (`App::menu_activated`). `PlatformRequest::MenusChanged` refreshes the
  enabled states. Registry commands chosen while another window is focused
  reach it as their key, too (only `.global()` ones run).
* `--screenshot --keys`: keys go to the focused window; a window they open
  gets the focus (and the screenshot size) and the focused window is drawn,
  e.g. `--keys "Down F4 Primary+F a b c Enter"` shows the editor.

### The editor (`features/editor/`)

F4 / Shift+F4 open an `EditorWindow` per file (`EditorState` lists them).
`buffer.rs` is a ropey rope with cursor, selection and undo groups (typing
merges; each step has a unique id and "modified" = the newest step isn't
the one that was saved). Columns skip runs of plain ASCII a rope chunk at a
time and find uses the rope's line iterator + `str` search, so multi-MB
files and minified one-line files stay fast; Replace all is a single edit.
`highlight.rs` highlights with syntect on a worker in 30 ms slices on a rope
snapshot, keeping parser states every 64 lines; after an edit it restarts at
the edited line and stops when the state at a later checkpoint matches the
old one (`oc_preview::highlight::LineHighlighter` / `LineState`); lines over
20 000 bytes stay plain. Files are
read, decoded (`oc_preview::textfile`), turned into a rope and given a syntax
on a worker; local saves are a temp file + atomic rename keeping permissions,
others `open_write`; local files are watched (folder watch, 300 ms debounce,
own saves recognised by mtime+size).

## Widgets: `widgets/`

State structs with `draw(&self, f, rect, ctx[, focused])` and
`handle_key/handle_event/hit(...)`:

| Widget | Notes |
| --- | --- |
| `BoxFrame` | frame style from settings, title (top, elided left), subtitle (bottom right), active color; half-cell surface strips like `boxframe.py` |
| `boxframe::draw_tabs` / `tab_at` | mode tabs above a frame |
| `ButtonRow` | centered buttons, focus, ←/→, Enter/Space, hotkeys, `hit` |
| `TextInput` | cursor, selection (⇧), word/line moves, clipboard (⌘C/X/V/A), IME preedit/commit, scrolls to the cursor |
| `Checkbox` | Space / Alt+hotkey / click |
| `ListState` | cursor + scroll for any length; `draw` calls back per visible row; scrollbar |
| `Scrollbar` | pixel-exact thumb |
| `DropdownMenu` | items with shortcuts, checks, separators, hotkeys, mouse |
| `cover(f, rect, ctx)` | call before drawing anything that floats over other content (cuts pixel rects around it, clips images to what stays visible) |
| `features::settings::form` | combo box field + `Popup` list (separators, type-to-jump, ≤ 20 rows), spin box, heading/label/muted text — for settings-like forms |

A focused `BoxFrame` (panels, dialogs, popups) draws its border with
`Flags::STRONG`: oc-grid draws box-drawing lines one step thicker (1.x's
2 px pen). The check marks `✓`/`✔` are drawn geometrically when no
installed font has them.

Colors come only from `ctx.theme.colors` (the scheme's roles: `surface`,
`border_active`, `cursor_bg`, `footer_key`, …). Never hard-code colors.

## Find files: `features/search/`

* ⌘F / ⌥F7 → `FindFilesDialog`; `search::start_search(app, query, side)`
  runs `oc_search` on a worker (`tasks.spawn`) and shows an
  `oc_search::SearchResultsFS` in the panel. A 250 ms timer lists new hits
  while it runs; Esc (`search.stop`) cancels; ⇧⌘F shows the last results.
* Hits stand for the real files: use `panel.cursor_target()` /
  `selected_paths()` (resolved) for anything that acts on files (F4 editor,
  previews, rename) and check capabilities on those paths, not on the
  results folder (it is read-only: nothing can be created *in* it).
* After file operations hits whose files are gone are dropped (the
  `on_files_changed` hook). Renames must call
  `features::search::relocate(app, &old_to_new)` so the hits follow the
  renamed files (multi-rename and its undo).

## Settings and theme

* `App::new` loads `oc_config::Settings` from the 1.x config dir. Keys written
  (same meaning as 1.x): `left_path`, `right_path` (nearest local folder),
  `show_hidden`, `font_size` (zoom; reset → `null`), `color_scheme` /
  `light_scheme` / `dark_scheme`, `extra.left_mode` / `extra.right_mode`.
  New in 2.x (ignored by 1.x): `extra.left_sort`, `extra.left_sort_reverse`
  (and `right_…`), `extra.window_geometry` (logical `width`, `height`, `x`,
  `y`; 1.x's Qt `geometry` blob is left alone).
* `app.theme` (`Theme`) = scheme colors + `BoxStyle` + `FontConfig` (family,
  style, size, line spacing). `update_settings` re-resolves it when the
  appearance changes and emits `PlatformRequest::ThemeChanged`; the binary
  re-rasterizes the font and resizes the grid. `set_system_dark` follows
  the OS when `follow_system_appearance` is on (winit `ThemeChanged`).
* Color schemes come from `theme::SchemeCache` (app state): built-ins need
  no disk; the user's `color-schemes/*.toml` are read once in the
  background at start (synchronously only when the configured scheme is a
  user one), so zooming or the OS going dark never reads the disk. Use
  `app.color_scheme(name)`, `app.color_schemes()`, and
  `app.refresh_color_schemes(then)` after writing scheme files.
* Settings window: `features/settings/` (⌘,; pages Appearance, Preview,
  Terminal; `scheme_editor.rs` for the color scheme editor). Every control
  writes through `update_settings` with only its own keys; values are read
  back from `app.settings` when drawn. Keys it writes besides the
  appearance ones: `play_next`, `extra["preview.media.autoplay"]`,
  `terminal_shell`, `terminal_link`, `menu_bar_visible`. Features that
  react to them (player, terminal) use `reg.on_settings_changed`.
* Saved folders are not checked at start on the UI thread: the first
  listing gets `LoadOptions::fallback` (left: current folder, right: home)
  and the worker falls back if the saved folder can't be listed, leaving
  the saved setting alone (a network mount may come back).

## File systems and archives

* The UI only uses `VPath` / `FileSystem` capabilities (`writable`,
  `can_trash`, `can_rename`, `watchable`) and `fs.is_local()`.
* Archives: `oc_archive::register()` (called by the binary at start) installs
  `oc_vfs::set_archive_opener` / `set_archive_name_test`. Panels, previews
  (`LoadContext.open_archive` may stay `None`) and searches
  (`Query.archive_opener` may stay `None`) use the hook; tests may still
  inject their own.
* Copy/move: `oc_local::copy` (native local-to-local, streaming otherwise);
  delete/trash/mkdir: `oc_vfs::ops`. `oc_vfs::ops::materialize` uses
  `FileSystem::materialize` first (archives extract into their own cache).
* Folder watching: `panel/watch.rs` (notify), debounced 250 ms, reload keeps
  the cursor; never supersedes a listing still on its way. Watchers are
  started and stopped on a worker (FSEvents starts/joins a thread). A
  watcher reload is skipped when the shown listing started after the
  change (our own reload after F5/F6/F8…), and waits while `app.operation`
  runs (its end reloads both panels).
* Quick search (type a name in a panel): letters within 400 ms extend it
  (Qt's keyboard input interval, as in 1.x).
* Mouse wheel: scrolls the view, not the cursor (like 1.x's Qt views); the
  next cursor key scrolls the cursor back into view.

## Servers: `connections/`

`crates/oc-ui/src/connections/` (registered in `features::ALL`) ports 1.x's
`connections.py`, `connect_dialogs.py` and main_window's connection code.

* Connection types are the `oc_provider` registry. The binary registers them
  at start (`register_connection_types` in `app/src/main.rs`: SFTP, SMB,
  WebDAV, S3 — 1.x's order, which is the editor's Type list); tests call the
  crates' `register_builtin()` themselves.
* A saved connection is `oc_config::Connection` (connections.json, written on
  a worker, re-read so other instances' changes survive). Its *account* is
  `connections::account(&conn)` = `provider.account(params)` (e.g.
  `sftp://me@host:2222`): the session key and the keychain user name under
  service "Orchard Commander" — exactly 1.x's strings. Passwords are read and
  written only on workers (the keychain may prompt) and only with
  "Remember in the system keychain".
* State: `app.state::<connections::Connections>()` — `sessions:
  Arc<Sessions>` (one live `Arc<dyn FileSystem>` per account, shared with
  workers; reconnects when `provider.is_connected` says no), `saved`.
  `connections::use_credential_store(app, store)` swaps the keychain
  (`headless_app` uses `MemoryCredentials`; never the real keychain in tests).
* Connecting runs on a worker with a `ConnectContext` whose `confirm()` asks
  through `tasks.ask_ui` (host keys, certificates; Cancel is the default)
  and whose `data_dir()` is the config dir (known_hosts,
  trusted_certificates.json). `AuthenticationRequired` → password prompt
  (again with "The server didn't accept the password." if one was tried).
* A listing that fails because the session broke (`RemoteError::{Connection,
  AuthenticationRequired}` downcast from `VfsError::Io`, `NotConnected`/reset
  kinds, or `!is_connected`) reconnects once per 10 s and lists again
  (`on_load_error` hook); the other panel on that session follows.
* Passwords follow the accounts: deleting a saved connection keeps the
  password while another saved connection uses the same account; an edit
  that changes the account with "Remember" ticked moves it.
* ⌘K manager, ⇧⌘K disconnect (panels on that server go home), ⌘G accepts
  any registered address (`connections::is_url` / `open_url`), ⌥F1/⌥F2 list
  saved servers and open sessions.

**API for other features** (`connections::api`, agreed with bookmarks — keep
these signatures):

```rust
// {"kind": "server", "account": …, "path": …} → a live location (connecting,
// asking for trust/passwords as needed). None: failed or cancelled (the user
// was told). Doesn't touch the panels.
pub fn resolve_server(app: &mut App, account: &str, path: &str,
                      on_done: impl FnOnce(&mut App, Option<VPath>) + 'static);
// The other way, for oc_config::locations::spec_of:
pub fn spec_context(app: &App) -> ServerAccounts;          // impl SpecContext
pub fn account_of(app: &App, fs: &dyn FileSystem) -> Option<String>;
pub fn is_on_server(app: &App, location: &VPath) -> bool;
```

Also public: `connections::connect_in_panel(app, side, conn, secret)`,
`connections::root_filesystem(&location)` (the server under nested archives).

## The binary

* `orchard-commander [LEFT] [RIGHT]`.
* `--self-test`: renderer (offscreen GPU device), fonts, archive opener on a
  tiny zip, syntect, PDFium (loads and renders a page), FFmpeg (probes and
  decodes a 2 s MP3), config dir, the server connection types (registered in
  order, addresses round-trip) and the system certificate store. Exit 0 when
  all pass. CI runs it.
* `--screenshot OUT.png [--size COLSxROWS] [--scale F] [--keys "Down Space F5"]
  [LEFT] [RIGHT]`: headless app with the user's settings (nothing written),
  waits for listings, presses the keys, renders one frame, writes a PNG and
  prints timings. Use it to look at UI changes without a display. In
  `--keys`, `text:abc` types text and `wait:500` runs timers and background
  output for 500 ms (e.g. `"Ctrl+` wait:3000 text:ls Enter wait:500"`);
  `ORCHARD_COMMANDER_CONFIG_DIR=<empty dir>` gives default settings.
* `OC_TIMING=1` prints the startup phases and first-frame time;
  `OC_TIMING=exit` quits right after the first frame.
* macOS: native menu (muda), Dock icon from `app/resources/icon-macos.png`
  (the bundle's Info.plist icon wins when packaged), window geometry restore.
  Windows/Linux: the in-window strip is the menu bar (F9 or
  `menu_bar_visible`), window icon from `app/resources/icon.png`.

## Testing checklist for a new feature

* `cargo test --workspace` and `cargo clippy --workspace --all-targets` clean.
* App-level test with `testing::headless_app` (temp config dir!): never touch
  the real config dir or keychain from tests.
* Wait for background work with `settle` / `wait_until`, never with sleeps.
* Look at it: `cargo run -p orchard-commander -- --screenshot /tmp/x.png
  --keys "…"` and compare with `../fcmd/site/screenshots/*.webp`.
