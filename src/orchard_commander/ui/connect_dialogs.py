"""Dialogs for server connections: manager, editor, password, provider questions."""

from __future__ import annotations

from dataclasses import dataclass

from PySide6.QtCore import Qt
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QDialog,
    QFormLayout,
    QLabel,
    QLineEdit,
    QListWidget,
    QListWidgetItem,
    QWidget,
)

from orchard_commander import connections, fsproviders
from orchard_commander.connections import Connection
from orchard_commander.fsproviders import FileSystemProvider
from orchard_commander.ui.dialogs import Dialog
from orchard_commander.ui.style import ThemeManager


@dataclass
class Credentials:
    password: str | None
    remember: bool


# ---------------------------------------------------------------------------
# Editor
# ---------------------------------------------------------------------------


class ConnectionEditor(Dialog):
    """Create or edit a connection. The form comes from the connection type's
    `FileSystemProvider.fields`, so add-on connection types get one too."""

    def __init__(self, parent: QWidget, themes: ThemeManager, conn: Connection | None = None) -> None:
        super().__init__(parent, themes, "Edit connection" if conn else "New connection", width_chars=64)
        self._original = conn
        self._values: dict[str, str] = {k: str(v) for k, v in (conn.params if conn else {}).items()}
        self._inputs: dict[str, QLineEdit] = {}

        top = QWidget()
        top.setObjectName("dialogBody")
        top_form = QFormLayout(top)
        top_form.setContentsMargins(0, 0, 0, 0)
        self.kind = QComboBox()
        for provider in fsproviders.providers():
            self.kind.addItem(provider.title, provider.scheme)
        self.name = QLineEdit(conn.name if conn else "")
        self.name.setPlaceholderText("optional")
        top_form.addRow(QLabel("Type"), self.kind)
        top_form.addRow(QLabel("Name"), self.name)
        self.content.addWidget(top)

        self._fields_widget = QWidget()
        self._fields_widget.setObjectName("dialogBody")
        self._fields_form = QFormLayout(self._fields_widget)
        self._fields_form.setContentsMargins(0, 0, 0, 0)
        self.content.addWidget(self._fields_widget)

        bottom = QWidget()
        bottom.setObjectName("dialogBody")
        bottom_form = QFormLayout(bottom)
        bottom_form.setContentsMargins(0, 0, 0, 0)
        self.password = QLineEdit()
        self.password.setEchoMode(QLineEdit.EchoMode.Password)
        self._password_label = QLabel("Password")
        self.remember = QCheckBox("Remember in the system keychain")
        bottom_form.addRow(self._password_label, self.password)
        bottom_form.addRow(QLabel(""), self.remember)
        self.content.addWidget(bottom)
        self._forms = (top_form, self._fields_form, bottom_form)

        self.error = self.add_label("", muted=True)
        self.add_buttons(("Save", self._save), ("Cancel", self.reject))

        if conn is not None:
            index = self.kind.findData(conn.kind)
            if index >= 0:
                self.kind.setCurrentIndex(index)
            self.remember.setChecked(connections.get_password(conn) is not None)
        self.kind.currentIndexChanged.connect(self._kind_changed)
        self._kind_changed()
        self._result: tuple[Connection, Credentials] | None = None

    @property
    def provider(self) -> FileSystemProvider:
        provider = fsproviders.get(self.kind.currentData())
        assert provider is not None
        return provider

    def _kind_changed(self) -> None:
        for key, edit in self._inputs.items():  # keep what was typed, e.g. the server name
            self._values[key] = edit.text()
        while self._fields_form.rowCount():
            self._fields_form.removeRow(0)
        self._inputs = {}
        for f in self.provider.fields:
            edit = QLineEdit(self._values.get(f.key, "" if f.default is None else str(f.default)))
            edit.setPlaceholderText(f.placeholder)
            self._fields_form.addRow(QLabel(f.label), edit)
            self._inputs[f.key] = edit
        self._password_label.setText(self.provider.secret_label)
        self.password.setPlaceholderText(self.provider.secret_hint)
        self._align_labels()
        if self._inputs:
            next(iter(self._inputs.values())).setFocus()
        self.adjustSize()

    def _align_labels(self) -> None:
        """One label column across the three forms (type, the provider's fields, secret)."""
        labels = []
        for form in self._forms:
            for row in range(form.rowCount()):
                item = form.itemAt(row, QFormLayout.ItemRole.LabelRole)
                if item is not None and item.widget() is not None:
                    labels.append(item.widget())
        width = max((label.sizeHint().width() for label in labels), default=0)
        for label in labels:
            label.setMinimumWidth(width)

    def _params(self) -> dict[str, str | int]:
        params: dict[str, str | int] = {}
        for f in self.provider.fields:
            value = self._inputs[f.key].text().strip()
            if f.kind == "int" and value.isdigit():
                params[f.key] = int(value)
            elif value:
                params[f.key] = value
        return params

    def _save(self) -> None:
        params = self._params()
        error = self.provider.validate(params)
        if error:
            self.error.setText(error)
            return
        base = self._original or Connection(self.provider.scheme)
        conn = base.with_changes(kind=self.provider.scheme, params=params, name=self.name.text().strip())
        self._result = (conn, Credentials(self.password.text() or None, self.remember.isChecked()))
        self.accept()

    def edit(self) -> tuple[Connection, Credentials] | None:
        return self._result if self.exec() == QDialog.DialogCode.Accepted else None


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------


class ConnectionManager(Dialog):
    """Saved connections: connect, add, edit, delete. Returns the one to connect to."""

    def __init__(self, parent: QWidget, themes: ThemeManager) -> None:
        super().__init__(parent, themes, "Connect to server", width_chars=72)
        self.add_label(f"Saved connections — or use Go to (Ctrl+G) with an address: {fsproviders.address_examples()}",
                       muted=True)
        self.list = QListWidget()
        self.content.addWidget(self.list)
        fm = self.fontMetrics()
        self.list.setMinimumHeight(fm.height() * 8)
        self.list.itemActivated.connect(lambda _: self._connect())
        self.buttons = self.add_buttons(
            ("Connect", self._connect), ("New…", self._new), ("Edit…", self._edit), ("Delete", self._delete),
            ("Close", self.reject),
        )
        self.chosen: tuple[Connection, Credentials | None] | None = None
        self._reload()

    def _reload(self, select_id: str | None = None) -> None:
        self.list.clear()
        for conn in sorted(connections.load_saved(), key=lambda c: c.label.casefold()):
            item = QListWidgetItem(f"{conn.label:<24}  {conn.url}" if conn.name else conn.url)
            item.setData(Qt.ItemDataRole.UserRole, conn)
            self.list.addItem(item)
            if conn.id == select_id:
                self.list.setCurrentItem(item)
        if self.list.currentRow() < 0 and self.list.count():
            self.list.setCurrentRow(0)
        has = self.list.count() > 0
        for b in (self.buttons[0], self.buttons[2], self.buttons[3]):
            b.setEnabled(has)
        (self.list if has else self.buttons[1]).setFocus()

    def _current(self) -> Connection | None:
        item = self.list.currentItem()
        return item.data(Qt.ItemDataRole.UserRole) if item else None

    def _apply(self, edited: tuple[Connection, Credentials] | None) -> tuple[Connection, Credentials] | None:
        if edited is None:
            return None
        conn, creds = edited
        connections.save(conn)
        if creds.remember and creds.password:
            connections.store_password(conn, creds.password)
        elif not creds.remember:
            connections.forget_password(conn)
        self._reload(conn.id)
        return edited

    def _new(self) -> None:
        edited = self._apply(ConnectionEditor(self, self.themes).edit())
        if edited is not None:  # a new connection is usually wanted right away
            self.chosen = edited
            self.accept()

    def _edit(self) -> None:
        conn = self._current()
        if conn is not None:
            self._apply(ConnectionEditor(self, self.themes, conn).edit())

    def _delete(self) -> None:
        from orchard_commander.ui.dialogs import confirm

        conn = self._current()
        if conn is not None and confirm(self, self.themes, "Delete connection", f"Delete '{conn.label}'?", "Delete"):
            connections.remove(conn)
            self._reload()

    def _connect(self) -> None:
        conn = self._current()
        if conn is not None:
            self.chosen = (conn, None)
            self.accept()

    def choose(self) -> tuple[Connection, Credentials | None] | None:
        return self.chosen if self.exec() == QDialog.DialogCode.Accepted else None


# ---------------------------------------------------------------------------
# Prompts
# ---------------------------------------------------------------------------


def ask_password(parent: QWidget, themes: ThemeManager, conn: Connection, reason: str = "") -> Credentials | None:
    provider = fsproviders.get(conn.kind)
    secret_label = provider.secret_label if provider else "Password"
    d = Dialog(parent, themes, secret_label, width_chars=56)
    if reason:
        d.add_label(reason, muted=True)
    d.add_label(f"{secret_label} for {conn.description}:")
    edit = QLineEdit()
    edit.setEchoMode(QLineEdit.EchoMode.Password)
    d.content.addWidget(edit)
    remember = QCheckBox("Remember in the system keychain")
    d.content.addWidget(remember)
    d.add_buttons(("Connect", d.accept), ("Cancel", d.reject))
    edit.returnPressed.connect(d.accept)
    edit.setFocus()
    if d.exec() != QDialog.DialogCode.Accepted:
        return None
    return Credentials(edit.text(), remember.isChecked())


def confirm_question(parent: QWidget, themes: ThemeManager, title: str, message: str, ok_label: str) -> bool:
    """A yes/no question from a connection provider (e.g. trusting a host key).
    Cancel is the default, so agreeing is always a deliberate choice."""
    d = Dialog(parent, themes, title, width_chars=72)
    paragraphs = [p for p in message.split("\n\n") if p.strip()]
    for i, paragraph in enumerate(paragraphs):
        d.add_label(paragraph, muted=i == len(paragraphs) - 1 and len(paragraphs) > 2)
    _, cancel = d.add_buttons((ok_label, d.accept), ("Cancel", d.reject), default=1)
    cancel.setFocus()
    return d.exec() == QDialog.DialogCode.Accepted
