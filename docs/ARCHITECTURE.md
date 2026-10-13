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
* The same registry feeds: key dispatch, the native macOS menu bar (only ⌘
  shortcuts become native key equivalents, so plain keys stay with the
  window), the in-window strip (F9), the F-key bar (`.fkey`), the right end
  of the bar (`.footer_right("Commands")`) and the command palette (⌘P,
  fuzzy filter; `.hidden_from_palette()` to leave one out).
* While a dialog is open, a native menu item is delivered to the dialog as
  its key (so ⌘A selects text in a field). `.global()` commands (Quit) run anyway.

## Adding a feature module (the usual case)

1. Create `crates/oc-ui/src/features/<name>.rs` with
   `pub fn register(reg: &mut Registry)`.
2. Add `pub mod <name>;` and one line `<name>::register,` to `ALL` in
   `features/mod.rs`. That is the only shared edit.
3. In `register`: `reg.add(Command…)`, `reg.add_side_view(factory)`,
   `reg.on_start(fn)`, `reg.on_settings_changed(fn)` as needed, and:
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
}
```

* Switch with `main_window::set_mode(app, side, SideMode::View(i))` or
  `toggle_view(app, side, "preview")`; modes persist as `left_mode` /
  `right_mode` like 1.x.
* A view that follows the other panel gets `target_changed` whenever that
  panel's cursor moves (and when it is shown). Load content in the
  background (`app.tasks`), keep a request counter or `CancelToken` to drop
  stale results, call `app.redraw()` when done.
* `features/preview_stub.rs` is the placeholder Preview: replace its body
  with `oc_preview::Registry` (the archive hook is already registered, see
  below). Images: upload via the window's `GridRenderer` — the binary owns
  renderers, so add a `PlatformRequest` / image cache hook when the preview
  agent needs it (planned: `App` keeps decoded RGBA by key, the platform
  uploads and hands back `ImageId`s).
* A bottom pane (the terminal) has space reserved: `app.main.bottom_rows`
  (`Layout::bottom`). Draw/route it from your feature (add the call in
  `main_window::draw` / `handle_event` — one line each).

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
    fn as_any(&self) -> &dyn Any;  fn as_any_mut(&mut self) -> &mut dyn Any;
}
app.window_mut::<EditorWindow>(app.focused)   // from a command acting on the focused window
```

Dialogs can be pushed on any window (`app.push_modal(id, …)`). Quitting asks
every window first. An editor needs its own native menu (File/Edit) on macOS:
extend `app/src/macos.rs` to switch menus on focus (planned, not built).

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
| `cover(f, rect, ctx)` | call before drawing anything that floats over other content |

Colors come only from `ctx.theme.colors` (the scheme's roles: `surface`,
`border_active`, `cursor_bg`, `footer_key`, …). Never hard-code colors.

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
  the OS when `follow_system_appearance` is on.

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
  started and stopped on a worker (FSEvents starts/joins a thread).
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
  tiny zip, syntect, config dir, the server connection types (registered in
  order, addresses round-trip) and the system certificate store. Exit 0 when
  all pass. CI runs it.
* `--screenshot OUT.png [--size COLSxROWS] [--scale F] [--keys "Down Space F5"]
  [LEFT] [RIGHT]`: headless app with the user's settings (nothing written),
  waits for listings, presses the keys, renders one frame, writes a PNG and
  prints timings. Use it to look at UI changes without a display.
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
