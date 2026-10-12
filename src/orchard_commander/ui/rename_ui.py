"""The Multi-Rename dialog (Total Commander's Ctrl+M)."""

from __future__ import annotations

import threading
from collections.abc import Callable
from dataclasses import replace

from PySide6.QtCore import Qt, QTimer
from PySide6.QtGui import QAction, QColor
from PySide6.QtWidgets import (
    QAbstractItemView,
    QCheckBox,
    QComboBox,
    QDialog,
    QFormLayout,
    QHBoxLayout,
    QHeaderView,
    QLabel,
    QLineEdit,
    QMenu,
    QPushButton,
    QSpinBox,
    QTableWidget,
    QTableWidgetItem,
    QWidget,
)

from orchard_commander import metadata, rename, vfs
from orchard_commander.rename import Rule
from orchard_commander.ui.dialogs import Dialog
from orchard_commander.ui.style import ThemeManager

CASE_LABELS = {"unchanged": "Unchanged", "lower": "lowercase", "upper": "UPPERCASE",
               "first": "First letter uppercase", "words": "Every Word Uppercase"}

# (placeholder, description); None separates groups in the menu
PLACEHOLDERS: list[tuple[str, str] | None] = [
    ("[N]", "Name (without extension)"),
    ("[N1-3]", "Name, characters 1 to 3"),
    ("[N2,5]", "Name, 5 characters from the 2nd"),
    ("[N3-]", "Name, from the 3rd character to the end"),
    ("[N-5-]", "Name, the last 5 characters"),
    ("[N2--5]", "Name, from the 2nd to the 5th-last character"),
    ("[E]", "Extension"),
    ("[1-5]", "Whole name with extension, characters 1 to 5"),
    None,
    ("[P]", "Parent folder's name"),
    ("[G]", "Grandparent folder's name"),
    None,
    ("[C]", "Counter (start, step and digits below)"),
    ("[C10+5:3]", "Counter starting at 10, step 5, 3 digits"),
    None,
    ("[Y]-[M]-[D]", "Modification date: year, month, day"),
    ("[h].[m].[s]", "Modification time: hours, minutes, seconds"),
    ("[d]", "Modification date (2026-03-07)"),
    ("[t]", "Modification time (09.05.04)"),
    None,
    ("[U]", "UPPERCASE from here on"),
    ("[L]", "lowercase from here on"),
    ("[F]", "First Letter Of Each Word uppercase from here on"),
    ("[n]", "Original case from here on"),
    ("[[]", "A literal ["),
    ("[]]", "A literal ]"),
]

OLD, NEW, STATUS = range(3)


class MultiRenameDialog(Dialog):
    def __init__(self, parent: QWidget, themes: ThemeManager, items: list[rename.Item], rule: Rule,
                 exists: Callable[[vfs.VPath], bool]) -> None:
        super().__init__(parent, themes, f"Multi-rename {len(items)} item{'s' * (len(items) != 1)}",
                         width_chars=110)
        self.items = list(items)  # filled in with metadata as it arrives
        self._exists = exists
        self.plan: rename.Plan | None = None
        self._meta_thread: threading.Thread | None = None
        self._meta_results: dict[int, tuple[dict[str, str], str]] = {}  # filled by the worker
        self._meta_cancel = False
        self._meta_poll = QTimer(self, interval=200)
        self._meta_poll.timeout.connect(self._poll_metadata)
        self.rule = rule

        form_widget = QWidget()
        form_widget.setObjectName("dialogBody")
        form = QFormLayout(form_widget)
        form.setContentsMargins(0, 0, 0, 8)
        self.content.addWidget(form_widget)

        def row(*widgets: QWidget, stretch: bool = False) -> QWidget:
            w = QWidget()
            w.setObjectName("dialogBody")
            layout = QHBoxLayout(w)
            layout.setContentsMargins(0, 0, 0, 0)
            for widget in widgets:
                layout.addWidget(widget)
            if stretch:
                layout.addStretch(1)
            return w

        self.name_mask = QLineEdit(rule.name_mask)
        self.ext_mask = QLineEdit(rule.ext_mask)
        self._mask_field = self.name_mask  # where "Insert" puts placeholders
        form.addRow(QLabel("Name"), row(self.name_mask, self._insert_button(self.name_mask)))
        form.addRow(QLabel("Extension"), row(self.ext_mask, self._insert_button(self.ext_mask)))

        self.search = QLineEdit(rule.search)
        self.search.setPlaceholderText("text to find; several: a|b")
        self.replace = QLineEdit(rule.replace)
        self.replace.setPlaceholderText("replacement; pairs: x|y   groups: $1")
        form.addRow(QLabel("Search"), row(self.search, QLabel("Replace"), self.replace))
        self.regex = QCheckBox("RegEx")
        self.regex.setChecked(rule.regex)
        self.case_sensitive = QCheckBox("Case sensitive")
        self.case_sensitive.setChecked(rule.case_sensitive)
        form.addRow(QLabel(""), row(self.regex, self.case_sensitive, stretch=True))

        self.case = QComboBox()
        for key in rename.CASES:
            self.case.addItem(CASE_LABELS[key], key)
        self.case.setCurrentIndex(rename.CASES.index(rule.case))
        self.counter_start = self._spin(rule.counter_start, -999999, 999999)
        self.counter_step = self._spin(rule.counter_step, -1000, 1000)
        self.counter_digits = self._spin(rule.counter_digits, 1, 12)
        form.addRow(QLabel("Case"), row(self.case, QLabel("  Counter [C]: start"), self.counter_start,
                                        QLabel("step"), self.counter_step, QLabel("digits"),
                                        self.counter_digits, stretch=True))

        self.table = QTableWidget(len(items), 3)
        self.table.setHorizontalHeaderLabels(["Old name", "New name", ""])
        self.table.verticalHeader().hide()
        self.table.setShowGrid(False)
        self.table.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self.table.setSelectionMode(QAbstractItemView.SelectionMode.NoSelection)
        self.table.setFocusPolicy(Qt.FocusPolicy.NoFocus)
        header = self.table.horizontalHeader()
        header.setSectionResizeMode(OLD, QHeaderView.ResizeMode.Stretch)
        header.setSectionResizeMode(NEW, QHeaderView.ResizeMode.Stretch)
        header.setSectionResizeMode(STATUS, QHeaderView.ResizeMode.ResizeToContents)
        rows = min(max(len(items), 4), 16)
        row_height = self.table.verticalHeader().defaultSectionSize()
        self.table.setMinimumHeight(row_height * rows + self.table.horizontalHeader().sizeHint().height() + 8)
        for r, item in enumerate(items):
            self.table.setItem(r, OLD, QTableWidgetItem(item.name))
            self.table.setItem(r, NEW, QTableWidgetItem(""))
            self.table.setItem(r, STATUS, QTableWidgetItem(""))
        self.content.addWidget(self.table, 1)

        self.summary = self.add_label("", muted=True)
        self.rename_button, _ = self.add_buttons(("Rename", self._accept), ("Cancel", self.reject))

        self._timer = QTimer(self, singleShot=True, interval=120)
        self._timer.timeout.connect(self._update)
        for edit in (self.name_mask, self.ext_mask, self.search, self.replace):
            edit.textChanged.connect(self._timer.start)
            edit.returnPressed.connect(self._accept)
        for box in (self.regex, self.case_sensitive):
            box.toggled.connect(self._timer.start)
        self.case.currentIndexChanged.connect(self._timer.start)
        for spin in (self.counter_start, self.counter_step, self.counter_digits):
            spin.valueChanged.connect(self._timer.start)
        self.name_mask.selectAll()
        self.name_mask.setFocus()
        self._update()

    # -- building blocks -------------------------------------------------------------

    @staticmethod
    def _spin(value: int, low: int, high: int) -> QSpinBox:
        spin = QSpinBox()
        spin.setRange(low, high)
        spin.setValue(value)
        return spin

    def _insert_button(self, target: QLineEdit) -> QPushButton:
        button = QPushButton("Insert…")
        button.setAutoDefault(False)
        button.setFocusPolicy(Qt.FocusPolicy.NoFocus)
        menu = QMenu(button)
        for entry in PLACEHOLDERS:
            if entry is None:
                menu.addSeparator()
                continue
            code, description = entry
            action = QAction(f"{code}\t{description}", menu)
            action.triggered.connect(lambda _=False, c=code: self._insert(target, c))
            menu.addAction(action)
        menu.addSeparator()
        for provider in metadata.providers():  # [=field] placeholders, one submenu per kind of file
            sub = menu.addMenu(provider.label)
            for name, description in provider.fields.items():
                code = f"[={name}]"
                action = QAction(f"{code}\t{description}", sub)
                action.triggered.connect(lambda _=False, c=code: self._insert(target, c))
                sub.addAction(action)
        hint = menu.addAction("[=artist|Unknown]\tA field with a fallback when the file has none")
        hint.triggered.connect(lambda: self._insert(target, "[=artist|Unknown]"))
        button.setMenu(menu)
        return button

    def _insert(self, target: QLineEdit, code: str) -> None:
        if target is self.name_mask and target.text() == "[N]" and target.hasSelectedText():
            target.setText("")  # the default is selected: replace it
        target.insert(code)
        target.setFocus()

    # -- preview ---------------------------------------------------------------------

    def current_rule(self) -> Rule:
        return Rule(
            name_mask=self.name_mask.text(),
            ext_mask=self.ext_mask.text(),
            search=self.search.text(),
            replace=self.replace.text(),
            regex=self.regex.isChecked(),
            case_sensitive=self.case_sensitive.isChecked(),
            case=self.case.currentData(),
            counter_start=self.counter_start.value(),
            counter_step=self.counter_step.value(),
            counter_digits=self.counter_digits.value(),
        )

    # -- metadata ([=field] placeholders) -----------------------------------------------

    def _start_metadata(self) -> None:
        """Read every file's metadata once, in the background; the preview fills in as it arrives."""
        if self._meta_thread is not None:
            return
        items = list(self.items)
        results = self._meta_results

        def work() -> None:
            for i, item in enumerate(items):
                if self._meta_cancel:
                    return
                try:
                    meta, error = metadata.read(item.location, is_dir=item.is_dir, online_only=item.online_only), ""
                except metadata.MetadataUnavailable as exc:
                    meta, error = {}, str(exc)
                except Exception as exc:  # unreadable file: skip it, with the reason
                    meta, error = {}, f"unreadable ({exc})"
                results[i] = (meta, error)

        self._meta_thread = threading.Thread(target=work, daemon=True, name="rename-metadata")
        self._meta_thread.start()
        self._meta_poll.start()

    def _poll_metadata(self) -> None:
        arrived = [(i, v) for i, v in list(self._meta_results.items()) if self.items[i].meta is None]
        for i, (meta, error) in arrived:
            self.items[i] = replace(self.items[i], meta=meta, meta_error=error)
        if len(self._meta_results) == len(self.items):
            self._meta_poll.stop()
        if arrived:
            self._update()

    def done(self, result: int) -> None:
        self._meta_cancel = True
        self._meta_poll.stop()
        super().done(result)

    # -- preview ---------------------------------------------------------------------

    def _update(self) -> None:
        self.rule = self.current_rule()
        if rename.needs_metadata(self.rule):
            self._start_metadata()
        plan = self.plan = rename.plan(self.items, self.rule, exists=self._exists)
        colors = self.themes.colors
        for r, change in enumerate(plan.changes):
            new_item, status = self.table.item(r, NEW), self.table.item(r, STATUS)
            if plan.mask_error:
                new_item.setText("")
                status.setText("")
                continue
            new_item.setText(change.new)
            if change.error:
                status.setText(change.error)
                color = colors.error
            elif change.note:
                status.setText(change.note)
                color = colors.muted
            elif change.changed:
                status.setText("")
                color = colors.foreground
            else:
                status.setText("unchanged")
                color = colors.muted
            new_item.setForeground(QColor(color))
            status.setForeground(QColor(colors.error if change.error
                                        else colors.warning if change.note and not change.pending else colors.muted))
        if plan.mask_error:
            text = plan.mask_error
        else:
            n, problems = len(plan.to_rename), len(plan.problems)
            text = f"{n} of {len(self.items)} will be renamed"
            if plan.pending:
                text += f" · reading metadata… {len(self.items) - plan.pending} of {len(self.items)}"
            if plan.skipped:
                text += f" · {len(plan.skipped)} skipped (no value; add a fallback like [=artist|Unknown])"
            if problems:
                text += f" · {problems} problem{'s' * (problems != 1)} to fix before renaming"
        self.summary.setText(text)
        self.rename_button.setEnabled(plan.can_run)

    def _accept(self) -> None:
        self._timer.stop()
        self._update()
        if self.plan is not None and self.plan.can_run:
            self.accept()

    def ask(self) -> tuple[rename.Plan, Rule] | None:
        if self.exec() != QDialog.DialogCode.Accepted or self.plan is None:
            return None
        return self.plan, self.rule
