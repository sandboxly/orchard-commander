"""Persistent user settings, stored as JSON in the platform config directory."""

from __future__ import annotations

import json
import shutil
import sys
from dataclasses import asdict, dataclass, field, fields
from pathlib import Path

from platformdirs import user_config_dir

from orchard_commander import APP_NAME, COMMAND, themes

DEFAULT_SHOW_HIDDEN = True

# "Orchard Commander" on macOS/Windows; Linux config dirs conventionally avoid spaces.
CONFIG_FILE = Path(user_config_dir(COMMAND if sys.platform.startswith("linux") else APP_NAME, appauthor=False)) / (
    "settings.json"
)
LEGACY_CONFIG_DIR = Path(user_config_dir("fcmd", appauthor=False))  # before the rename


def migrate_legacy_config() -> bool:
    """Copy settings, saved connections and trusted host keys from the old "fcmd"
    config directory, once, if the new one doesn't exist yet. The old one is kept."""
    new_dir = CONFIG_FILE.parent
    if new_dir.exists() or not LEGACY_CONFIG_DIR.is_dir():
        return False
    shutil.copytree(LEGACY_CONFIG_DIR, new_dir)
    return True


@dataclass
class Settings:
    # Appearance; empty / None: the default in themes.Appearance.
    color_scheme: str = ""
    follow_system_appearance: bool | None = None
    light_scheme: str = ""
    dark_scheme: str = ""
    font_family: str = ""
    font_style: str = ""
    font_size: int | None = None
    line_spacing: float | None = None
    frame_style: str = ""
    menu_bar_visible: bool | None = None  # in-window menu bar always shown (otherwise F9 shows it)

    show_hidden: bool | None = None  # None: DEFAULT_SHOW_HIDDEN (only a toggle stores a choice)
    play_next: bool | None = None  # previews: play the next audio/video file when one ends (None: on)
    terminal_shell: str = ""  # the terminal panel's shell command; "": the user's login shell / PowerShell
    terminal_link: bool | None = None  # a side's shell and panel follow each other's folder (None: on)
    left_path: str = ""
    right_path: str = ""
    extra: dict = field(default_factory=dict)

    @property
    def autoplay_enabled(self) -> bool:
        """Audio/video previews start playing when selected (the player's own toggle, shared)."""
        return bool(self.extra.get("preview.media.autoplay", True))

    @property
    def terminal_link_enabled(self) -> bool:
        return True if self.terminal_link is None else bool(self.terminal_link)

    @property
    def play_next_enabled(self) -> bool:
        return True if self.play_next is None else bool(self.play_next)

    @property
    def hidden_visible(self) -> bool:
        return DEFAULT_SHOW_HIDDEN if self.show_hidden is None else self.show_hidden

    def appearance(self) -> themes.Appearance:
        """The chosen appearance, with defaults for whatever wasn't chosen."""
        default = themes.Appearance()
        lo, hi = themes.FONT_SIZE_RANGE
        spacing_lo, spacing_hi = themes.LINE_SPACING_RANGE
        return themes.Appearance(
            color_scheme=self.color_scheme or default.color_scheme,
            follow_system_appearance=bool(self.follow_system_appearance),
            light_scheme=self.light_scheme or default.light_scheme,
            dark_scheme=self.dark_scheme or default.dark_scheme,
            font_family=self.font_family,
            font_style=self.font_style,
            font_size=default.font_size if self.font_size is None else max(lo, min(int(self.font_size), hi)),
            line_spacing=(
                default.line_spacing if self.line_spacing is None
                else max(spacing_lo, min(float(self.line_spacing), spacing_hi))
            ),
            frame_style=self.frame_style if self.frame_style in themes.FRAME_STYLES else default.frame_style,
        )

    @classmethod
    def load(cls, path: Path | None = None) -> Settings:
        path = path or CONFIG_FILE
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return cls()
        if not isinstance(data, dict):
            return cls()
        # Before appearance settings: "theme" was the color scheme, the font size an extra.
        if "theme" in data:
            data.setdefault("color_scheme", data.pop("theme"))
        extra = data.get("extra")
        if isinstance(extra, dict) and "font_size" in extra:
            data["extra"] = {k: v for k, v in extra.items() if k != "font_size"}
            data.setdefault("font_size", extra["font_size"])
        known = {f.name for f in fields(cls)}
        return cls(**{k: v for k, v in data.items() if k in known})

    def update(self, **changes: object) -> None:
        """Apply `changes` and persist only those keys.

        The file is re-read first, so several running instances don't
        overwrite each other's unrelated settings with stale values.
        """
        for key, value in changes.items():
            setattr(self, key, value)
        on_disk = Settings.load()
        for key, value in changes.items():
            setattr(on_disk, key, value)
        on_disk.save()

    def save(self, path: Path | None = None) -> None:
        path = path or CONFIG_FILE
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(asdict(self), indent=2), encoding="utf-8")
        except OSError:
            pass  # settings are a convenience; never crash over them
