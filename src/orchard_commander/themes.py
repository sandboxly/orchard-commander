"""The look of the app: color schemes, fonts and frame styles.

A theme is the whole appearance (`Appearance`): a color scheme (or one for
light and one for dark system mode), a font, line spacing and a frame style.
Color schemes are named, shareable data: the built-ins below plus TOML files in
the config directory's `color-schemes/` folder, which the scheme editor writes.
The UI derives its whole look (Qt style sheet, panel borders, file colors,
syntax highlighting) from a scheme's colors.
"""

from __future__ import annotations

import json
import re
import tomllib
from dataclasses import dataclass, replace
from pathlib import Path


@dataclass(frozen=True, slots=True)
class ColorScheme:
    name: str
    dark: bool
    background: str  # window background, around the panels
    surface: str  # inside panels and dialogs
    foreground: str
    muted: str  # secondary text, inactive borders' titles
    border: str  # inactive panel border
    border_active: str  # active panel border and title chip
    accent: str  # column headers, dialog titles
    cursor_bg: str
    cursor_fg: str
    cursor_inactive_bg: str  # cursor in the panel without focus
    dir: str
    exec: str
    link: str
    hidden: str
    marked: str
    error: str
    warning: str
    success: str
    footer_bg: str
    footer_fg: str
    footer_key: str
    pygments_style: str


# Every color of a scheme: (field, label, group), in the order the scheme editor shows them.
COLOR_ROLES: list[tuple[str, str, str]] = [
    ("background", "Window background", "General"),
    ("surface", "Panel background", "General"),
    ("foreground", "Text", "General"),
    ("muted", "Secondary text", "General"),
    ("accent", "Headers and titles", "General"),
    ("border", "Border", "General"),
    ("border_active", "Active border", "General"),
    ("cursor_bg", "Cursor", "Cursor"),
    ("cursor_fg", "Cursor text", "Cursor"),
    ("cursor_inactive_bg", "Cursor, inactive panel", "Cursor"),
    ("dir", "Directories", "Files"),
    ("exec", "Executables", "Files"),
    ("link", "Symbolic links", "Files"),
    ("hidden", "Hidden files", "Files"),
    ("marked", "Marked files", "Files"),
    ("error", "Errors", "Messages"),
    ("warning", "Warnings", "Messages"),
    ("success", "Success", "Messages"),
    ("footer_bg", "Key bar background", "Key bar"),
    ("footer_fg", "Key bar text", "Key bar"),
    ("footer_key", "Key bar keys", "Key bar"),
]


BUILTIN_SCHEMES: dict[str, ColorScheme] = {
    t.name: t
    for t in [
        ColorScheme(
            name="monokai",
            dark=True,
            background="#1e1f1c",
            surface="#272822",
            foreground="#f8f8f2",
            muted="#75715e",
            border="#49483e",
            border_active="#a6e22e",
            accent="#fd971f",
            cursor_bg="#66d9ef",
            cursor_fg="#1e1f1c",
            cursor_inactive_bg="#3e3d32",
            dir="#66d9ef",
            exec="#a6e22e",
            link="#ae81ff",
            hidden="#75715e",
            marked="#e6db74",
            error="#f92672",
            warning="#fd971f",
            success="#a6e22e",
            footer_bg="#3e3d32",
            footer_fg="#f8f8f2",
            footer_key="#fd971f",
            pygments_style="monokai",
        ),
        ColorScheme(
            name="dracula",
            dark=True,
            background="#21222c",
            surface="#282a36",
            foreground="#f8f8f2",
            muted="#6272a4",
            border="#44475a",
            border_active="#bd93f9",
            accent="#ff79c6",
            cursor_bg="#bd93f9",
            cursor_fg="#282a36",
            cursor_inactive_bg="#44475a",
            dir="#8be9fd",
            exec="#50fa7b",
            link="#ff79c6",
            hidden="#6272a4",
            marked="#f1fa8c",
            error="#ff5555",
            warning="#ffb86c",
            success="#50fa7b",
            footer_bg="#44475a",
            footer_fg="#f8f8f2",
            footer_key="#ff79c6",
            pygments_style="dracula",
        ),
        ColorScheme(
            name="nord",
            dark=True,
            background="#242933",
            surface="#2e3440",
            foreground="#d8dee9",
            muted="#616e88",
            border="#434c5e",
            border_active="#88c0d0",
            accent="#ebcb8b",
            cursor_bg="#88c0d0",
            cursor_fg="#2e3440",
            cursor_inactive_bg="#3b4252",
            dir="#81a1c1",
            exec="#a3be8c",
            link="#b48ead",
            hidden="#616e88",
            marked="#ebcb8b",
            error="#bf616a",
            warning="#d08770",
            success="#a3be8c",
            footer_bg="#3b4252",
            footer_fg="#d8dee9",
            footer_key="#88c0d0",
            pygments_style="nord",
        ),
        ColorScheme(
            name="gruvbox",
            dark=True,
            background="#1d2021",
            surface="#282828",
            foreground="#ebdbb2",
            muted="#928374",
            border="#504945",
            border_active="#fabd2f",
            accent="#fe8019",
            cursor_bg="#fabd2f",
            cursor_fg="#282828",
            cursor_inactive_bg="#3c3836",
            dir="#83a598",
            exec="#b8bb26",
            link="#d3869b",
            hidden="#928374",
            marked="#fe8019",
            error="#fb4934",
            warning="#fe8019",
            success="#b8bb26",
            footer_bg="#3c3836",
            footer_fg="#ebdbb2",
            footer_key="#fabd2f",
            pygments_style="gruvbox-dark",
        ),
        ColorScheme(
            name="tokyo-night",
            dark=True,
            background="#16161e",
            surface="#1a1b26",
            foreground="#c0caf5",
            muted="#565f89",
            border="#292e42",
            border_active="#7aa2f7",
            accent="#bb9af7",
            cursor_bg="#7aa2f7",
            cursor_fg="#1a1b26",
            cursor_inactive_bg="#292e42",
            dir="#7aa2f7",
            exec="#9ece6a",
            link="#7dcfff",
            hidden="#565f89",
            marked="#e0af68",
            error="#f7768e",
            warning="#e0af68",
            success="#9ece6a",
            footer_bg="#24283b",
            footer_fg="#c0caf5",
            footer_key="#bb9af7",
            pygments_style="one-dark",
        ),
        ColorScheme(
            name="commander",
            dark=True,
            background="#000060",
            surface="#000080",
            foreground="#55ffff",
            muted="#5f87af",
            border="#00a8a8",
            border_active="#55ffff",
            accent="#ffff55",
            cursor_bg="#00a8a8",
            cursor_fg="#000000",
            cursor_inactive_bg="#005f87",
            dir="#ffffff",
            exec="#55ff55",
            link="#ff55ff",
            hidden="#5f87af",
            marked="#ffff55",
            error="#ff5555",
            warning="#ffff55",
            success="#55ff55",
            footer_bg="#00a8a8",
            footer_fg="#000000",
            footer_key="#ffffff",
            pygments_style="native",
        ),
        ColorScheme(
            name="solarized-light",
            dark=False,
            background="#eee8d5",
            surface="#fdf6e3",
            foreground="#586e75",
            muted="#93a1a1",
            border="#d3cbb7",
            border_active="#268bd2",
            accent="#cb4b16",
            cursor_bg="#268bd2",
            cursor_fg="#fdf6e3",
            cursor_inactive_bg="#e4ddc8",
            dir="#268bd2",
            exec="#859900",
            link="#6c71c4",
            hidden="#93a1a1",
            marked="#cb4b16",
            error="#dc322f",
            warning="#b58900",
            success="#859900",
            footer_bg="#e4ddc8",
            footer_fg="#586e75",
            footer_key="#cb4b16",
            pygments_style="solarized-light",
        ),
        ColorScheme(
            name="paper",
            dark=False,
            background="#e8e8e3",
            surface="#fafaf7",
            foreground="#1c1c1c",
            muted="#8a8a8a",
            border="#c8c8c0",
            border_active="#005f87",
            accent="#af5f00",
            cursor_bg="#005f87",
            cursor_fg="#ffffff",
            cursor_inactive_bg="#dcdcd4",
            dir="#005f87",
            exec="#008700",
            link="#8700af",
            hidden="#8a8a8a",
            marked="#d75f00",
            error="#d70000",
            warning="#af5f00",
            success="#008700",
            footer_bg="#dcdcd4",
            footer_fg="#1c1c1c",
            footer_key="#af5f00",
            pygments_style="default",
        ),
    ]
}


DEFAULT_SCHEME = "monokai"
DEFAULT_LIGHT_SCHEME = "solarized-light"


def is_builtin(name: str) -> bool:
    return name in BUILTIN_SCHEMES


# -- user color schemes ------------------------------------------------------

_COLOR = re.compile(r"#[0-9a-fA-F]{6}")


def is_color(value: object) -> bool:
    """A "#rrggbb" color, as schemes store them."""
    return isinstance(value, str) and _COLOR.fullmatch(value) is not None


def user_scheme_dir() -> Path:
    from orchard_commander import config

    return config.CONFIG_FILE.parent / "color-schemes"


def scheme_from_dict(data: dict, fallback_name: str = "") -> ColorScheme:
    """A scheme from parsed TOML. Missing or invalid colors come from a built-in
    scheme of the same brightness, so files stay valid when roles are added."""
    dark = data.get("dark", True) is not False
    base = BUILTIN_SCHEMES[DEFAULT_SCHEME if dark else DEFAULT_LIGHT_SCHEME]
    colors = {
        role: value.lower()
        for role, _, _ in COLOR_ROLES
        if is_color(value := data.get(role))
    }
    style = data.get("pygments_style")
    return replace(
        base,
        name=str(data.get("name") or fallback_name).strip(),
        dark=dark,
        pygments_style=style if isinstance(style, str) and style else base.pygments_style,
        **colors,
    )


def scheme_to_toml(scheme: ColorScheme) -> str:
    def text(value: str) -> str:
        return json.dumps(value, ensure_ascii=False)  # a valid TOML basic string

    lines = [
        f"# {scheme.name}: an Orchard Commander color scheme",
        f"name = {text(scheme.name)}",
        f"dark = {'true' if scheme.dark else 'false'}",
        f"pygments_style = {text(scheme.pygments_style)}  # syntax highlighting in text previews",
        "",
    ]
    width = max(len(role) for role, _, _ in COLOR_ROLES)
    lines += [f"{role:<{width}} = {text(getattr(scheme, role))}  # {label}" for role, label, _ in COLOR_ROLES]
    return "\n".join(lines) + "\n"


def _user_schemes() -> dict[str, tuple[ColorScheme, Path]]:
    found: dict[str, tuple[ColorScheme, Path]] = {}
    directory = user_scheme_dir()
    if not directory.is_dir():
        return found
    for path in sorted(directory.glob("*.toml")):
        try:
            data = tomllib.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError):
            continue
        scheme = scheme_from_dict(data, path.stem)
        if scheme.name and not is_builtin(scheme.name) and scheme.name not in found:
            found[scheme.name] = (scheme, path)
    return found


def all_schemes() -> dict[str, ColorScheme]:
    """Built-in schemes, then the user's, by name. Read from disk on every call:
    another running instance may have saved one."""
    return {**BUILTIN_SCHEMES, **{name: s for name, (s, _) in _user_schemes().items()}}


def get_scheme(name: str) -> ColorScheme:
    return all_schemes().get(name) or BUILTIN_SCHEMES[DEFAULT_SCHEME]


def save_user_scheme(scheme: ColorScheme, replaces: str | None = None) -> Path:
    """Write `scheme` to the user's scheme folder. `replaces` is the user scheme
    being edited (it may be renamed). Raises ValueError for a taken name."""
    name = scheme.name.strip()
    if not name:
        raise ValueError("A color scheme needs a name")
    if is_builtin(name):
        raise ValueError(f"'{name}' is a built-in color scheme")
    existing = _user_schemes()
    if name in existing and name != replaces:
        raise ValueError(f"A color scheme named '{name}' already exists")
    if replaces in existing and replaces != name:
        existing[replaces][1].unlink(missing_ok=True)
    if name in existing:
        path = existing[name][1]
    else:
        slug = re.sub(r"[^a-z0-9]+", "-", name.casefold()).strip("-") or "scheme"
        path = user_scheme_dir() / f"{slug}.toml"
        n = 2
        while path.exists():
            path = path.with_name(f"{slug}-{n}.toml")
            n += 1
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(scheme_to_toml(replace(scheme, name=name)), encoding="utf-8")
    return path


def delete_user_scheme(name: str) -> None:
    entry = _user_schemes().get(name)
    if entry is not None:
        entry[1].unlink(missing_ok=True)


# -- the whole appearance ----------------------------------------------------

FRAME_STYLES = {"rounded": "Rounded", "single": "Single line", "double": "Double line", "heavy": "Heavy"}
DEFAULT_FRAME_STYLE = "rounded"
DEFAULT_FONT_SIZE = 13
FONT_SIZE_RANGE = (7, 40)
DEFAULT_LINE_SPACING = 1.0
LINE_SPACING_RANGE = (0.8, 2.0)


@dataclass(frozen=True, slots=True)
class Appearance:
    """What the user chose; `Settings.appearance()` fills in the defaults."""

    color_scheme: str = DEFAULT_SCHEME
    follow_system_appearance: bool = False  # use light_scheme / dark_scheme by the OS mode
    light_scheme: str = DEFAULT_LIGHT_SCHEME
    dark_scheme: str = DEFAULT_SCHEME
    font_family: str = ""  # empty: the first installed of a list of good monospace fonts
    font_style: str = ""  # e.g. "Light", "Medium"; empty: regular
    font_size: int = DEFAULT_FONT_SIZE
    line_spacing: float = DEFAULT_LINE_SPACING  # file list row height, as a multiple of the font's
    frame_style: str = DEFAULT_FRAME_STYLE

    def scheme_setting(self, system_dark: bool) -> str:
        """The name of the setting that holds the scheme in use."""
        if not self.follow_system_appearance:
            return "color_scheme"
        return "dark_scheme" if system_dark else "light_scheme"

    def scheme_name(self, system_dark: bool) -> str:
        return getattr(self, self.scheme_setting(system_dark))
