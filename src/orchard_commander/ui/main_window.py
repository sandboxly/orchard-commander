"""The main window: two sides (file panel or preview) and a function-key bar."""

from __future__ import annotations

import os
import sys
import threading
from collections.abc import Callable
from dataclasses import dataclass, field, fields
from pathlib import Path

from PySide6.QtCore import QByteArray, QPoint, Qt, QTimer, Signal
from PySide6.QtGui import QAction, QCloseEvent, QKeySequence, QMouseEvent
from PySide6.QtWidgets import (
    QApplication,
    QSplitter,
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

from orchard_commander import APP_NAME, archives, bookmarks, connections, fs, fsproviders, locations, rename, search, terminal, vfs
from orchard_commander.fsproviders import AuthenticationRequired, ConnectContext, ConnectionCancelled
from orchard_commander.config import Settings
from orchard_commander.preview.api import PreviewStore
from orchard_commander.themes import FONT_SIZE_RANGE, Appearance, all_schemes
from orchard_commander.ui import bookmarks_ui, dialogs, rename_ui, search_ui, tasks
from orchard_commander.ui import editor as editor_ui
from orchard_commander.ui.terminal_view import TOGGLE_KEY, TerminalPanel
from orchard_commander.ui.connect_dialogs import ConnectionManager, ask_password, confirm_question
from orchard_commander.ui.operations import FileOperation
from orchard_commander.ui.panel import FilePanel
from orchard_commander.ui.preview_pane import PreviewPane
from orchard_commander.ui.settings_dialog import SettingsDialog
from orchard_commander.ui.style import ThemeManager, system_is_dark

IS_MACOS = sys.platform == "darwin"


def _same_dir(a: Path | None, b: Path | None) -> bool:
    """Same folder, also through symlinks (/tmp is /private/tmp on macOS)."""
    if a is None or b is None:
        return False
    if a == b:
        return True
    try:
        return a.resolve() == b.resolve()
    except OSError:
        return False


def _as_seen_from(folder: Path, panel_dir: Path | None) -> Path:
    """`folder` (a shell's physical path) written relative to the panel's path where possible,
    so a panel in /tmp goes to /tmp/sub, not /private/tmp/sub."""
    if panel_dir is None:
        return folder
    try:
        real = panel_dir.resolve()
        if folder == real or real in folder.parents:
            return panel_dir / folder.relative_to(real)
        for ancestor in panel_dir.parents:
            if ancestor.resolve() == folder:
                return ancestor
    except (OSError, ValueError):
        pass
    return folder


class _SettingsStore(PreviewStore):
    """A previewer's remembered values, kept in the settings file (`extra`)."""

    def __init__(self, settings: Settings, previewer_id: str) -> None:
        super().__init__()
        self._settings = settings
        self._prefix = f"preview.{previewer_id}."

    def get(self, key: str, default=None):
        return self._settings.extra.get(self._prefix + key, default)

    def set(self, key: str, value) -> None:
        if self.get(key) != value:
            self._settings.update(extra={**self._settings.extra, self._prefix + key: value})
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
        self._search_form = search_ui.Form()
        self._rename_rule = rename.Rule()
        self._editors: list[editor_ui.EditorWindow] = []
        self._last_rename: rename.Done | None = None
        self._last_results: search.SearchResultsFS | None = None
        self._search_seen: dict[int, int] = {}
        self._shell_dirs: dict[str, Path] = {}  # each side's shell folder, as last seen
        self._pending_cd: dict[str, Path] = {}  # panel moves to pass on once the shell is idle
        self._commands_seen: dict[str, int] = {}
        self._terminal_sync = QTimer(self, interval=300)
        self._terminal_sync.timeout.connect(self._sync_terminals)
        self._search_timer = QTimer(self, interval=300)
        self._search_timer.timeout.connect(self._refresh_searches)
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
        sides_widget = QWidget()
        sides = QHBoxLayout(sides_widget)
        sides.setContentsMargins(0, 0, 0, 0)
        sides.setSpacing(0)
        sides.addWidget(self.left, 1)
        sides.addWidget(self.right, 1)
        # The terminal spans both sides below them; each side has its own shell in the background.
        self.terminal = TerminalPanel(themes, lambda: terminal.shell_command(self.settings.terminal_shell))
        self.terminal.view.toggle_requested.connect(self.toggle_terminal)
        self.terminal.hide()
        self.splitter = QSplitter(Qt.Orientation.Vertical)
        self.splitter.setChildrenCollapsible(False)
        self.splitter.setHandleWidth(3)
        self.splitter.addWidget(sides_widget)
        self.splitter.addWidget(self.terminal)
        self.splitter.setStretchFactor(0, 1)
        self.splitter.splitterMoved.connect(lambda *_: self._remember_terminal())
        layout.addWidget(self.splitter, 1)
        self.commands = self._build_commands()
        layout.addWidget(self._build_footer())
        self.setCentralWidget(central)
        self.menu_strip = self._build_menu_bars(layout)
        self.toast = Toast(central)

        for side in (self.left, self.right):
            other = self._other(side)
            side.mode_requested.connect(self.set_mode)
            side.panel.message.connect(self.notify)
            side.preview.notice.connect(self.notify)
            side.preview.store_for = self._preview_store
            side.preview.set_continuous(self.settings.play_next_enabled)
            side.preview.finished_playing.connect(lambda path, s=side: self._play_next(s, path))
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
            Command("Edit", "Edit the file under the cursor in the text editor", self.edit_file, ["F4"],
                    ("F4", "Edit")),
            Command("New file", "Create a text file in the active panel and edit it", self.new_text_file,
                    ["Shift+F4"]),
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
            Command("Swap panels", "Exchange the left and right folders",
                    self.swap_panels, [f"{mod}+U"]),
            Command("Same directory", "Show the active folder in the other panel too (Target = Source)",
                    self.same_dir, [f"{mod}+E"]),
            Command("Transfer to left panel", "Left panel: open the folder under the cursor, or match the right panel",
                    lambda: self.transfer(self.left), [f"{mod}+Left"]),
            Command("Transfer to right panel", "Right panel: open the folder under the cursor, or match the left panel",
                    lambda: self.transfer(self.right), [f"{mod}+Right"]),
            Command("Select all", "Mark every file and folder in the active panel",
                    lambda: self.active_panel.set_all_marks(True), [f"{mod}+A"]),
            Command("Clear selection", "Unmark everything in the active panel",
                    lambda: self.active_panel.set_all_marks(False), [f"{mod}+Alt+A"]),
            Command("Terminal", "Show the terminal (one shell per side) and type into it; inside it: hide it",
                    self.toggle_terminal, [TOGGLE_KEY, *(["Ctrl+J"] if IS_MACOS else [])]),  # ⌘J: any layout
            Command("Terminal: go to panel folder", "Change the active side's shell to its panel's folder",
                    self.terminal_to_panel_folder, ["Meta+Shift+`" if IS_MACOS else "Ctrl+Shift+`"]),
            Command("Multi-rename", "Rename marked files with a pattern: [N], [C], dates, search & replace", self.multi_rename, [f"{mod}+M"]),
            Command("Undo rename", "Undo the last multi-rename", self.undo_rename, [f"{mod}+Z"]),
            Command("Find files", "Search by name, contents, size and date",
                    self.find_files, ["Alt+F7", f"{mod}+F"]),
            Command("Stop search", "Stop the running search", self.stop_search, ["Escape"]),
            Command("Last search results", "Show the results of the last search again",
                    self.show_last_results, [f"{mod}+Shift+F"]),
            Command("Bookmarks", "Bookmarked folders: open, add the current folder, organize",
                    self.bookmark_menu, [f"{mod}+D"]),
            Command("Go to bookmark", "Search bookmarks as you type", self.go_to_bookmark, [f"{mod}+Alt+D"]),
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
        add(bar.addMenu("&Files"), "Select all", "Clear selection", None, "Copy", "Move / rename", "Multi-rename",
            "Undo rename", "Make directory", "Move to trash", None, "Find files", "Last search results", None, "Quit")
        add(bar.addMenu("&Commands"),
            "Command palette", None, "Toggle preview", "Next preview view", None,
            "Go to path", "Bookmarks", "Go to bookmark", "Add current folder to bookmarks", "Organize bookmarks",
            None, "Connect to server", "Disconnect", None, "Swap panels", "Same directory", "Reload panels",
            None, "Terminal", "Terminal: go to panel folder")
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
        self.terminal.set_active(new is not None and self.terminal.isAncestorOf(new))
        side = self._side_of(new)
        if side is not None and side is not self._active:
            self._active = side
            self._update_active()

    def _update_active(self) -> None:
        for side in (self.left, self.right):
            side.set_active(side is self._active)
        if self.terminal.isVisible():
            self._show_terminal_session()

    # -- terminal --------------------------------------------------------------

    def _side_folder(self, side: Side) -> Path:
        return side.panel.location.local_anchor() or Path.home()

    def _show_terminal_session(self) -> None:
        """The terminal shows the active side's shell (the other keeps running)."""
        self.terminal.show_side(self._active.name, self._side_folder(self._active))
        if not self._terminal_sync.isActive():
            self._terminal_sync.start()

    def toggle_terminal(self) -> None:
        """Show the terminal and type into it; from inside it, hide it again."""
        if not self.terminal.isVisible():
            self.terminal.show()
            total = sum(self.splitter.sizes())
            height = self.settings.extra.get("terminal_height")
            if not isinstance(height, int) or height < self.terminal.minimumHeight():
                height = total // 3  # first time: a third of the window
            height = min(height, total - self.terminal.minimumHeight())  # leave room for the panels
            self.splitter.setSizes([max(total - height, 1), height])
            self._show_terminal_session()
            self.terminal.view.setFocus()
        elif self.terminal.view.hasFocus():
            self._active.focus_main()  # first: hiding the focused terminal would focus some other side
            self.terminal.hide()
        else:
            self.terminal.view.setFocus()
        self._remember_terminal()

    # Linked folders: a side's shell and panel follow each other (Settings → Terminal).

    def _sync_terminals(self) -> None:
        """Every 300 ms: follow the shells' folders, type pending cds, refresh after commands."""
        if not self.terminal.sessions:
            self._terminal_sync.stop()
            return
        finished_command = False
        for name, session in list(self.terminal.sessions.items()):
            if not session.alive:
                continue
            side = self.left if name == "left" else self.right
            at_prompt = session.at_prompt()
            if session.commands != self._commands_seen.get(name, 0) and at_prompt:
                self._commands_seen[name] = session.commands
                finished_command = True
            if not self.settings.terminal_link_enabled:
                continue
            now = session.current_dir()
            if now is None:
                continue
            seen = self._shell_dirs.get(name)
            if seen is None or not _same_dir(now, seen):  # the shell moved (cd in the terminal)
                self._shell_dirs[name] = now
                panel_dir = self._panel_dir(side)
                if seen is not None and not _same_dir(now, panel_dir) and now.is_dir():
                    self._pending_cd.pop(name, None)
                    side.panel.load(vfs.local(_as_seen_from(now, panel_dir)))
                continue
            pending = self._pending_cd.get(name)
            if pending is not None and at_prompt:  # the panel moved while a program ran: catch up now
                del self._pending_cd[name]
                if not _same_dir(pending, now):
                    session.change_dir(pending)
        if finished_command:  # rm, touch, echo >> …: show the result (watching folders misses some)
            for side in (self.left, self.right):
                if side.panel.location.is_local:
                    side.panel.reload()

    @staticmethod
    def _panel_dir(side: Side) -> Path | None:
        location = side.panel.location
        return Path(location.path) if location.is_local else None

    def _panel_moved(self, side: Side, location: vfs.VPath) -> None:
        """The panel went somewhere: take that side's shell along (when it's idle at its prompt)."""
        session = self.terminal.sessions.get(side.name)
        if not self.settings.terminal_link_enabled or session is None or not session.alive:
            return
        if not location.is_local:
            return  # servers, archives, search results: the shell stays where it is
        folder = Path(location.path)
        if _same_dir(folder, self._shell_dirs.get(side.name)):
            return  # already there (e.g. the shell moved the panel)
        if session.at_prompt():
            self._pending_cd.pop(side.name, None)
            session.change_dir(folder)
        else:
            self._pending_cd[side.name] = folder  # don't type into vim or a running build

    def terminal_to_panel_folder(self) -> None:
        """Change the active side's shell to its panel's folder."""
        if not self.terminal.isVisible():
            self.toggle_terminal()
        self.terminal.run_in(self._active.name, self._side_folder(self._active))
        self.terminal.view.setFocus()

    def _remember_terminal(self) -> None:
        changes = {"terminal_visible": self.terminal.isVisible()}
        if self.terminal.isVisible():
            changes["terminal_height"] = self.splitter.sizes()[1]
        if any(self.settings.extra.get(k) != v for k, v in changes.items()):
            self.settings.update(extra={**self.settings.extra, **changes})

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
        self._panel_moved(side, location)
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

    # -- text editor (Total Commander's F4) ------------------------------------------

    def edit_file(self) -> None:
        panel = self.active_panel
        target = panel.cursor_target
        if target is None or panel.cursor_name == "..":
            return
        entry = panel._entry(panel.cursor_name)
        if entry is not None and entry.is_dir:
            self.notify("F4 edits files; this is a folder", "warning")
            return
        if entry is not None and entry.size > editor_ui.ASK_ABOVE_BYTES and not dialogs.confirm(
            self, self.themes, "Large file",
            f"{target.name} is {fs.human_size(entry.size)}. Open it in the editor anyway?", "Open",
        ):
            return
        self.open_editor(target)

    def open_editor(self, location: vfs.VPath) -> editor_ui.EditorWindow:
        for existing in self._editors:  # one window per file
            if existing.location == location:
                existing.show()
                existing.raise_()
                existing.activateWindow()
                return existing
        window = editor_ui.EditorWindow(self.themes, location)
        self._editors.append(window)
        window.closed.connect(lambda w: self._editors.remove(w) if w in self._editors else None)
        window.saved.connect(lambda _: self.reload())
        if not location.fs.writable:
            self.notify(f"{location.fs.read_only_reason}: the file opens read-only", "warning")
        window.show()
        window.raise_()
        window.activateWindow()
        return window

    def new_text_file(self) -> None:
        panel = self.active_panel
        folder = panel.location
        if self._refuse(folder, "create a file here", ok=folder.fs.writable):
            return
        name = dialogs.ask_text(self, self.themes, "New file", f"Create in {folder.display}:", "", "Create")
        if not name:
            return

        def created(location: vfs.VPath) -> None:
            panel.load(folder, select=location.name)
            self.open_editor(location)

        def failed(exc: BaseException) -> None:
            reason = "a file with this name already exists" if isinstance(exc, FileExistsError) else str(exc)
            self.notify(f"Can't create {name}: {reason}", "warning")

        tasks.run_in_background(lambda: editor_ui.new_file(folder, name), created, failed, owner=self)

    # -- multi-rename (Total Commander's Ctrl+M) ---------------------------------

    def multi_rename(self) -> None:
        panel = self.active_panel
        entries = panel.selected_entries()
        if not entries:
            return
        # The real files: search hits are renamed where they are.
        paths = panel.selected_paths()
        for path in paths:
            if not path.fs.writable:
                self._refuse(path, "rename here", ok=False)
                return
            if not path.fs.can_rename:
                self.notify(f"Can't rename on {path.fs.type_name}", "warning")
                return
        items = [rename.item_for(p, e) for p, e in zip(paths, entries)]

        def ask(exists) -> None:
            dialog = rename_ui.MultiRenameDialog(self, self.themes, items, self._rename_rule, exists)
            answer = dialog.ask()
            self._rename_rule = dialog.rule  # remembered for this session, even when cancelled
            if answer is None:
                return
            plan, _ = answer
            tasks.run_in_background(lambda: rename.execute(plan), lambda done: self._renamed(done, panel),
                                    owner=self)

        tasks.run_in_background(lambda: rename.existing_names(items), ask, owner=self)

    def _renamed(self, done: rename.Done, panel: FilePanel, *, undo: bool = False) -> None:
        for _, results in self._shown_results():
            results.relocate(rename.renamed_locations(done))
        if self._last_results is not None:
            self._last_results.relocate(rename.renamed_locations(done))
        self._last_rename = None if undo else done
        panel.clear_marks()
        self.reload()
        n = len(done.renames)
        what = f"{'Undid renaming' if undo else 'Renamed'} {n} item{'s' * (n != 1)}"
        if done.errors:
            self.notify(f"{what}; {len(done.errors)} failed: {done.errors[0]}", "warning")
        else:
            self.notify(what + ("" if undo else " · ⌘Z to undo" if IS_MACOS else " · Ctrl+Z to undo"))

    def undo_rename(self) -> None:
        done = self._last_rename
        if done is None or not done.renames:
            self.notify("Nothing to undo", "warning")
            return
        n = len(done.renames)
        if not dialogs.confirm(self, self.themes, "Undo rename",
                               f"Give {n} item{'s' * (n != 1)} their old names back?", "Undo"):
            return
        panel = self.active_panel
        tasks.run_in_background(lambda: rename.undo(done), lambda undone: self._renamed(undone, panel, undo=True),
                                owner=self)

    # -- find files (Total Commander's Alt+F7 + "Feed to listbox") -----------------

    def find_files(self) -> None:
        side = self._side_of_location()
        here = side.panel.location
        if isinstance(here.fs, search.SearchResultsFS):  # searching from results: search where they came from
            here = here.fs.exit_to() or vfs.local(Path.home())
        answer = search_ui.FindFilesDialog(self, self.themes, here, self._search_form).ask()
        if answer is None:
            return
        query, self._search_form = answer
        self.start_search(query, side)

    def start_search(self, query: search.Query, side: Side) -> search.SearchResultsFS:
        """Search in the background; the hits appear in `side` as a folder while it runs."""
        results = search.SearchResultsFS(query)
        results.cancel_requested = False  # type: ignore[attr-defined]
        self._last_results = results
        if side.mode == PREVIEW:
            self.set_mode(side, FILES)
        side.panel.load(vfs.VPath(results, ""))
        side.panel.setFocus()

        def run() -> None:
            try:
                search.run(query, results.add, progress=results.progress,
                           is_cancelled=lambda: results.cancel_requested)  # type: ignore[attr-defined]
            except Exception as exc:  # report, don't lose the hits found so far
                results.progress.errors.append(str(exc))
            finally:
                results.finish(cancelled=results.cancel_requested)  # type: ignore[attr-defined]

        threading.Thread(target=run, daemon=True, name="search").start()
        self._search_timer.start()
        return results

    def _shown_results(self) -> list[tuple[Side, search.SearchResultsFS]]:
        return [(s, s.panel.location.fs) for s in (self.left, self.right)
                if isinstance(s.panel.location.fs, search.SearchResultsFS)]

    def _refresh_searches(self) -> None:
        """While searches run, show new hits and progress (every 300 ms)."""
        active = False
        for side, results in self._shown_results():
            seen = self._search_seen.get(id(results))
            if results.running:
                active = True
            if results.changes != seen or results.running:
                self._search_seen[id(results)] = results.changes
                if results.changes != seen:
                    side.panel.reload()
                else:
                    side.panel._update_subtitle()  # progress counters
            if not results.running and seen is not None and not getattr(results, "announced", False):
                results.announced = True  # type: ignore[attr-defined]
                found = len(results.hits)
                self.notify(f"Search {'stopped' if results.cancelled else 'finished'}: "
                            f"{found:,} hit{'s' * (found != 1)}")
        if self._last_results is not None and self._last_results.running:
            active = True
        if not active:
            self._search_timer.stop()

    def stop_search(self) -> None:
        for _, results in self._shown_results():
            if results.running:
                results.cancel_requested = True  # type: ignore[attr-defined]
        if self._last_results is not None and self._last_results.running:
            self._last_results.cancel_requested = True  # type: ignore[attr-defined]

    def show_last_results(self) -> None:
        if self._last_results is None:
            self.notify("No search yet: Alt+F7 or ⌘F to find files", "warning")
            return
        side = self._side_of_location()
        if side.mode == PREVIEW:
            self.set_mode(side, FILES)
        side.panel.load(vfs.VPath(self._last_results, ""))
        side.panel.setFocus()
        if self._last_results.running:
            self._search_timer.start()

    def _prune_search_results_then_reload(self) -> None:
        """After file operations: forget hits that were moved or deleted, then reload."""
        shown = self._shown_results()
        if not shown:
            self.reload()
            return
        tasks.run_in_background(
            lambda: [results.prune() for _, results in shown],
            lambda _: self.reload(),
            lambda exc: self.reload(),
            owner=self,
        )

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
        """Search all bookmarks as you type (Ctrl+Alt+D, ⌥⌘D)."""
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
        if "play_next" in changes:
            for side in (self.left, self.right):
                side.preview.set_continuous(self.settings.play_next_enabled)
        if "extra" in changes:  # e.g. autoplay, also the players' own toggle
            for side in (self.left, self.right):
                side.preview.store_changed()

    def _preview_store(self, previewer_id: str) -> PreviewStore:
        return _SettingsStore(self.settings, previewer_id)

    def _play_next(self, side: Side, finished: vfs.VPath) -> None:
        """A file in `side`'s preview ended: select the next one it plays in the other panel, and play it."""
        panel = self._other(side).panel
        if panel.cursor_target != finished or side.mode != PREVIEW:
            return  # the user moved on meanwhile
        for row in range(panel.table.current_row() + 1, panel.model.rowCount()):
            entry = panel.model.entry_at(row)
            if entry is not None and side.preview.plays(entry):
                panel.table.set_current_row(row)
                if panel.cursor_target is not None:
                    side.preview.play_when_shown(panel.cursor_target)
                return

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
        # Ask the real files (search hits stand for files elsewhere), not the folder they're listed in.
        sources = source.selected_paths()
        blocked = next((p for p in sources if not p.fs.writable), None)
        if kind == "move" and blocked is not None and self._refuse(blocked, "move from here", ok=False):
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
                    self._run(FileOperation(kind, sources, target), source)
                    return
                path = (source.location.local_anchor() or Path.home()) / path
            target = vfs.local(path)
        self._run(FileOperation(kind, sources, target), source)

    def trash(self) -> None:
        """Move to the trash, or — where there is none, e.g. on servers — delete permanently."""
        panel = self.active_panel
        entries = panel.selected_entries()
        if not entries:
            return
        # Ask the real files (search hits stand for files elsewhere), not the folder they're listed in.
        paths = panel.selected_paths()
        blocked = next((p for p in paths if not (p.fs.can_trash or p.fs.writable)), None)
        if blocked is not None and self._refuse(blocked, "delete here", ok=False):
            return
        what = self._describe(entries)
        if all(p.fs.can_trash for p in paths):
            if dialogs.confirm(self, self.themes, "Move to trash", f"Move {what} to the trash?", "Trash"):
                self._run(FileOperation("trash", paths), panel)
        else:
            no_trash = next(p for p in paths if not p.fs.can_trash).fs.type_name
            if dialogs.confirm(
                self, self.themes, "Delete permanently",
                f"{no_trash} has no trash.\n\nDelete {what} permanently? This can't be undone.", "Delete",
            ):
                self._run(FileOperation("delete", paths), panel)

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
            self._prune_search_results_then_reload()
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
        if extra.get("terminal_visible"):
            QTimer.singleShot(0, self._reopen_terminal)

    def _reopen_terminal(self) -> None:
        self.toggle_terminal()  # shows it and focuses it…
        self._active.focus_main()  # …but the app starts in the panels
    def closeEvent(self, event: QCloseEvent) -> None:
        for editor in list(self._editors):  # unsaved edits: each editor asks (and may cancel quitting)
            if not editor.close():
                event.ignore()
                return
        if self._operation is not None and self._operation.isRunning():
            self._operation.cancel_requested = True
            self._operation.wait(3000)
        geometry = bytes(self.saveGeometry().toBase64().data()).decode()
        self.settings.update(extra={**self.settings.extra, "geometry": geometry})
        for side in (self.left, self.right):
            side.preview.deactivate()
        self.sessions.close_all()
        self.terminal.close_all()
        super().closeEvent(event)
