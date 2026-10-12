"""The Find Files dialog (Total Commander's Alt+F7)."""

from __future__ import annotations

import os
import re
import time
from dataclasses import dataclass, replace

from PySide6.QtWidgets import QCheckBox, QComboBox, QDialog, QFormLayout, QHBoxLayout, QLabel, QLineEdit, QWidget

from orchard_commander import vfs
from orchard_commander.search import Query
from orchard_commander.ui.dialogs import Dialog
from orchard_commander.ui.style import ThemeManager

DEPTHS = [("All subfolders", None), ("This folder only", 0), ("1 level", 1), ("2 levels", 2), ("3 levels", 3),
          ("5 levels", 5)]
KINDS = [("Files and folders", "both"), ("Files only", "files"), ("Folders only", "dirs")]
DAY = 86400.0
DATES = [("Any time", None, None), ("Today", 1, None), ("Last 7 days", 7, None), ("Last 30 days", 30, None),
         ("Last year", 365, None), ("Older than 30 days", None, 30), ("Older than a year", None, 365)]
UNITS = {"": 1, "b": 1, "k": 1024, "kb": 1024, "m": 1024**2, "mb": 1024**2, "g": 1024**3, "gb": 1024**3}


def parse_size(text: str) -> int | None:
    """'10M', '1.5 GB', '200k', '42' -> bytes; '' -> None. Raises ValueError."""
    text = text.strip().lower().replace(",", ".")
    if not text:
        return None
    match = re.fullmatch(r"(\d+(?:\.\d+)?)\s*([kmg]?b?)", text)
    if not match:
        raise ValueError(f"Not a size: {text!r} (examples: 500k, 10M, 1.5G)")
    return int(float(match.group(1)) * UNITS[match.group(2)])


@dataclass
class Form:
    """What the dialog remembers between searches (this session)."""

    names: str = "*"
    name_regex: bool = False
    name_case: bool = False
    depth: int = 0  # index into DEPTHS
    kinds: int = 0
    include_hidden: bool = True
    search_archives: bool = False
    text: str = ""
    text_regex: bool = False
    text_case: bool = False
    whole_words: bool = False
    not_containing: bool = False
    include_online_only: bool = False
    min_size: str = ""
    max_size: str = ""
    date: int = 0


class FindFilesDialog(Dialog):
    def __init__(self, parent: QWidget, themes: ThemeManager, here: vfs.VPath, form: Form) -> None:
        super().__init__(parent, themes, "Find files", width_chars=84)
        self._here = here
        self._form = form

        def section(title: str) -> QFormLayout:
            label = QLabel(title)
            label.setObjectName("dialogTitle")
            self.content.addWidget(label)
            body = QWidget()
            body.setObjectName("dialogBody")
            layout = QFormLayout(body)
            layout.setContentsMargins(0, 0, 0, 8)
            self.content.addWidget(body)
            return layout

        def checks(*boxes: QCheckBox) -> QWidget:
            row_widget = QWidget()
            row_widget.setObjectName("dialogBody")
            row = QHBoxLayout(row_widget)
            row.setContentsMargins(0, 0, 0, 0)
            for box in boxes:
                row.addWidget(box)
            row.addStretch(1)
            return row_widget

        names = section("Names")
        self.names = QLineEdit(form.names)
        self.names.setPlaceholderText("*.pdf;*.docx   report   *.zip|*old*   (| excludes)")
        self.name_regex = QCheckBox("RegEx")
        self.name_regex.setChecked(form.name_regex)
        self.name_case = QCheckBox("Case sensitive")
        self.name_case.setChecked(form.name_case)
        names.addRow(QLabel("Search for"), self.names)
        names.addRow(QLabel(""), checks(self.name_regex, self.name_case))
        self.where = QLineEdit(here.display)
        self.where.setToolTip("Several folders: separate them with ;")
        names.addRow(QLabel("Search in"), self.where)
        self.depth = QComboBox()
        for label, _ in DEPTHS:
            self.depth.addItem(label)
        self.depth.setCurrentIndex(form.depth)
        self.kinds = QComboBox()
        for label, _ in KINDS:
            self.kinds.addItem(label)
        self.kinds.setCurrentIndex(form.kinds)
        names.addRow(QLabel("Depth"), self.depth)
        names.addRow(QLabel("Find"), self.kinds)
        self.include_hidden = QCheckBox("Hidden files")
        self.include_hidden.setChecked(form.include_hidden)
        self.search_archives = QCheckBox("Inside archives")
        self.search_archives.setChecked(form.search_archives)
        names.addRow(QLabel("Include"), checks(self.include_hidden, self.search_archives))

        content = section("Contents")
        self.text = QLineEdit(form.text)
        self.text.setPlaceholderText("optional: text the files must contain")
        content.addRow(QLabel("Containing"), self.text)
        self.text_regex = QCheckBox("RegEx")
        self.text_regex.setChecked(form.text_regex)
        self.text_case = QCheckBox("Case sensitive")
        self.text_case.setChecked(form.text_case)
        self.whole_words = QCheckBox("Whole words")
        self.whole_words.setChecked(form.whole_words)
        self.not_containing = QCheckBox("NOT containing")
        self.not_containing.setChecked(form.not_containing)
        content.addRow(QLabel(""), checks(self.text_regex, self.text_case, self.whole_words, self.not_containing))
        self.include_online_only = QCheckBox("Also cloud-only files (downloads them)")
        self.include_online_only.setChecked(form.include_online_only)
        content.addRow(QLabel(""), checks(self.include_online_only))

        more = section("Size and date")
        self.min_size = QLineEdit(form.min_size)
        self.min_size.setPlaceholderText("e.g. 10M")
        self.max_size = QLineEdit(form.max_size)
        self.max_size.setPlaceholderText("e.g. 1.5G")
        sizes = QWidget()
        sizes.setObjectName("dialogBody")
        size_row = QHBoxLayout(sizes)
        size_row.setContentsMargins(0, 0, 0, 0)
        size_row.addWidget(QLabel("at least"))
        size_row.addWidget(self.min_size)
        size_row.addWidget(QLabel("at most"))
        size_row.addWidget(self.max_size)
        more.addRow(QLabel("Size"), sizes)
        self.date = QComboBox()
        for label, _, _ in DATES:
            self.date.addItem(label)
        self.date.setCurrentIndex(form.date)
        more.addRow(QLabel("Modified"), self.date)

        self.error = self.add_label("", muted=True)
        self.add_buttons(("Search", self._search), ("Cancel", self.reject))
        self.names.returnPressed.connect(self._search)
        self.text.returnPressed.connect(self._search)
        self.names.selectAll()
        self.names.setFocus()
        self.query: Query | None = None

    def _roots(self) -> tuple[vfs.VPath, ...]:
        text = self.where.text().strip()
        if text == self._here.display:
            return (self._here,)  # unchanged: also works for servers and archives
        roots = []
        for part in (p.strip() for p in text.split(";")):
            if part:
                roots.append(vfs.local(os.path.abspath(os.path.expanduser(part))))
        return tuple(roots)

    def _search(self) -> None:
        try:
            min_size = parse_size(self.min_size.text())
            max_size = parse_size(self.max_size.text())
            for pattern, enabled in ((self.names.text(), self.name_regex.isChecked()),
                                     (self.text.text(), self.text_regex.isChecked())):
                if enabled and pattern:
                    re.compile(pattern)
        except (ValueError, re.error) as exc:
            self.error.setText(f"{exc}")
            return
        roots = self._roots()
        if not roots:
            self.error.setText("Enter a folder to search in.")
            return
        missing = [r for r in roots if r.is_local and not r.is_dir()]
        if missing:
            self.error.setText(f"Not a folder: {missing[0].display}")
            return
        _, newer_days, older_days = DATES[self.date.currentIndex()]
        now = time.time()
        self.query = Query(
            roots=roots,
            names=self.names.text().strip() or "*",
            name_regex=self.name_regex.isChecked(),
            name_case=self.name_case.isChecked(),
            max_depth=DEPTHS[self.depth.currentIndex()][1],
            kinds=KINDS[self.kinds.currentIndex()][1],
            include_hidden=self.include_hidden.isChecked(),
            search_archives=self.search_archives.isChecked(),
            text=self.text.text(),
            text_regex=self.text_regex.isChecked(),
            text_case=self.text_case.isChecked(),
            whole_words=self.whole_words.isChecked(),
            not_containing=self.not_containing.isChecked(),
            include_online_only=self.include_online_only.isChecked(),
            min_size=min_size,
            max_size=max_size,
            newer_than=now - newer_days * DAY if newer_days else None,
            older_than=now - older_days * DAY if older_days else None,
        )
        self._form = replace(
            self._form,
            names=self.names.text(), name_regex=self.name_regex.isChecked(), name_case=self.name_case.isChecked(),
            depth=self.depth.currentIndex(), kinds=self.kinds.currentIndex(),
            include_hidden=self.include_hidden.isChecked(), search_archives=self.search_archives.isChecked(),
            text=self.text.text(), text_regex=self.text_regex.isChecked(), text_case=self.text_case.isChecked(),
            whole_words=self.whole_words.isChecked(), not_containing=self.not_containing.isChecked(),
            include_online_only=self.include_online_only.isChecked(),
            min_size=self.min_size.text(), max_size=self.max_size.text(), date=self.date.currentIndex(),
        )
        self.accept()

    def ask(self) -> tuple[Query, Form] | None:
        if self.exec() != QDialog.DialogCode.Accepted or self.query is None:
            return None
        return self.query, self._form
