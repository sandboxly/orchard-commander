"""Bookmark menu, "add bookmark" dialog and the bookmark organizer."""

from __future__ import annotations

import os
from collections.abc import Callable

from PySide6.QtCore import QEvent, Qt
from PySide6.QtGui import QAction, QKeyEvent
from PySide6.QtWidgets import (
    QAbstractItemView,
    QCheckBox,
    QComboBox,
    QDialog,
    QFormLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QMenu,
    QPushButton,
    QTreeWidget,
    QTreeWidgetItem,
    QWidget,
)

from orchard_commander import bookmarks, connections, locations
from orchard_commander.bookmarks import Bookmark, Folder, Item, Separator
from orchard_commander.locations import Spec
from orchard_commander.ui.dialogs import Dialog
from orchard_commander.ui.style import ThemeManager

ItemRole = Qt.ItemDataRole.UserRole + 1


# ---------------------------------------------------------------------------
# The menu (Ctrl+D)
# ---------------------------------------------------------------------------


class HotkeyMenu(QMenu):
    """A menu whose entries are chosen by their hotkey letter, as in Total Commander.

    Works the same on every platform (macOS menus have no letter mnemonics).
    If several entries share a letter, pressing it moves between them.
    """

    def keyPressEvent(self, event: QKeyEvent) -> None:
        text = event.text().lower()
        if len(text) == 1 and text.isprintable() and not text.isspace() and not event.modifiers() & (
            Qt.KeyboardModifier.ControlModifier | Qt.KeyboardModifier.MetaModifier
        ):
            matches = [a for a in self.actions() if a.isEnabled() and a.property("hotkey") == text]
            if len(matches) == 1:
                self._activate(matches[0])
                return
            if matches:  # cycle
                current = self.activeAction()
                index = (matches.index(current) + 1) % len(matches) if current in matches else 0
                self.setActiveAction(matches[index])
                return
        super().keyPressEvent(event)

    def _activate(self, action: QAction) -> None:
        if action.menu() is not None:  # a folder: open it
            self.setActiveAction(action)
            right = QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_Right, Qt.KeyboardModifier.NoModifier)
            super().keyPressEvent(right)
        else:
            action.trigger()
            self.close_all()

    def close_all(self) -> None:
        menu: QWidget | None = self
        while isinstance(menu, QMenu):
            menu.close()
            menu = menu.parentWidget()


def menu_text(title: str, detail: str = "", *, key: str | None = None) -> str:
    """'P  Projects<TAB>~/dev' — hotkey column, title, and a right-aligned detail."""
    key = key or bookmarks.hotkey(title)
    text = f"{key.upper() if key else ' '}  {bookmarks.plain(title)}"
    return f"{text}\t{detail}" if detail else text


def build_menu(
    parent: QWidget,
    items: list[Item],
    *,
    on_open: Callable[[Bookmark], None],
    current: Spec | None,
    on_add: Callable[[], None],
    on_remove: Callable[[Bookmark], None],
    on_organize: Callable[[], None],
) -> HotkeyMenu:
    menu = HotkeyMenu(parent)
    menu.setObjectName("bookmarks")

    def fill(target: QMenu, children: list[Item]) -> None:
        for item in children:
            if isinstance(item, Separator):
                target.addSeparator()
            elif isinstance(item, Folder):
                sub = HotkeyMenu(menu_text(item.title), target)
                sub.menuAction().setProperty("hotkey", bookmarks.hotkey(item.title))
                fill(sub, item.items)
                if not item.items:
                    empty = sub.addAction("   (empty)")
                    empty.setEnabled(False)
                target.addMenu(sub)
            else:
                detail = locations.describe(item.location)
                if item.target is not None:
                    detail += "  ⇄  " + locations.describe(item.target)
                action = target.addAction(menu_text(item.title, detail))
                action.setProperty("hotkey", bookmarks.hotkey(item.title))
                action.triggered.connect(lambda _=False, b=item: on_open(b))

    fill(menu, items)
    if items:
        menu.addSeparator()
    existing = bookmarks.find_location(items, current) if current is not None else None
    # Built-in entries use symbols, so every letter stays free for the user's bookmarks.
    if current is not None and existing is None:
        add = menu.addAction(menu_text("Add current folder…", key="+"))
        add.setProperty("hotkey", "+")
        add.triggered.connect(on_add)
    if existing is not None:
        rm = menu.addAction(menu_text(f"Remove current folder ({bookmarks.plain(existing.title)})", key="-"))
        rm.setProperty("hotkey", "-")
        rm.triggered.connect(lambda: on_remove(existing))
    organize = menu.addAction(menu_text("Organize bookmarks…", key="*"))
    organize.setProperty("hotkey", "*")
    organize.triggered.connect(on_organize)
    return menu


# ---------------------------------------------------------------------------
# Add bookmark
# ---------------------------------------------------------------------------


def folder_choices(items: list[Item]) -> list[tuple[str, str | None]]:
    """(label, folder id) for every place a bookmark can go; None is the top level."""
    choices: list[tuple[str, str | None]] = [("Top level", None)]
    for trail, folder in bookmarks.folders(items):
        choices.append((" › ".join((*trail, bookmarks.plain(folder.title))), folder.id))
    return choices


class AddBookmarkDialog(Dialog):
    def __init__(
        self, parent: QWidget, themes: ThemeManager, items: list[Item], location: Spec, target: Spec | None
    ) -> None:
        super().__init__(parent, themes, "Add bookmark", width_chars=70)
        body = QWidget()
        body.setObjectName("dialogBody")
        form = QFormLayout(body)
        form.setContentsMargins(0, 0, 0, 0)
        self.title = QLineEdit(locations.name_of(location))
        self.title.selectAll()
        self.folder = QComboBox()
        for label, folder_id in folder_choices(items):
            self.folder.addItem(label, folder_id)
        form.addRow(QLabel("Title"), self.title)
        form.addRow(QLabel("Folder"), QLabel(locations.describe(location)))
        form.addRow(QLabel("Put in"), self.folder)
        self.content.addWidget(body)
        self.with_target = QCheckBox(
            f"Also open the other panel at {locations.describe(target)}" if target else "Also open the other panel"
        )
        self.with_target.setEnabled(target is not None)
        self.content.addWidget(self.with_target)
        self.add_label("Tip: an & in the title sets its hotkey, e.g. “&Projects” → P.", muted=True)
        self.add_buttons(("Add", self.accept), ("Cancel", self.reject))
        self.title.returnPressed.connect(self.accept)
        self.title.setFocus()

    def ask(self) -> tuple[str, str | None, bool] | None:
        if self.exec() != QDialog.DialogCode.Accepted:
            return None
        title = self.title.text().strip()
        return (title or "Bookmark", self.folder.currentData(), self.with_target.isChecked())


# ---------------------------------------------------------------------------
# Organizer (TC's "Configure")
# ---------------------------------------------------------------------------


def parse_location(text: str, original: Spec | None) -> Spec | None:
    """A spec from what the user typed: a local path or a server address.
    Unchanged text keeps the original spec (e.g. a place inside an archive)."""
    text = text.strip()
    if not text:
        return None
    if original is not None and text == locations.describe(original):
        return original
    if connections.is_url(text):
        conn = connections.Connection.from_url(text)
        return {"kind": "server", "account": conn.account, "path": str(conn.get("path", "")) or "/"}
    return {"kind": "local", "path": os.path.abspath(os.path.expanduser(text))}


class EditBookmarkDialog(Dialog):
    def __init__(self, parent: QWidget, themes: ThemeManager, bookmark: Bookmark,
                 active: Spec | None, other: Spec | None) -> None:
        super().__init__(parent, themes, "Edit bookmark", width_chars=76)
        self._bookmark = bookmark
        body = QWidget()
        body.setObjectName("dialogBody")
        form = QFormLayout(body)
        form.setContentsMargins(0, 0, 0, 0)
        self.title = QLineEdit(bookmark.title)
        self.location = QLineEdit(locations.describe(bookmark.location))
        self.target = QLineEdit(locations.describe(bookmark.target) if bookmark.target else "")
        self.target.setPlaceholderText("optional: a folder to open in the other panel")
        form.addRow(QLabel("Title"), self.title)
        form.addRow(QLabel("Folder"), self._with_button(self.location, "Active panel", active))
        form.addRow(QLabel("Other panel"), self._with_button(self.target, "Other panel", other))
        self.content.addWidget(body)
        self.error = self.add_label("", muted=True)
        self.add_buttons(("Save", self._save), ("Cancel", self.reject))
        self._picked: dict[int, Spec] = {}
        self.result_bookmark: Bookmark | None = None

    def _with_button(self, edit: QLineEdit, label: str, spec: Spec | None) -> QWidget:
        row_widget = QWidget()
        row_widget.setObjectName("dialogBody")
        row = QHBoxLayout(row_widget)
        row.setContentsMargins(0, 0, 0, 0)
        row.addWidget(edit, 1)
        button = QPushButton(f"Use {label.lower()}")
        button.setEnabled(spec is not None)
        button.setToolTip(locations.describe(spec) if spec else "")
        button.clicked.connect(lambda: (edit.setText(locations.describe(spec)), self._picked.__setitem__(id(edit), spec)))
        row.addWidget(button)
        return row_widget

    def _spec(self, edit: QLineEdit, original: Spec | None) -> Spec | None:
        picked = self._picked.get(id(edit))
        if picked is not None and edit.text().strip() == locations.describe(picked):
            return picked
        return parse_location(edit.text(), original)

    def _save(self) -> None:
        try:
            location = self._spec(self.location, self._bookmark.location)
            target = self._spec(self.target, self._bookmark.target)
        except ValueError as exc:
            self.error.setText(str(exc))
            return
        if location is None:
            self.error.setText("Enter a folder.")
            return
        b = self._bookmark
        self.result_bookmark = Bookmark(self.title.text().strip() or "Bookmark", location, target, b.id)
        self.accept()


class BookmarkOrganizer(Dialog):
    """Reorder by drag and drop (into folders, too), add folders and separators,
    edit and delete. Changes apply only with OK."""

    def __init__(self, parent: QWidget, themes: ThemeManager, items: list[Item],
                 active: Spec | None, other: Spec | None) -> None:
        super().__init__(parent, themes, "Organize bookmarks", width_chars=96)
        self._active, self._other = active, other
        self.tree = QTreeWidget()
        self.tree.setHeaderLabels(["Title", "Folder", "Other panel"])
        self.tree.setDragDropMode(QAbstractItemView.DragDropMode.InternalMove)
        self.tree.setSelectionMode(QAbstractItemView.SelectionMode.SingleSelection)
        self.tree.setMinimumHeight(self.fontMetrics().height() * 16)
        self.tree.itemDoubleClicked.connect(lambda *_: self.edit())
        self.content.addWidget(self.tree)
        for item in items:
            self.tree.addTopLevelItem(self._node(item))
        self.tree.expandAll()
        for column in range(3):
            self.tree.resizeColumnToContents(column)
        buttons = QWidget()
        buttons.setObjectName("buttonRow")
        row = QHBoxLayout(buttons)
        row.setContentsMargins(0, 0, 0, 0)
        for text, slot in (("New folder", self.add_folder), ("Separator", self.add_separator),
                           ("Edit…", self.edit), ("Delete", self.delete), ("▲", lambda: self.move(-1)),
                           ("▼", lambda: self.move(1))):
            b = QPushButton(text)
            b.setAutoDefault(False)
            b.clicked.connect(slot)
            row.addWidget(b)
        row.addStretch(1)
        self.content.addWidget(buttons)
        self.add_label("Drag entries to reorder them or to move them into folders.", muted=True)
        self.add_buttons(("OK", self.accept), ("Cancel", self.reject))
        self.tree.setFocus()

    # -- tree <-> items ------------------------------------------------------------

    def _node(self, item: Item) -> QTreeWidgetItem:
        node = QTreeWidgetItem()
        node.setData(0, ItemRole, item)
        self._label(node)
        flags = node.flags() | Qt.ItemFlag.ItemIsDragEnabled
        if isinstance(item, Folder):
            flags |= Qt.ItemFlag.ItemIsDropEnabled
            for child in item.items:
                node.addChild(self._node(child))
        else:
            flags &= ~Qt.ItemFlag.ItemIsDropEnabled  # only folders accept children
        node.setFlags(flags)
        return node

    @staticmethod
    def _label(node: QTreeWidgetItem) -> None:
        item = node.data(0, ItemRole)
        if isinstance(item, Separator):
            node.setText(0, "────────")
        elif isinstance(item, Folder):
            node.setText(0, menu_text(item.title))
        else:
            node.setText(0, menu_text(item.title))
            node.setText(1, locations.describe(item.location))
            node.setText(2, locations.describe(item.target) if item.target else "")

    def _collect(self, parent: QTreeWidgetItem | None) -> list[Item]:
        count = parent.childCount() if parent else self.tree.topLevelItemCount()
        result: list[Item] = []
        for i in range(count):
            node = parent.child(i) if parent else self.tree.topLevelItem(i)
            item = node.data(0, ItemRole)
            if isinstance(item, Folder):
                item = Folder(item.title, self._collect(node), item.id)
            result.append(item)
        return result

    def items(self) -> list[Item]:
        return self._collect(None)

    # -- actions ----------------------------------------------------------------------

    def _insert(self, node: QTreeWidgetItem) -> None:
        current = self.tree.currentItem()
        if current is not None and isinstance(current.data(0, ItemRole), Folder):
            current.addChild(node)
            current.setExpanded(True)
        elif current is not None and current.parent() is not None:
            parent = current.parent()
            parent.insertChild(parent.indexOfChild(current) + 1, node)
        elif current is not None:
            self.tree.insertTopLevelItem(self.tree.indexOfTopLevelItem(current) + 1, node)
        else:
            self.tree.addTopLevelItem(node)
        self.tree.setCurrentItem(node)

    def add_folder(self) -> None:
        from orchard_commander.ui.dialogs import ask_text

        title = ask_text(self, self.themes, "New folder", "Folder title (an & sets its hotkey):", "", "Create")
        if title:
            self._insert(self._node(Folder(title)))

    def add_separator(self) -> None:
        self._insert(self._node(Separator()))

    def edit(self) -> None:
        from orchard_commander.ui.dialogs import ask_text

        node = self.tree.currentItem()
        if node is None:
            return
        item = node.data(0, ItemRole)
        if isinstance(item, Folder):
            title = ask_text(self, self.themes, "Rename folder", "Folder title:", item.title, "Save")
            if title:
                node.setData(0, ItemRole, Folder(title, item.items, item.id))
                self._label(node)
        elif isinstance(item, Bookmark):
            dialog = EditBookmarkDialog(self, self.themes, item, self._active, self._other)
            if dialog.exec() == QDialog.DialogCode.Accepted and dialog.result_bookmark is not None:
                node.setData(0, ItemRole, dialog.result_bookmark)
                self._label(node)

    def delete(self) -> None:
        node = self.tree.currentItem()
        if node is None:
            return
        parent = node.parent()
        (parent.removeChild(node) if parent else self.tree.takeTopLevelItem(self.tree.indexOfTopLevelItem(node)))

    def move(self, delta: int) -> None:
        node = self.tree.currentItem()
        if node is None:
            return
        parent = node.parent()
        siblings = parent.childCount() if parent else self.tree.topLevelItemCount()
        index = parent.indexOfChild(node) if parent else self.tree.indexOfTopLevelItem(node)
        new = index + delta
        if not 0 <= new < siblings:
            return
        expanded = node.isExpanded()
        if parent:
            parent.takeChild(index)
            parent.insertChild(new, node)
        else:
            self.tree.takeTopLevelItem(index)
            self.tree.insertTopLevelItem(new, node)
        node.setExpanded(expanded)
        self.tree.setCurrentItem(node)

    def organize(self) -> list[Item] | None:
        return self.items() if self.exec() == QDialog.DialogCode.Accepted else None
