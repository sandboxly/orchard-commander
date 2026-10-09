"""Persistent user settings, stored as JSON in the platform config directory."""

from __future__ import annotations

import json
import shutil
import sys
from dataclasses import asdict, dataclass, field, fields
from pathlib import Path

from platformdirs import user_config_dir

from orchard_commander import APP_NAME, COMMAND

DEFAULT_THEME = "monokai"
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
    theme: str = ""  # empty: use DEFAULT_THEME
    show_hidden: bool | None = None  # None: DEFAULT_SHOW_HIDDEN (only a toggle stores a choice)
    left_path: str = ""
    right_path: str = ""
    extra: dict = field(default_factory=dict)

    @property
    def hidden_visible(self) -> bool:
        return DEFAULT_SHOW_HIDDEN if self.show_hidden is None else self.show_hidden

    @classmethod
    def load(cls, path: Path | None = None) -> Settings:
        path = path or CONFIG_FILE
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return cls()
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
