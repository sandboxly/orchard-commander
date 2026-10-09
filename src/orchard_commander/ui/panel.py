"""A file panel: one directory listing with a cursor and marked files."""

from __future__ import annotations

import datetime as dt
import os
from pathlib import Path

from PySide6.QtCore import (
    QAbstractTableModel,
    QFileSystemWatcher,
    QModelIndex,
    QPersistentModelIndex,
    QRect,
    Qt,
    QTimer,
    Signal,
)
from PySide6.QtGui import QColor, QFont, QKeyEvent, QMouseEvent, QPainter
from PySide6.QtWidgets import (
    QAbstractItemView,
    QHeaderView,
    QStyle,
    QStyledItemDelegate,
    QStyleOptionViewItem,
    QTableView,
)

from orchard_commander import archives, fs, vfs
from orchard_commander.ui import tasks
from orchard_commander.ui.boxframe import BoxFrame
from orchard_commander.ui.style import ThemeManager

PARENT = ".."
DATE_FORMAT = "%Y-%m-%d %H:%M"
COLUMNS = [(fs.SortKey.NAME, "Name"), (fs.SortKey.SIZE, "Size"), (fs.SortKey.MTIME, "Modified")]
COL_NAME, COL_SIZE, COL_MTIME = range(3)
EntryRole = Qt.ItemDataRole.UserRole + 1

type Index = QModelIndex | QPersistentModelIndex


class FileModel(QAbstractTableModel):
    def __init__(self) -> None:
        super().__init__()
        self.entries: list[fs.Entry] = []
        self.has_parent = False
        self.marked: set[str] = set()
        self.sort_key = fs.SortKey.NAME
        self.sort_reverse = False

    def set_entries(self, entries: list[fs.Entry], has_parent: bool) -> None:
        self.beginResetModel()
        self.entries = entries
        self.has_parent = has_parent
        self.marked &= {e.name for e in entries}
        self.endResetModel()

    def entry_at(self, row: int) -> fs.Entry | None:
        """The entry for `row`, or None for the '..' row."""
        if self.has_parent:
            row -= 1
        return self.entries[row] if 0 <= row < len(self.entries) else None

    def name_at(self, row: int) -> str | None:
        if self.has_parent and row == 0:
            return PARENT
        entry = self.entry_at(row)
        return entry.name if entry else None

    def row_of(self, name: str) -> int | None:
        if name == PARENT:
            return 0 if self.has_parent else None
        for i, e in enumerate(self.entries):
            if e.name == name:
                return i + (1 if self.has_parent else 0)
        return None

    def refresh_rows(self) -> None:
        if self.rowCount():
            self.dataChanged.emit(self.index(0, 0), self.index(self.rowCount() - 1, len(COLUMNS) - 1))

    # -- Qt model API ------------------------------------------------------

    def rowCount(self, parent: Index = QModelIndex()) -> int:
        return 0 if parent.isValid() else len(self.entries) + (1 if self.has_parent else 0)

    def columnCount(self, parent: Index = QModelIndex()) -> int:
        return 0 if parent.isValid() else len(COLUMNS)

    def headerData(self, section: int, orientation: Qt.Orientation, role: int = Qt.ItemDataRole.DisplayRole):
        if orientation != Qt.Orientation.Horizontal or role != Qt.ItemDataRole.DisplayRole:
            if role == Qt.ItemDataRole.TextAlignmentRole and section == COL_SIZE:
                return Qt.AlignmentFlag.AlignRight | Qt.AlignmentFlag.AlignVCenter
            return None
        key, label = COLUMNS[section]
        if key == self.sort_key:
            label += " ▼" if self.sort_reverse else " ▲"
        return label

    def data(self, index: Index, role: int = Qt.ItemDataRole.DisplayRole):
        if not index.isValid():
            return None
        entry = self.entry_at(index.row())
        col = index.column()
        if role == Qt.ItemDataRole.DisplayRole:
            if entry is None:
                return {COL_NAME: PARENT, COL_SIZE: "<UP>"}.get(col, "")
            if col == COL_NAME:
                return entry.name + (os.sep if entry.is_dir else "")
            if col == COL_SIZE:
                if entry.is_dir:
                    return "<DIR>"
                cloud = "☁ " if entry.is_online_only else ""  # placeholder: data still in the cloud
                return cloud + fs.human_size(entry.size)
            if col == COL_MTIME:
                return dt.datetime.fromtimestamp(entry.mtime).strftime(DATE_FORMAT) if entry.mtime else ""
        if role == Qt.ItemDataRole.TextAlignmentRole and col == COL_SIZE:
            return Qt.AlignmentFlag.AlignRight | Qt.AlignmentFlag.AlignVCenter
        if role == EntryRole:
            return entry
        return None


class RowDelegate(QStyledItemDelegate):
    """Paints rows terminal-style: colored text, a solid cursor bar, mark glyph."""

    def __init__(self, view: FileTable) -> None:
        super().__init__(view)
        self.view = view

    def paint(self, painter: QPainter, option: QStyleOptionViewItem, index: Index) -> None:
        t = self.view.themes.colors
        model = self.view.file_model
        entry: fs.Entry | None = index.data(EntryRole)
        marked = entry is not None and entry.name in model.marked
        is_cursor = bool(option.state & QStyle.StateFlag.State_Selected)  # type: ignore[operator]

        if entry is None or entry.is_dir:
            color = t.dir
        elif entry.is_link:
            color = t.link
        elif entry.is_executable:
            color = t.exec
        elif entry.is_hidden:
            color = t.hidden
        else:
            color = t.foreground
        if marked:
            color = t.marked

        rect: QRect = option.rect  # type: ignore[assignment]
        painter.save()
        if is_cursor:
            focused = self.view.hasFocus()
            painter.fillRect(rect, QColor(t.cursor_bg if focused else t.cursor_inactive_bg))
            if focused:
                color = t.cursor_fg
        font = QFont(self.view.themes.font)
        font.setBold(entry is None or entry.is_dir or marked)
        font.setItalic(entry is not None and entry.is_link)
        painter.setFont(font)
        painter.setPen(QColor(color))

        cw = painter.fontMetrics().horizontalAdvance("M")
        text = index.data(Qt.ItemDataRole.DisplayRole) or ""
        if index.column() == COL_NAME:
            text = ("✓" if marked else " ") + text
        inner = rect.adjusted(cw // 2, 0, -cw // 2, 0)
        align = index.data(Qt.ItemDataRole.TextAlignmentRole) or (
            Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter
        )
        elided = painter.fontMetrics().elidedText(text, Qt.TextElideMode.ElideRight, inner.width())
        painter.drawText(inner, align, elided)
        painter.restore()


class FileTable(QTableView):
    """Table with commander keys. Navigation keys are Qt's own; type to jump."""

    activated_name = Signal(str)
    open_external = Signal(str)
    go_parent = Signal()
    toggle_mark = Signal()
    mark_all = Signal(bool)
    invert_marks = Signal()

    def __init__(self, themes: ThemeManager, model: FileModel) -> None:
        super().__init__()
        self.themes = themes
        self.file_model = model
        self.setModel(model)
        self.setItemDelegate(RowDelegate(self))
        self.setShowGrid(False)
        self.setWordWrap(False)
        self.setTabKeyNavigation(False)
        self.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        self.setSelectionMode(QAbstractItemView.SelectionMode.SingleSelection)
        self.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        self.setVerticalScrollMode(QAbstractItemView.ScrollMode.ScrollPerItem)
        self.verticalHeader().hide()
        header = self.horizontalHeader()
        header.setSectionsClickable(True)
        header.setHighlightSections(False)
        header.setSectionResizeMode(COL_NAME, QHeaderView.ResizeMode.Stretch)
        header.setSectionResizeMode(COL_SIZE, QHeaderView.ResizeMode.Fixed)
        header.setSectionResizeMode(COL_MTIME, QHeaderView.ResizeMode.Fixed)
        header.setDefaultAlignment(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter)
        self.doubleClicked.connect(self._double_clicked)
        themes.changed.connect(self._restyle)
        self._restyle()

    def _restyle(self) -> None:
        fm = self.fontMetrics()
        cw = fm.horizontalAdvance("M")
        self.verticalHeader().setDefaultSectionSize(self.themes.row_height() + 2)
        self.verticalHeader().setMinimumSectionSize(fm.height())
        self.horizontalHeader().setFixedHeight(fm.height() + 2)
        self.horizontalHeader().resizeSection(COL_SIZE, cw * 11)
        self.horizontalHeader().resizeSection(COL_MTIME, cw * 18)
        self.viewport().update()

    def resizeEvent(self, event) -> None:
        super().resizeEvent(event)
        # Drop the date column when the panel gets narrow.
        cw = self.fontMetrics().horizontalAdvance("M")
        self.setColumnHidden(COL_MTIME, self.viewport().width() < cw * 52)

    def focusInEvent(self, event) -> None:
        super().focusInEvent(event)
        self.viewport().update()

    def focusOutEvent(self, event) -> None:
        super().focusOutEvent(event)
        self.viewport().update()

    def current_row(self) -> int:
        return self.currentIndex().row() if self.currentIndex().isValid() else -1

    def set_current_row(self, row: int) -> None:
        if self.file_model.rowCount() == 0:
            return
        row = max(0, min(row, self.file_model.rowCount() - 1))
        self.setCurrentIndex(self.file_model.index(row, 0))
        self.scrollTo(self.file_model.index(row, 0))

    def _double_clicked(self, index: QModelIndex) -> None:
        name = self.file_model.name_at(index.row())
        if name:
            self.activated_name.emit(name)

    def mousePressEvent(self, event: QMouseEvent) -> None:
        mods = event.modifiers()
        if event.button() == Qt.MouseButton.LeftButton and mods & (
            Qt.KeyboardModifier.ControlModifier | Qt.KeyboardModifier.MetaModifier
        ):
            index = self.indexAt(event.position().toPoint())
            if index.isValid():
                self.setCurrentIndex(index)
                self.toggle_mark.emit()
                self.set_current_row(index.row())  # undo the auto-advance for clicks
            return
        if event.button() == Qt.MouseButton.RightButton:
            index = self.indexAt(event.position().toPoint())
            if index.isValid():
                self.setCurrentIndex(index)
                self.toggle_mark.emit()
                self.set_current_row(index.row())
            return
        super().mousePressEvent(event)

    def keyPressEvent(self, event: QKeyEvent) -> None:
        key = event.key()
        mods = event.modifiers() & ~Qt.KeyboardModifier.KeypadModifier
        plain = mods == Qt.KeyboardModifier.NoModifier
        if key in (Qt.Key.Key_Return, Qt.Key.Key_Enter):
            name = self.file_model.name_at(self.current_row())
            if name and plain:
                self.activated_name.emit(name)
            elif name and mods & (Qt.KeyboardModifier.ShiftModifier | Qt.KeyboardModifier.ControlModifier):
                self.open_external.emit(name)  # open archives/folders with the OS instead
        elif key == Qt.Key.Key_Backspace and plain:
            self.go_parent.emit()
        elif key in (Qt.Key.Key_Insert, Qt.Key.Key_Space) and plain:
            self.toggle_mark.emit()
        elif event.text() == "+" and not mods & Qt.KeyboardModifier.ControlModifier:
            self.mark_all.emit(True)
        elif event.text() == "-" and not mods & Qt.KeyboardModifier.ControlModifier:
            self.mark_all.emit(False)
        elif event.text() == "*":
            self.invert_marks.emit()
        else:
            super().keyPressEvent(event)  # arrows, paging, type-to-jump


class FilePanel(BoxFrame):
    """Lists one directory on any file system (see `vfs`)."""

    location_changed = Signal(object)  # vfs.VPath
    cursor_changed = Signal(object)  # vfs.VPath | None
    message = Signal(str, str)  # text, severity

    def __init__(self, themes: ThemeManager, location: vfs.VPath, *, show_hidden: bool = False) -> None:
        super().__init__(themes)
        self.location = location
        self.show_hidden = show_hidden
        self.model = FileModel()
        self.table = FileTable(themes, self.model)
        self.body.addWidget(self.table)
        self.setFocusProxy(self.table)

        t = self.table
        t.activated_name.connect(self.activate)
        t.open_external.connect(self.open_external)
        t.go_parent.connect(self.go_parent)
        t.toggle_mark.connect(self.toggle_mark)
        t.mark_all.connect(self.set_all_marks)
        t.invert_marks.connect(self.invert_marks)
        t.horizontalHeader().sectionClicked.connect(self._header_clicked)
        t.selectionModel().currentRowChanged.connect(self._current_changed)

        # Auto-refresh when a watchable directory changes on disk (debounced).
        self._watcher = QFileSystemWatcher(self)
        self._watcher.directoryChanged.connect(lambda _: self._reload_timer.start())
        self._reload_timer = QTimer(self, singleShot=True, interval=250)
        self._reload_timer.timeout.connect(self._directory_changed)

        # Listing runs in the background; `_request` discards stale results.
        self._request = 0
        self._completed = 0
        self._jobs = 0  # other background work (opening archives/files)
        self._free: int | None = None
        self._loading_hint = QTimer(self, singleShot=True, interval=200)
        self._loading_hint.timeout.connect(lambda: self.set_subtitle("loading…"))

    @property
    def loading(self) -> bool:
        return self._completed != self._request or self._jobs > 0

    def _background(self, fn, on_done, on_error) -> None:
        """Run `fn` in the background, counting it towards `loading`."""
        self._jobs += 1

        def done(result: object) -> None:
            self._jobs -= 1
            on_done(result)

        def failed(exc: BaseException) -> None:
            self._jobs -= 1
            on_error(exc)

        tasks.run_in_background(fn, done, failed, owner=self)

    # -- navigation --------------------------------------------------------

    def load(self, location: vfs.VPath, select: str | None = None, *, walk_up: bool = False) -> None:
        """Show directory `location` once it has been listed (in the background).

        The current listing stays until then; on error it is kept and a message
        shown. With `walk_up`, the nearest existing ancestor is shown instead of
        failing (for directories that vanished).
        """
        self._request += 1
        request = self._request
        options = {"show_hidden": self.show_hidden, "sort_key": self.model.sort_key, "reverse": self.model.sort_reverse}

        def job() -> tuple[vfs.VPath, list[fs.Entry], bool, int | None]:
            loc: vfs.VPath | None = location
            if walk_up:
                while loc is not None and not loc.exists():
                    loc = loc.parent()
                if loc is None:
                    raise FileNotFoundError(location.display)
            assert loc is not None
            entries = loc.list_dir(**options)
            return loc, entries, loc.parent() is not None, loc.fs.free_space(loc.path)

        self._loading_hint.start()
        tasks.run_in_background(
            job,
            lambda result: self._listed(request, select, *result),
            lambda exc: self._list_failed(request, location, exc),
            owner=self,
        )

    def _listed(
        self, request: int, select: str | None, location: vfs.VPath, entries: list[fs.Entry],
        has_parent: bool, free: int | None,
    ) -> None:
        if request != self._request:
            return  # superseded by a newer load
        self._completed = request
        self._loading_hint.stop()
        changed = location != self.location
        if changed:
            self.model.marked.clear()
        old_row = self.table.current_row()
        self.location = location
        self._free = free
        self.model.set_entries(entries, has_parent)
        row = self.model.row_of(select) if select is not None else None
        self.table.set_current_row(row if row is not None else (old_row if not changed and old_row > 0 else 0))

        if self._watcher.directories():
            self._watcher.removePaths(self._watcher.directories())
        if location.fs.watchable and (local := location.local_path()) is not None:
            self._watcher.addPath(str(local))
        self.set_title(self._display_path())
        self._update_subtitle()
        if changed:
            self.location_changed.emit(location)
        self.cursor_changed.emit(self.cursor_target)

    def _list_failed(self, request: int, location: vfs.VPath, exc: BaseException) -> None:
        if request != self._request:
            return
        self._completed = request
        self._loading_hint.stop()
        self._update_subtitle()
        reason = exc.strerror if isinstance(exc, OSError) and exc.strerror else exc
        self.message.emit(f"Cannot open {location.display}: {reason}", "error")

    def _directory_changed(self) -> None:
        # Never let a refresh of the folder being left supersede a listing still on
        # its way (e.g. from a server): with a busy folder, it would never arrive.
        if self._completed == self._request:
            self.reload()

    def reload(self) -> None:
        """Re-read the current directory, keeping cursor and marks."""
        self.load(self.location, select=self.cursor_name, walk_up=True)

    def go_parent(self) -> None:
        parent = self.location.parent()
        if parent is not None:
            self.load(parent, select=self.location.name)

    def _target(self, name: str) -> vfs.VPath:
        return self.location.parent() or self.location if name == PARENT else self.location.child(name)

    def activate(self, name: str) -> None:
        """Enter a directory or archive, or open a file with its default application."""
        if name == PARENT:
            self.go_parent()
            return
        target = self.location.child(name)
        entry = self._entry(name)
        if entry is not None and entry.is_dir:
            self.load(target)
        elif archives.is_archive(name):
            self.enter_archive(target)
        else:
            self.open_external(name)

    def enter_archive(self, file: vfs.VPath) -> None:
        self.set_subtitle(f"opening {file.name}…")
        self._background(
            lambda: vfs.open_archive(file),
            self.load,
            lambda exc: (self._update_subtitle(), self.message.emit(str(exc), "error")),
        )

    def open_external(self, name: str) -> None:
        """Open with the OS default application (folders open in the file manager).

        Files not on the local disk are fetched to a temp cache first.
        """
        target = self._target(name)
        entry = self._entry(name)
        if (entry is not None and entry.is_dir) or name == PARENT:
            local = target.local_path()
            if local is None:
                self.message.emit("Copy the folder somewhere local (F5) to open it", "warning")
                return
            self._open_local(name, local)
            return
        if not target.is_local:
            self.message.emit(f"Fetching {name}…", "info")
        self._background(
            target.materialize,
            lambda local: self._open_local(name, local),
            lambda exc: self.message.emit(f"Cannot open {name}: {exc}", "error"),
        )

    def _open_local(self, name: str, local: Path) -> None:
        try:
            fs.open_with_default_app(local)
        except OSError as exc:
            self.message.emit(f"Cannot open {name}: {exc}", "error")

    def _header_clicked(self, section: int) -> None:
        key = COLUMNS[section][0]
        if key == self.model.sort_key:
            self.model.sort_reverse = not self.model.sort_reverse
        else:
            self.model.sort_key, self.model.sort_reverse = key, False
        self.reload()

    def _current_changed(self) -> None:
        self.cursor_changed.emit(self.cursor_target)

    # -- marks -------------------------------------------------------------

    def toggle_mark(self) -> None:
        name = self.cursor_name
        if name and name != PARENT:
            self.model.marked ^= {name}
            self.model.refresh_rows()
            self._update_subtitle()
        self.table.set_current_row(self.table.current_row() + 1)

    def set_all_marks(self, value: bool) -> None:
        self.model.marked = {e.name for e in self.model.entries} if value else set()
        self.model.refresh_rows()
        self._update_subtitle()

    def invert_marks(self) -> None:
        self.model.marked = {e.name for e in self.model.entries} - self.model.marked
        self.model.refresh_rows()
        self._update_subtitle()

    def clear_marks(self) -> None:
        self.set_all_marks(False)

    # -- queries -----------------------------------------------------------

    @property
    def entries(self) -> list[fs.Entry]:
        return self.model.entries

    @property
    def marked(self) -> set[str]:
        return self.model.marked

    @property
    def cursor_name(self) -> str | None:
        return self.model.name_at(self.table.current_row())

    def _entry(self, name: str) -> fs.Entry | None:
        row = self.model.row_of(name)
        return self.model.entry_at(row) if row is not None else None

    @property
    def cursor_target(self) -> vfs.VPath | None:
        """What the cursor is on, e.g. for a preview."""
        name = self.cursor_name
        return self._target(name) if name is not None else None

    def selected_entries(self) -> list[fs.Entry]:
        """Marked entries, or the entry under the cursor if nothing is marked."""
        if self.model.marked:
            return [e for e in self.model.entries if e.name in self.model.marked]
        entry = self.model.entry_at(self.table.current_row())
        return [entry] if entry else []

    def selected_paths(self) -> list[vfs.VPath]:
        return [self.location.child(e.name) for e in self.selected_entries()]

    # -- chrome ------------------------------------------------------------

    def _display_path(self) -> str:
        text = self.location.display
        home = str(Path.home())
        if text == home or text.startswith(home + os.sep):
            text = "~" + text[len(home) :]
        return text

    def _update_subtitle(self) -> None:
        filesystem = self.location.fs
        free = self._free
        if free is not None:
            where = f"{fs.human_size(free)} free"
        else:
            where = filesystem.type_name + ("" if filesystem.writable else " · read-only")
        entries = self.model.entries
        if self.model.marked:
            marked = [e for e in entries if e.name in self.model.marked]
            info = f"{len(marked)} marked, {fs.human_size(sum(e.size for e in marked))}"
        else:
            nfiles = sum(1 for e in entries if not e.is_dir)
            ndirs = len(entries) - nfiles
            info = f"{ndirs} dir{'s' * (ndirs != 1)}, {nfiles} file{'s' * (nfiles != 1)}"
        self.set_subtitle(" · ".join(p for p in (info, where) if p))
