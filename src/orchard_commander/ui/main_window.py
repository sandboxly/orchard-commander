"""The main window: two sides (file panel or preview) and a function-key bar."""

from __future__ import annotations

import os
import sys
from collections.abc import Callable
from dataclasses import dataclass, field, fields
from pathlib import Path

from PySide6.QtCore import QByteArray, QPoint, Qt, QTimer, Signal
from PySide6.QtGui import QAction, QCloseEvent, QKeySequence, QMouseEvent
from PySide6.QtWidgets import (
    QApplication,
    QButtonGroup,
    QHBoxLayout,
    QLabel,
    QMainWindow,
    QMenu,
    QMenuBar,
    QStackedWidget,
    QToolButton,
    QVBoxLayout,
    QWidget,
)

from orchard_commander import APP_NAME, archives, bookmarks, connections, fs, fsproviders, locations, vfs
from orchard_commander.fsproviders import AuthenticationRequired, ConnectContext, ConnectionCancelled
from orchard_commander.config import Settings
from orchard_commander.themes import FONT_SIZE_RANGE, Appearance, all_schemes
from orchard_commander.ui import bookmarks_ui, dialogs, tasks
from orchard_commander.ui.connect_dialogs import ConnectionManager, ask_password, confirm_question
from orchard_commander.ui.operations import FileOperation
from orchard_commander.ui.panel import FilePanel
from orchard_commander.ui.preview_pane import PreviewPane
from orchard_commander.ui.settings_dialog import SettingsDialog
from orchard_commander.ui.style import ThemeManager, system_is_dark

IS_MACOS = sys.platform == "darwin"
FILES, PREVIEW = "files", "preview"


# ---------------------------------------------------------------------------
# Building blocks
# ---------------------------------------------------------------------------


class Side(QWidget):
    """One half of the window: mode tabs above a file panel or a preview."""

    mode_requested = Signal(object, str)  # side, mode

    def __init__(self, name: str, themes: ThemeManager, location: vfs.VPath, show_hidden: bool) -> None:
        super().__init__()
        self.name = name
        self.setObjectName("side")
        self.setProperty("active", False)
        self.panel = FilePanel(themes, location, show_hidden=show_hidden)
        self.preview = PreviewPane(themes)
        self.preview.show_hidden = show_hidden

        tabs = QWidget()
        row = QHBoxLayout(tabs)
        cw = self.fontMetrics().horizontalAdvance("M")
        row.setContentsMargins(cw, 0, cw, 0)
        row.setSpacing(cw)
        self.tab_group = QButtonGroup(self)
        self.tabs: dict[str, QToolButton] = {}
        for mode, label in ((FILES, "Files"), (PREVIEW, "Preview")):
            b = QToolButton()
            b.setText(f" {label} ")
            b.setObjectName("modeTab")
            b.setCheckable(True)
            b.setFocusPolicy(Qt.FocusPolicy.NoFocus)
            b.setCursor(Qt.CursorShape.PointingHandCursor)
            b.clicked.connect(lambda _=False, m=mode: self.mode_requested.emit(self, m))
            self.tab_group.addButton(b)
            self.tabs[mode] = b
            row.addWidget(b)
        row.addStretch(1)
        self.tabs[FILES].setChecked(True)

        self.stack = QStackedWidget()
        self.stack.addWidget(self.panel)
        self.stack.addWidget(self.preview)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)
        layout.addWidget(tabs)
        layout.addWidget(self.stack, 1)

    @property
    def mode(self) -> str:
        return PREVIEW if self.stack.currentWidget() is self.preview else FILES

    def set_mode(self, mode: str) -> None:
        self.tabs[mode].setChecked(True)
        if mode == PREVIEW:
            self.stack.setCurrentWidget(self.preview)
        else:
            self.preview.deactivate()
            self.stack.setCurrentWidget(self.panel)

    def focus_main(self) -> None:
        (self.preview if self.mode == PREVIEW else self.panel).setFocus()

    def set_active(self, active: bool) -> None:
        self.setProperty("active", active)
        self.style().unpolish(self)
        self.style().polish(self)
        for b in self.tabs.values():
            b.style().unpolish(b)
            b.style().polish(b)
        self.panel.set_active(active and self.mode == FILES)
        self.preview.set_active(active and self.mode == PREVIEW)


class FKey(QLabel):
    """A function-key bar entry: highlighted key followed by its label."""

    def __init__(self, themes: ThemeManager, key: str, label: str, slot: Callable[[], None]) -> None:
        super().__init__()
        self.setObjectName("fkey")
        self.themes = themes
        self.key, self.label = key, label
        self.slot = slot
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        themes.changed.connect(self._restyle)
        self._restyle()

    def _restyle(self) -> None:
        t = self.themes.colors
        self.setText(f'<span style="color:{t.footer_key}; font-weight:bold">{self.key}</span>&nbsp;{self.label}')

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton:
            self.slot()


class Toast(QLabel):
    def __init__(self, parent: QWidget) -> None:
        super().__init__(parent)
        self.setObjectName("toast")
        self.setWordWrap(True)
        self.hide()
        self._timer = QTimer(self, singleShot=True)
        self._timer.timeout.connect(self.hide)

    def show_message(self, text: str, severity: str = "info") -> None:
        self.setProperty("severity", severity)
        self.style().unpolish(self)
        self.style().polish(self)
        self.setText(text)
        parent = self.parentWidget()
        assert parent is not None
        cw = self.fontMetrics().horizontalAdvance("M")
        self.setFixedWidth(min(cw * 60, parent.width() - 4 * cw))
        self.adjustSize()
        self.move(parent.width() - self.width() - 2 * cw, parent.height() - self.height() - 3 * cw)
        self.raise_()
        self.show()
        self._timer.start(8000 if severity == "error" else 3500)


@dataclass
class Command:
    title: str
    help: str
    slot: Callable[[], None]
    shortcuts: list[str] = field(default_factory=list)
    fkey: tuple[str, str] | None = None  # (key label, bar label)
    role: QAction.MenuRole = QAction.MenuRole.NoRole  # macOS: Settings and Quit go to the app menu


# ---------------------------------------------------------------------------
# Main window
# ---------------------------------------------------------------------------


class MainWindow(QMainWindow):
    def __init__(
        self, settings: Settings, themes: ThemeManager, left: Path | None = None, right: Path | None = None
    ) -> None:
        super().__init__()
        tasks.init()
        self.settings = settings
        self.themes = themes
        self.sessions = connections.Sessions()
        self.bookmark_items: list[bookmarks.Item] = bookmarks.load()
        self.setWindowTitle(APP_NAME)
        self._operation: FileOperation | None = None

        left_path = left or self._saved_path(settings.left_path) or Path.cwd()
        right_path = right or self._saved_path(settings.right_path) or Path.home()
        self.left = Side("left", themes, vfs.local(left_path), settings.hidden_visible)
        self.right = Side("right", themes, vfs.local(right_path), settings.hidden_visible)
        self._active = self.left

        central = QWidget()
        layout = QVBoxLayout(central)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)
        sides = QHBoxLayout()
        sides.setSpacing(0)
        sides.addWidget(self.left, 1)
        sides.addWidget(self.right, 1)
        layout.addLayout(sides, 1)
        self.commands = self._build_commands()
        layout.addWidget(self._build_footer())
        self.setCentralWidget(central)
        self.menu_strip = self._build_menu_bars(layout)
        self.toast = Toast(central)

        for side in (self.left, self.right):
            other = self._other(side)
            side.mode_requested.connect(self.set_mode)
            side.panel.message.connect(self.notify)
            side.panel.title_clicked.connect(lambda pos, s=side: self.bookmark_menu(s, pos))
            side.panel.location_changed.connect(lambda loc, s=side: self._location_changed(s, loc))
            side.panel.cursor_changed.connect(lambda target, o=other: self._cursor_changed(o, target))
        QApplication.instance().focusChanged.connect(self._focus_changed)  # type: ignore[union-attr]

        for side in (self.left, self.right):
            side.panel.load(side.panel.location)
        self._restore_state()
        themes.changed.connect(self._update_active)

    @staticmethod
    def _saved_path(value: str) -> Path | None:
        return Path(value) if value and Path(value).is_dir() else None

    # -- commands ----------------------------------------------------------

    def _build_commands(self) -> list[Command]:
        mod = "Ctrl"  # Qt maps Ctrl to ⌘ on macOS
        hidden_keys = [f"{mod}+Shift+."] + ([] if IS_MACOS else [f"{mod}+H"])
        commands = [
            Command("Toggle preview", "Preview the file under the cursor in the other panel",
                    self.toggle_preview, ["F3"], ("F3", "Preview")),
            Command("Next preview view", "Show the previewed file another way (e.g. as text or hex)",
                    self.cycle_preview_view, ["Shift+F3"]),
            Command("Copy", "Copy marked files to the other panel", self.copy, ["F5"], ("F5", "Copy")),
            Command("Move / rename", "Move or rename marked files", self.move, ["F6"], ("F6", "Move")),
            Command("Make directory", "Create a directory", self.mkdir, ["F7"], ("F7", "MkDir")),
            Command("Move to trash", "Move marked files to the trash", self.trash,
                    ["F8", "Delete", f"{mod}+Backspace"], ("F8", "Trash")),
            Command("Menu", "Open the menu bar", self.show_menu, ["F9"], ("F9", "Menu")),
            Command("Quit", f"Quit {APP_NAME}", self.close, ["F10", f"{mod}+Q"], ("F10", "Quit"),
                    QAction.MenuRole.QuitRole),
            Command("Settings…", "Appearance and other settings", self.open_settings, [f"{mod}+,"],
                    role=QAction.MenuRole.PreferencesRole),
            Command("Choose color scheme…", "Switch the color scheme, previewing as you move",
                    self.choose_color_scheme),
            Command("Command palette", "Search all commands", self.command_palette, [f"{mod}+P", f"{mod}+Shift+P"]),
            Command("Reload panels", "Re-read both directories", self.reload, [f"{mod}+R"]),
            Command("Toggle hidden files", "Show or hide hidden files", self.toggle_hidden, hidden_keys),
            Command("Swap panels", "Exchange the left and right folders (Total Commander: Ctrl+U)",
                    self.swap_panels, [f"{mod}+U"]),
            Command("Same directory", "Show the active folder in the other panel too (Target = Source)",
                    self.same_dir, [f"{mod}+E"]),
            Command("Transfer to left panel", "Left panel: open the folder under the cursor, or match the right panel",
                    lambda: self.transfer(self.left), [f"{mod}+Left"]),
            Command("Transfer to right panel", "Right panel: open the folder under the cursor, or match the left panel",
                    lambda: self.transfer(self.right), [f"{mod}+Right"]),
            Command("Bookmarks", "Bookmarked folders: open, add the current folder, organize",
                    self.bookmark_menu, [f"{mod}+D"]),
            Command("Go to bookmark", "Search bookmarks as you type", self.go_to_bookmark, [f"{mod}+Shift+D"]),
            Command("Add current folder to bookmarks", "Bookmark the active panel's folder", self.add_bookmark),
            Command("Organize bookmarks", "Reorder, group, edit and delete bookmarks", self.organize_bookmarks),
            Command("Go to path", "Type a directory or a server address", self.goto_path,
                    [f"{mod}+G", f"{mod}+Shift+G"]),
            Command("Connect to server", "Saved SFTP and SMB connections", self.connection_manager, [f"{mod}+K"]),
            Command("Disconnect", "Close the connection of the active panel's server", self.disconnect,
                    [f"{mod}+Shift+K"]),
            Command("Left drive / location", "Pick a drive or volume for the left panel",
                    lambda: self.choose_location(self.left), ["Alt+F1"]),
            Command("Right drive / location", "Pick a drive or volume for the right panel",
                    lambda: self.choose_location(self.right), ["Alt+F2"]),
            Command("Zoom in", "Larger font", lambda: self.zoom(+1), [f"{mod}+=", f"{mod}++"]),
            Command("Zoom out", "Smaller font", lambda: self.zoom(-1), [f"{mod}+-"]),
            Command("Reset zoom", "Default font size", lambda: self.zoom(0), [f"{mod}+0"]),
        ]
        self.actions_by_title: dict[str, QAction] = {}
        for cmd in commands:
            action = QAction(cmd.title, self)
            action.setShortcuts([QKeySequence(k) for k in cmd.shortcuts])
            action.setShortcutContext(Qt.ShortcutContext.WindowShortcut)
            action.setMenuRole(cmd.role)
            action.triggered.connect(cmd.slot)
            self.addAction(action)
            self.actions_by_title[cmd.title] = action
        return commands

    # -- menu bar --------------------------------------------------------------

    def _build_menu_bars(self, layout: QVBoxLayout) -> QMenuBar:
        """Norton / FAR style menus: Left, Files, Commands, Options, Right.

        Where there is a global menu bar (macOS, KDE with a global menu) the
        menus go there, and a second, in-window copy is the "strip". Elsewhere
        the window's own menu bar is the strip. The strip is hidden unless the
        user wants it always shown; F9 shows it and opens a menu, Norton-style.
        """
        bar = self.menuBar()
        self._fill_menu_bar(bar)
        if bar.isNativeMenuBar():
            strip = QMenuBar()
            strip.setNativeMenuBar(False)
            self._fill_menu_bar(strip)
            layout.insertWidget(0, strip)
        else:
            strip = bar
        for menu in strip.findChildren(QMenu):
            if menu.parent() is strip:
                menu.aboutToHide.connect(self._menu_closed)
        strip.setVisible(bool(self.settings.menu_bar_visible))
        return strip

    def _fill_menu_bar(self, bar: QMenuBar) -> None:
        def add(menu: QMenu, *titles: str | None) -> None:
            for title in titles:
                if title is None:
                    menu.addSeparator()
                else:
                    menu.addAction(self.actions_by_title[title])

        def side_menu(side: Side, title: str, location: str, transfer: str) -> None:
            menu = bar.addMenu(title)
            files = menu.addAction("Files", lambda: self.set_mode(side, FILES))
            preview = menu.addAction("Preview", lambda: self.set_mode(side, PREVIEW))
            for action in (files, preview):
                action.setCheckable(True)
            menu.aboutToShow.connect(lambda: (files.setChecked(side.mode == FILES),
                                              preview.setChecked(side.mode == PREVIEW)))
            menu.addSeparator()
            add(menu, location, transfer)

        side_menu(self.left, "&Left", "Left drive / location", "Transfer to left panel")
        add(bar.addMenu("&Files"), "Copy", "Move / rename", "Make directory", "Move to trash", None, "Quit")
        add(bar.addMenu("&Commands"),
            "Command palette", None, "Toggle preview", "Next preview view", None,
            "Go to path", "Bookmarks", "Go to bookmark", "Add current folder to bookmarks", "Organize bookmarks",
            None, "Connect to server", "Disconnect", None, "Swap panels", "Same directory", "Reload panels")
        add(bar.addMenu("&Options"),
            "Settings…", "Choose color scheme…", None, "Toggle hidden files", None,
            "Zoom in", "Zoom out", "Reset zoom")
        side_menu(self.right, "&Right", "Right drive / location", "Transfer to right panel")

    def show_menu(self) -> None:
        """F9: show the in-window menu bar and open the active side's menu."""
        strip = self.menu_strip
        strip.show()
        index = 0 if self._active is self.left else -1
        QTimer.singleShot(0, lambda: strip.setActiveAction(strip.actions()[index]))

    def _menu_closed(self) -> None:
        def hide_unless_open() -> None:
            if self.settings.menu_bar_visible or QApplication.activePopupWidget() is not None:
                return
            self.menu_strip.hide()
            if QApplication.activeModalWidget() is None:
                self._active.focus_main()

        QTimer.singleShot(0, hide_unless_open)  # moving to the next menu closes this one first

    def _build_footer(self) -> QWidget:
        footer = QWidget()
        footer.setObjectName("footer")
        row = QHBoxLayout(footer)
        cw = self.fontMetrics().horizontalAdvance("M")
        row.setContentsMargins(cw, 2, cw, 2)
        row.setSpacing(2 * cw)
        for cmd in self.commands:
            if cmd.fkey:
                row.addWidget(FKey(self.themes, *cmd.fkey, cmd.slot))
        row.addStretch(1)
        palette_key = QKeySequence("Ctrl+P").toString(QKeySequence.SequenceFormat.NativeText)
        connect_key = QKeySequence("Ctrl+K").toString(QKeySequence.SequenceFormat.NativeText)
        row.addWidget(FKey(self.themes, connect_key, "Connect", self.connection_manager))
        row.addWidget(FKey(self.themes, palette_key, "Commands", self.command_palette))
        return footer

    # -- sides, focus, preview ----------------------------------------------

    def _other(self, side: Side) -> Side:
        return self.right if side is self.left else self.left

    def _side_of(self, widget: QWidget | None) -> Side | None:
        while widget is not None:
            if isinstance(widget, Side):
                return widget
            widget = widget.parentWidget()
        return None

    def _focus_changed(self, _old: QWidget | None, new: QWidget | None) -> None:
        side = self._side_of(new)
        if side is not None and side is not self._active:
            self._active = side
            self._update_active()

    def _update_active(self) -> None:
        for side in (self.left, self.right):
            side.set_active(side is self._active)

    @property
    def active_panel(self) -> FilePanel:
        """The panel file operations act on: the focused side, unless it's a preview."""
        side = self._active if self._active.mode == FILES else self._other(self._active)
        return side.panel

    @property
    def other_panel(self) -> FilePanel:
        return self.right.panel if self.active_panel is self.left.panel else self.left.panel

    def focusNextPrevChild(self, next: bool) -> bool:
        """Tab / Shift+Tab switches between the two sides."""
        self._other(self._active).focus_main()
        return True

    def set_mode(self, side: Side, mode: str) -> None:
        other = self._other(side)
        if mode == PREVIEW:
            if other.mode == PREVIEW:
                other.set_mode(FILES)
            side.set_mode(PREVIEW)
            side.preview.show_path(other.panel.cursor_target)
            other.focus_main()
            self._active = other
        else:
            side.set_mode(FILES)
            side.focus_main()
            self._active = side
        self._update_active()
        self._save_modes()

    def toggle_preview(self) -> None:
        """Quick view: preview the active panel's file in the other side, or close it."""
        target = self._other(self._active) if self._active.mode == FILES else self._active
        self.set_mode(target, FILES if target.mode == PREVIEW else PREVIEW)

    def cycle_preview_view(self) -> None:
        for side in (self.left, self.right):
            if side.mode == PREVIEW:
                side.preview.cycle_view()

    def _cursor_changed(self, other: Side, target: vfs.VPath | None) -> None:
        if other.mode == PREVIEW:
            other.preview.show_path(target)

    def _location_changed(self, side: Side, location: vfs.VPath) -> None:
        # Persist the nearest real directory; places inside archives or on servers aren't restored.
        anchor = location.local_anchor()
        if anchor is not None:
            self.settings.update(**{f"{side.name}_path": str(anchor)})

    # -- simple commands -----------------------------------------------------

    def notify(self, text: str, severity: str = "info") -> None:
        self.toast.show_message(text, severity)

    def reload(self) -> None:
        for side in (self.left, self.right):
            side.panel.reload()
            side.preview.refresh()

    def toggle_hidden(self) -> None:
        show = not self.settings.hidden_visible
        self.settings.update(show_hidden=show)
        for side in (self.left, self.right):
            side.panel.show_hidden = show
            side.preview.show_hidden = show
            side.panel.reload()
        self.notify("Showing hidden files" if show else "Hiding hidden files")

    def swap_panels(self) -> None:
        a, b = self.left.panel.location, self.right.panel.location
        self.left.panel.load(b)
        self.right.panel.load(a)

    def same_dir(self) -> None:
        self.other_panel.load(self.active_panel.location)

    def transfer(self, destination: Side) -> None:
        """Total Commander's Ctrl+Left / Ctrl+Right (cm_TransferLeft / cm_TransferRight).

        From the other side: `destination` opens the folder (or archive) under the
        cursor there — or that side's current folder if the cursor is on a file.
        From `destination` itself: it shows the other side's folder.
        """
        source_side = self._side_of_location()
        if destination.mode == PREVIEW:
            self.set_mode(destination, FILES)
        if source_side is destination:
            destination.panel.load(self._other(destination).panel.location)
            return
        source = source_side.panel
        name = source.cursor_name
        entry = source._entry(name) if name else None
        if entry is not None and entry.is_dir:
            destination.panel.load(source.location.child(entry.name))
        elif entry is not None and archives.is_archive(entry.name):
            destination.panel.enter_archive(source.location.child(entry.name))
        else:  # a file, "..", or nothing: the folder itself
            destination.panel.load(source.location)

    def goto_path(self) -> None:
        panel = self.active_panel
        value = dialogs.ask_text(
            self, self.themes, "Go to", f"Directory, or server address ({fsproviders.address_examples()}):",
            panel.location.display, "Go",
        )
        if not value:
            return
        if connections.is_url(value):
            try:
                conn = connections.Connection.from_url(value)
            except ValueError as exc:
                self.notify(str(exc), "error")
                return
            self.open_connection(conn, self._side_of_panel(panel))
            return
        panel.load(vfs.local(os.path.expanduser(value)))

    def _side_of_panel(self, panel: FilePanel) -> Side:
        return self.left if panel is self.left.panel else self.right

    def choose_location(self, side: Side) -> None:
        items: list[tuple[str, str, object]] = [(label, str(path), path) for label, path in fs.locations()]
        items += [(f"⇄ {c.name or c.description}", c.url, c) for c in connections.load_saved()]
        items += [(f"★ {bookmarks.plain(b.title)}", locations.describe(b.location), b)
                  for _, b in bookmarks.bookmarks(self.bookmark_items)]
        choice = dialogs.Picker(self, self.themes, "Go to", items).choose()
        if isinstance(choice, bookmarks.Bookmark):
            self.open_bookmark(choice, side)
        elif isinstance(choice, connections.Connection):
            self.open_connection(choice, side)
        elif isinstance(choice, Path):
            if side.mode == PREVIEW:
                self.set_mode(side, FILES)
            side.panel.load(vfs.local(choice))
            side.panel.setFocus()

    # -- bookmarks (Total Commander's Directory Hotlist) ----------------------------

    def _side_of_location(self) -> Side:
        """The side bookmark commands act on: the active one, or the other if it previews."""
        return self._active if self._active.mode == FILES else self._other(self._active)

    def location_spec(self, side: Side) -> locations.Spec | None:
        return locations.spec_of(side.panel.location, self.sessions.owner_of)

    def bookmark_menu(self, side: Side | None = None, pos: QPoint | None = None) -> None:
        """Ctrl+D, or a click on a panel's path."""
        side = side or self._side_of_location()
        menu = bookmarks_ui.build_menu(
            self,
            self.bookmark_items,
            on_open=lambda b: self.open_bookmark(b, side),
            current=self.location_spec(side),
            on_add=lambda: self.add_bookmark(side),
            on_remove=self.remove_bookmark,
            on_organize=self.organize_bookmarks,
        )
        menu.exec(pos or side.panel.title_anchor())

    def add_bookmark(self, side: Side | None = None) -> None:
        side = side or self._side_of_location()
        location = self.location_spec(side)
        if location is None:
            self.notify("This location can't be bookmarked", "warning")
            return
        target = self.location_spec(self._other(side))
        dialog = bookmarks_ui.AddBookmarkDialog(self, self.themes, self.bookmark_items, location, target)
        answer = dialog.ask()
        if answer is None:
            return
        title, folder_id, with_target = answer
        bookmarks.container_of(self.bookmark_items, folder_id).append(
            bookmarks.Bookmark(title, location, target if with_target else None)
        )
        self._save_bookmarks()
        self.notify(f"Bookmarked {locations.describe(location)}")

    def remove_bookmark(self, bookmark: bookmarks.Bookmark) -> None:
        if bookmarks.remove(self.bookmark_items, bookmark.id):
            self._save_bookmarks()
            self.notify(f"Removed bookmark {bookmarks.plain(bookmark.title)}")

    def organize_bookmarks(self) -> None:
        side = self._side_of_location()
        organizer = bookmarks_ui.BookmarkOrganizer(
            self, self.themes, self.bookmark_items, self.location_spec(side), self.location_spec(self._other(side))
        )
        items = organizer.organize()
        if items is not None:
            self.bookmark_items = items
            self._save_bookmarks()

    def go_to_bookmark(self) -> None:
        """Search all bookmarks as you type (Ctrl+Shift+D)."""
        entries = bookmarks.bookmarks(self.bookmark_items)
        if not entries:
            self.notify("No bookmarks yet: add one with Ctrl+D → Add current folder", "warning")
            return
        items = [(" › ".join((*trail, bookmarks.plain(b.title))), locations.describe(b.location), b)
                 for trail, b in entries]
        chosen = dialogs.Picker(self, self.themes, "Go to bookmark", items, width_chars=90).choose()
        if isinstance(chosen, bookmarks.Bookmark):
            self.open_bookmark(chosen, self._side_of_location())

    def _save_bookmarks(self) -> None:
        try:
            bookmarks.save(self.bookmark_items)
        except OSError as exc:
            self.notify(f"Could not save bookmarks: {exc}", "error")

    def open_bookmark(self, bookmark: bookmarks.Bookmark, side: Side) -> None:
        """Open the bookmark's folder in `side`, and its other folder (if any) in the other side."""
        self.navigate(bookmark.location, side)
        if bookmark.target is not None:
            self.navigate(bookmark.target, self._other(side), focus=False)

    def navigate(self, spec: locations.Spec, side: Side, *, focus: bool = True) -> None:
        """Show the place described by `spec` in `side` (connecting/opening archives as needed)."""
        if side.mode == PREVIEW:
            self.set_mode(side, FILES)

        def arrived(location: vfs.VPath) -> None:
            side.panel.load(location)
            if focus:
                side.panel.setFocus()

        self.resolve(spec, side, arrived)

    def resolve(self, spec: locations.Spec, side: Side, then: Callable[[vfs.VPath], None]) -> None:
        """Turn a spec back into a live location (asynchronously), then call `then`."""
        if not locations.is_valid(spec):
            self.notify("This bookmark is damaged and can't be opened", "error")
            return
        kind = spec["kind"]
        if kind == "local":
            then(vfs.local(spec["path"]))
        elif kind == "server":
            account = spec["account"]
            saved = next((c for c in connections.load_saved() if c.account == account), None)
            try:
                conn = saved or connections.Connection.from_url(account)
            except ValueError as exc:
                self.notify(str(exc), "error")
                return
            self.open_connection(conn.with_params(path=spec["path"]), side, then=then)
        else:  # archive: first the archive file, then its contents

            def open_archive(container: vfs.VPath) -> None:
                tasks.run_in_background(
                    lambda: vfs.open_archive(container),
                    lambda root: then(vfs.VPath(root.fs, spec["path"])),
                    lambda exc: self.notify(f"Cannot open {container.name}: {exc}", "error"),
                    owner=self,
                )

            self.resolve(spec["archive"], side, open_archive)

    # -- connections -------------------------------------------------------------

    def connection_manager(self) -> None:
        picked = ConnectionManager(self, self.themes).choose()
        if picked is not None:
            conn, creds = picked
            self.open_connection(
                conn, self._side_of_panel(self.active_panel), creds.password if creds else None
            )

    def open_connection(self, conn: connections.Connection, side: Side, password: str | None = None,
                        *, remember: bool = False, then: Callable[[vfs.VPath], None] | None = None) -> None:
        """Connect in the background (asking for host-key trust/password as needed),
        then browse the start folder — or hand the location to `then`."""
        if side.mode == PREVIEW and then is None:
            self.set_mode(side, FILES)
        password = password or connections.get_password(conn)
        self.notify(f"Connecting to {conn.label}…")

        ctx = ConnectContext(
            data_dir=connections.data_dir(),
            confirm=lambda title, message, ok: tasks.ask_gui(
                lambda: confirm_question(self, self.themes, title, message, ok)
            ),
        )

        def connected(location: vfs.VPath) -> None:
            if remember and password:
                connections.store_password(conn, password)
            self.toast.hide()
            if then is not None:
                then(location)
                return
            side.panel.load(location)
            side.panel.setFocus()

        def failed(exc: BaseException) -> None:
            if isinstance(exc, AuthenticationRequired):
                reason = "The server didn't accept the password." if password else ""
                creds = ask_password(self, self.themes, conn, reason)
                if creds is not None and creds.password:
                    self.open_connection(conn, side, creds.password, remember=creds.remember, then=then)
                return
            severity = "warning" if isinstance(exc, ConnectionCancelled) else "error"
            self.notify(str(exc), severity)

        tasks.run_in_background(
            lambda: self.sessions.connect(conn, password, ctx), connected, failed, owner=self
        )

    def disconnect(self) -> None:
        server = connections.root_filesystem(self.active_panel.location)
        if self.sessions.owner_of(server) is None:
            self.notify("The active panel isn't on a server", "warning")
            return
        for side in (self.left, self.right):
            if connections.root_filesystem(side.panel.location) is server:
                side.panel.load(vfs.local(Path.home()))
            if side.preview.path is not None and connections.root_filesystem(side.preview.path) is server:
                side.preview.show_path(None)
        tasks.run_in_background(lambda: self.sessions.disconnect(server), lambda _: self.notify("Disconnected"),
                                lambda exc: None, owner=self)

    # -- settings --------------------------------------------------------------

    def open_settings(self) -> None:
        SettingsDialog(self, self.themes, self.settings, self.apply_settings).exec()

    def apply_settings(self, **changes: object) -> None:
        """Save setting changes and bring the app up to date."""
        self.settings.update(**changes)
        if changes.keys() & {f.name for f in fields(Appearance)}:
            self.themes.set_appearance(self.settings.appearance())
        if "menu_bar_visible" in changes:
            self.menu_strip.setVisible(bool(self.settings.menu_bar_visible))

    def choose_color_scheme(self) -> None:
        """Pick a scheme for the current mode (light or dark, when following the system)."""
        schemes = all_schemes()
        slot = self.themes.appearance.scheme_setting(system_is_dark())
        current = getattr(self.themes.appearance, slot)
        items = [(name, "dark" if s.dark else "light", name) for name, s in schemes.items()]
        picker = dialogs.Picker(
            self, self.themes, "Color scheme", items, current=current,
            on_highlight=lambda n: self.themes.preview_colors(schemes[str(n)]),
        )
        chosen = picker.choose()
        self.themes.preview_colors(None)
        if chosen is not None:
            self.apply_settings(**{slot: str(chosen)})

    def command_palette(self) -> None:
        items = []
        for cmd in self.commands:
            keys = ", ".join(QKeySequence(k).toString(QKeySequence.SequenceFormat.NativeText) for k in cmd.shortcuts)
            items.append((cmd.title, keys or cmd.help, cmd.slot))
        slot = dialogs.Picker(self, self.themes, "Commands", items, width_chars=72).choose()
        if callable(slot):
            QTimer.singleShot(0, slot)

    def zoom(self, delta: int) -> None:
        lo, hi = FONT_SIZE_RANGE
        self.apply_settings(font_size=None if delta == 0 else max(lo, min(self.themes.font_size + delta, hi)))

    def _refuse(self, location: vfs.VPath, what: str, *, ok: bool) -> bool:
        """Tell the user (and return True) if `location`'s file system can't do `what`."""
        if not ok:
            self.notify(f"Can't {what}: {location.fs.read_only_reason}", "warning")
        return not ok

    def mkdir(self) -> None:
        panel = self.active_panel
        here = panel.location
        if self._refuse(here, "create a directory here", ok=here.fs.writable):
            return
        name = dialogs.ask_text(self, self.themes, "Make directory", f"Create in {here.display}:", "", "Create")
        if not name:
            return

        def failed(exc: BaseException) -> None:
            reason = exc.strerror if isinstance(exc, OSError) and exc.strerror else exc
            self.notify(f"Cannot create directory: {reason}", "error")

        tasks.run_in_background(
            lambda: vfs.mkdir(here, name),
            lambda _: panel.load(here, select=name.replace("\\", "/").split("/")[0]),
            failed,
            owner=self,
        )

    # -- file operations -----------------------------------------------------

    @staticmethod
    def _describe(entries: list[fs.Entry]) -> str:
        if len(entries) == 1:
            e = entries[0]
            return f"'{e.name}'" + (" (directory)" if e.is_dir else "")
        ndirs = sum(1 for e in entries if e.is_dir)
        nfiles = len(entries) - ndirs
        parts = []
        if nfiles:
            parts.append(f"{nfiles} file{'s' if nfiles != 1 else ''}")
        if ndirs:
            parts.append(f"{ndirs} director{'ies' if ndirs != 1 else 'y'}")
        return " and ".join(parts)

    def copy(self) -> None:
        self._transfer("copy")

    def move(self) -> None:
        self._transfer("move")

    def _transfer(self, kind: str) -> None:
        source = self.active_panel
        entries = source.selected_entries()
        if not entries:
            return
        destination = self.other_panel.location
        if kind == "move" and self._refuse(source.location, "move from here", ok=source.location.fs.writable):
            return
        if self._refuse(destination, "copy there", ok=destination.fs.writable):
            return
        verb = "Copy" if kind == "copy" else "Move"
        default = destination.display
        if len(entries) == 1 and destination == source.location:
            default = entries[0].name  # same directory on both sides: suggest a rename
        value = dialogs.ask_text(self, self.themes, verb, f"{verb} {self._describe(entries)} to:", default, verb)
        if not value:
            return
        if value == destination.display:
            target = destination
        else:  # typed: absolute, or relative to where the source is
            path = Path(os.path.expanduser(value))
            if not path.is_absolute():
                if source.location.fs.writable:  # e.g. a new name in the same directory
                    target = source.location.child(value)
                    self._run(FileOperation(kind, source.selected_paths(), target), source)
                    return
                path = (source.location.local_anchor() or Path.home()) / path
            target = vfs.local(path)
        self._run(FileOperation(kind, source.selected_paths(), target), source)

    def trash(self) -> None:
        """Move to the trash, or — where there is none, e.g. on servers — delete permanently."""
        panel = self.active_panel
        filesystem = panel.location.fs
        if self._refuse(panel.location, "delete here", ok=filesystem.can_trash or filesystem.writable):
            return
        entries = panel.selected_entries()
        if not entries:
            return
        what = self._describe(entries)
        if filesystem.can_trash:
            if dialogs.confirm(self, self.themes, "Move to trash", f"Move {what} to the trash?", "Trash"):
                self._run(FileOperation("trash", panel.selected_paths()), panel)
        elif dialogs.confirm(
            self, self.themes, "Delete permanently",
            f"{filesystem.type_name} has no trash.\n\nDelete {what} permanently? This can't be undone.", "Delete",
        ):
            self._run(FileOperation("delete", panel.selected_paths()), panel)

    def _run(self, op: FileOperation, source: FilePanel) -> None:
        if self._operation is not None and self._operation.isRunning():
            self.notify("Another file operation is still running", "warning")
            return
        self._operation = op
        progress = None
        if op.kind not in ("trash", "delete"):
            progress = dialogs.ProgressDialog(self, self.themes, op.title)
            progress.cancel_requested.connect(lambda: setattr(op, "cancel_requested", True))
            op.progress.connect(progress.update_progress)
            progress.open()
        parent = progress or self
        op.conflict.connect(
            lambda src, dst: op.answer_conflict(*dialogs.ask_conflict(parent, self.themes, src, dst))
        )
        op.succeeded.connect(lambda msg: self.notify(msg))
        op.cancelled.connect(lambda msg: self.notify(msg, "warning"))
        op.failed.connect(lambda msg: self.notify(msg, "error"))

        def finished() -> None:
            if progress is not None:
                progress.done(0)
            source.clear_marks()
            self.reload()
            source.setFocus()

        op.finished.connect(finished)
        op.start()

    # -- persistence ---------------------------------------------------------

    def _save_modes(self) -> None:
        modes = {"left_mode": self.left.mode, "right_mode": self.right.mode}
        self.settings.update(extra={**self.settings.extra, **modes})

    def _restore_state(self) -> None:
        extra = self.settings.extra
        geometry = extra.get("geometry")
        if geometry:
            self.restoreGeometry(QByteArray.fromBase64(geometry.encode()))
        else:
            self.resize(1280, 800)
        for side in (self.left, self.right):
            if extra.get(f"{side.name}_mode") == PREVIEW:
                QTimer.singleShot(0, lambda s=side: self.set_mode(s, PREVIEW))
        QTimer.singleShot(0, self.left.panel.setFocus)

    def closeEvent(self, event: QCloseEvent) -> None:
        if self._operation is not None and self._operation.isRunning():
            self._operation.cancel_requested = True
            self._operation.wait(3000)
        geometry = bytes(self.saveGeometry().toBase64().data()).decode()
        self.settings.update(extra={**self.settings.extra, "geometry": geometry})
        for side in (self.left, self.right):
            side.preview.deactivate()
        self.sessions.close_all()
        super().closeEvent(event)
