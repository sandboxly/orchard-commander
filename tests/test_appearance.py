"""Appearance: settings, color scheme files, fonts, the settings window, menus."""

from __future__ import annotations

import json
from dataclasses import fields, replace

import pytest
from PySide6.QtCore import Qt
from PySide6.QtGui import QAction, QFontDatabase
from PySide6.QtWidgets import QApplication

from orchard_commander import config, themes
from orchard_commander.config import Settings
from orchard_commander.ui.boxframe import BoxFrame
from orchard_commander.ui.main_window import MainWindow
from orchard_commander.ui.settings_dialog import SchemeEditor, SettingsDialog
from orchard_commander.ui.style import ThemeManager, default_monospace_font, resolve_font

from test_ui import wait_until



# -- settings ------------------------------------------------------------------------------------


def test_old_theme_and_font_size_settings_are_migrated():
    config.CONFIG_FILE.write_text(json.dumps({"theme": "nord", "extra": {"font_size": 15, "geometry": "x"}}))
    s = Settings.load()
    assert s.color_scheme == "nord" and s.font_size == 15
    assert s.extra == {"geometry": "x"}
    s.update(frame_style="double")
    on_disk = json.loads(config.CONFIG_FILE.read_text())
    assert "theme" not in on_disk and on_disk["color_scheme"] == "nord"


def test_appearance_defaults_and_limits():
    assert Settings().appearance() == themes.Appearance()
    a = Settings(font_size=99, line_spacing=0.1, frame_style="bogus", color_scheme="nord").appearance()
    assert a.font_size == themes.FONT_SIZE_RANGE[1]
    assert a.line_spacing == themes.LINE_SPACING_RANGE[0]
    assert a.frame_style == themes.DEFAULT_FRAME_STYLE
    assert a.scheme_name(system_dark=False) == "nord"  # one scheme for both modes
    a = replace(a, follow_system_appearance=True, light_scheme="paper", dark_scheme="dracula")
    assert a.scheme_name(system_dark=False) == "paper"
    assert a.scheme_name(system_dark=True) == "dracula"
    assert a.scheme_setting(system_dark=True) == "dark_scheme"


# -- color scheme files --------------------------------------------------------------------------


def test_every_color_has_an_editor_role():
    colors = {f.name for f in fields(themes.ColorScheme)} - {"name", "dark", "pygments_style"}
    assert colors == {role for role, _, _ in themes.COLOR_ROLES}


def test_user_scheme_round_trip():
    scheme = replace(themes.get_scheme("nord"), name="My Nord", dir="#123456", pygments_style="monokai")
    path = themes.save_user_scheme(scheme)
    assert path.parent == themes.user_scheme_dir() and path.suffix == ".toml"
    assert themes.get_scheme("My Nord") == scheme
    assert list(themes.all_schemes())[: len(themes.BUILTIN_SCHEMES)] == list(themes.BUILTIN_SCHEMES)


def test_user_scheme_files_tolerate_missing_and_bad_values():
    directory = themes.user_scheme_dir()
    directory.mkdir(parents=True)
    (directory / "partial.toml").write_text('name = "Partial"\ndark = false\ndir = "#00ff00"\nerror = "red"\n')
    (directory / "broken.toml").write_text("name = [unclosed")
    (directory / "builtin.toml").write_text('name = "nord"\n')  # can't shadow a built-in
    partial = themes.get_scheme("Partial")
    base = themes.BUILTIN_SCHEMES[themes.DEFAULT_LIGHT_SCHEME]
    assert partial.dir == "#00ff00" and not partial.dark
    assert partial.error == base.error and partial.surface == base.surface
    assert themes.get_scheme("nord") == themes.BUILTIN_SCHEMES["nord"]
    assert set(themes.all_schemes()) == set(themes.BUILTIN_SCHEMES) | {"Partial"}


def test_user_scheme_names_rename_and_delete():
    base = themes.get_scheme("monokai")
    with pytest.raises(ValueError):
        themes.save_user_scheme(base)  # a built-in name
    themes.save_user_scheme(replace(base, name="One"))
    themes.save_user_scheme(replace(base, name="Two"))
    with pytest.raises(ValueError):
        themes.save_user_scheme(replace(base, name="Two"), replaces="One")
    themes.save_user_scheme(replace(base, name="Uno"), replaces="One")
    assert {"Uno", "Two"} <= set(themes.all_schemes()) and "One" not in themes.all_schemes()
    themes.delete_user_scheme("Uno")
    assert "Uno" not in themes.all_schemes()
    assert len(list(themes.user_scheme_dir().glob("*.toml"))) == 1


# -- theme manager -------------------------------------------------------------------------------


def test_fonts_and_line_spacing(qapp):
    fixed = next((f for f in QFontDatabase.families() if QFontDatabase.isFixedPitch(f)), None)
    if fixed is None:  # Qt's offscreen platform on Windows lists no fixed-pitch fonts
        pytest.skip("no fixed-pitch fonts in this font database")
    assert resolve_font(fixed, "", 15).family() == fixed
    assert resolve_font(fixed, "", 15).pointSize() == 15
    assert resolve_font("No Such Font", "", 12).family() == default_monospace_font(12).family()
    manager = ThemeManager()
    single = manager.row_height()
    manager.set_appearance(replace(manager.appearance, line_spacing=2.0))
    assert manager.row_height() >= 2 * single - 1


def test_color_preview_is_temporary(qapp):
    manager = ThemeManager(themes.Appearance(color_scheme="nord"))
    manager.preview_colors(themes.get_scheme("paper"))
    assert manager.colors.name == "paper"
    manager.preview_colors(None)
    assert manager.colors.name == "nord"


@pytest.mark.parametrize("style", list(themes.FRAME_STYLES))
def test_frame_styles_paint(qapp, style):
    manager = ThemeManager(themes.Appearance(frame_style=style))
    frame = BoxFrame(manager)
    frame.set_title("title")
    frame.set_subtitle("subtitle")
    frame.resize(300, 200)
    for active in (False, True):
        frame.set_active(active)
        assert not frame.grab().isNull()


# -- settings window -----------------------------------------------------------------------------


def test_settings_apply_live_and_persist(qtbot, window: MainWindow):
    dialog = SettingsDialog(window, window.themes, window.settings, window.apply_settings)
    qtbot.addWidget(dialog)
    page = dialog.appearance

    page.size.setValue(17)
    assert window.themes.font.pointSize() == 17
    page.frame.setCurrentIndex(page.frame.findData("double"))
    page.frame.activated.emit(page.frame.currentIndex())
    assert window.themes.frame_style == "double"
    combo = page.scheme_rows["color_scheme"][1]
    combo.setCurrentIndex(combo.findData("dracula"))
    combo.activated.emit(combo.currentIndex())
    assert window.themes.colors.name == "dracula"

    saved = Settings.load()
    assert (saved.font_size, saved.frame_style, saved.color_scheme) == (17, "double", "dracula")

    page.follow_system.setChecked(True)
    assert page.scheme_rows["color_scheme"][1].isHidden()
    assert not page.scheme_rows["dark_scheme"][1].isHidden()

    page.restore_defaults()
    assert Settings.load().appearance() == themes.Appearance()
    assert window.themes.font.pointSize() == themes.DEFAULT_FONT_SIZE


def test_zoom_saves_font_size(qtbot, window: MainWindow):
    window.zoom(+2)
    assert Settings.load().font_size == themes.DEFAULT_FONT_SIZE + 2
    window.zoom(0)
    assert Settings.load().font_size is None and window.themes.font_size == themes.DEFAULT_FONT_SIZE


def test_scheme_editor_saves_builtin_as_copy(qtbot, window: MainWindow):
    editor = SchemeEditor(window, window.themes, themes.get_scheme("nord"))
    qtbot.addWidget(editor)
    assert editor.name.text() == "nord (custom)"
    window.themes.preview_colors(editor.scheme)  # as edit() does
    _, hex_edit = editor.swatches["dir"]
    hex_edit.setText("#ABCDEF")
    hex_edit.textEdited.emit("#ABCDEF")
    assert window.themes.colors.dir == "#abcdef"  # live preview
    editor.save()
    assert editor.result_ == ("saved", "nord (custom)")
    assert themes.get_scheme("nord (custom)").dir == "#abcdef"
    assert window.themes.colors.name == "monokai"  # preview ended
    assert themes.BUILTIN_SCHEMES["nord"].dir != "#abcdef"


def test_scheme_editor_cancel_restores_colors(qtbot, window: MainWindow):
    editor = SchemeEditor(window, window.themes, themes.get_scheme("monokai"))
    qtbot.addWidget(editor)
    window.themes.preview_colors(editor.scheme)  # as edit() does
    editor.dark.setChecked(False)
    editor._set(surface="#ffffff")
    assert window.themes.colors.surface == "#ffffff"
    editor.reject()
    assert window.themes.colors == themes.get_scheme("monokai")


def test_edit_scheme_from_settings_selects_the_copy(qtbot, window: MainWindow, monkeypatch):
    dialog = SettingsDialog(window, window.themes, window.settings, window.apply_settings)
    qtbot.addWidget(dialog)

    def fake_edit(self):
        themes.save_user_scheme(replace(self.scheme, name="Mine"))
        return ("saved", "Mine")

    monkeypatch.setattr(SchemeEditor, "edit", fake_edit)
    dialog.appearance.edit_scheme("color_scheme")
    assert Settings.load().color_scheme == "Mine" and window.themes.colors.name == "Mine"
    combo = dialog.appearance.scheme_rows["color_scheme"][1]
    assert combo.currentData() == "Mine"

    monkeypatch.setattr(SchemeEditor, "edit", lambda self: ("deleted", "Mine"))
    dialog.appearance.edit_scheme("color_scheme")
    assert Settings.load().color_scheme == "" and window.themes.colors.name == themes.DEFAULT_SCHEME


# -- menus ---------------------------------------------------------------------------------------


def test_menu_bar_has_the_commands_and_f9_shows_it(qtbot, window: MainWindow):
    strip = window.menu_strip
    titles = [a.text().replace("&", "") for a in strip.actions()]
    assert titles == ["Left", "Files", "Commands", "Options", "Right"]
    options = strip.actions()[3].menu()
    assert "Settings…" in [a.text() for a in options.actions()]
    assert window.actions_by_title["Settings…"].menuRole() == QAction.MenuRole.PreferencesRole
    assert window.actions_by_title["Quit"].menuRole() == QAction.MenuRole.QuitRole

    assert strip.isHidden()
    qtbot.keyClick(QApplication.focusWidget() or window, Qt.Key.Key_F9)
    wait_until(lambda: strip.isVisible() and strip.activeAction() is strip.actions()[0])
    strip.activeAction().menu().hide()
    wait_until(lambda: strip.isHidden())


def test_menu_bar_can_stay_visible(qtbot, window: MainWindow):
    window.apply_settings(menu_bar_visible=True)
    assert window.menu_strip.isVisible()
    window.apply_settings(menu_bar_visible=None)
    assert window.menu_strip.isHidden()
