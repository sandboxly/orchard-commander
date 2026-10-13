# Settings

To open the Settings window, press **⌘,** / **Ctrl+,**, or choose
**Options → Settings…** (on macOS also **Orchard Commander → Settings…**).

Changes take effect immediately and are saved automatically. Close the window
with **Close**.

The window has three pages: **Appearance**, **Preview** and **Terminal**.

## Appearance

The **Sample** panel on the right shows the effect of your changes.

### Color scheme

| Setting | Description |
| --- | --- |
| Scheme | The color scheme in use |
| Follow the system's light / dark mode | Use one scheme in light mode and another in dark mode, switching with the system |
| Light mode / Dark mode | The schemes to use when following the system |
| Edit… | Change the colors of the scheme next to it (see [Custom color schemes](#custom-color-schemes)) |

Built-in schemes: `monokai` (default), `dracula`, `nord`, `gruvbox`,
`tokyo-night`, `commander` (white on blue, the classic look),
`solarized-light` and `paper`.

To try schemes quickly, choose **Options → Choose color scheme…**. The
window shows each scheme as you move through the list. Press **Enter** to
keep the selected scheme, or **Esc** to return to the previous one.

### Font

| Setting | Description |
| --- | --- |
| Typeface | The font for the whole window, including the terminal. The list contains monospaced fonts. |
| Include proportional fonts | Also list fonts that aren't monospaced |
| Style | For example Regular, Light or Medium, depending on the font |
| Size | 7 to 40 points. You can also use **⌘=**, **⌘-** and **⌘0** / **Ctrl+=**, **Ctrl+-** and **Ctrl+0**. |
| Line spacing | The height of every row in the window, from 0.8 to 2.0 times the font height |

### Window

| Setting | Description |
| --- | --- |
| Frames | The style of the panel borders: Rounded, Single line, Double line or Heavy |
| Always show the menu bar | Show the menu bar in the window at all times. When this is off, press **F9** to show it. |

**Restore defaults** resets all appearance settings.

## Preview

| Setting | Description |
| --- | --- |
| Start playing as soon as a file is selected (autoplay) | Audio and video start without pressing Space. Same as the **Autoplay** switch in the player. |
| Play the next file when one ends | Select and play the next audio or video file in the panel. When this is off, videos repeat. |

## Terminal

| Setting | Description |
| --- | --- |
| Shell | The command that starts the shell. Leave empty to use your login shell (PowerShell on Windows). Applies to new sessions. |
| Link each side's terminal and panel folders | Changing the folder in the shell moves the panel, and moving the panel changes the shell's folder |

## Custom color schemes

### Editing a scheme

1. In **Settings → Appearance**, click **Edit…** next to the scheme.
2. Change the **Name** if you want to save the scheme under a new name.
3. Click a color swatch to open the color picker, or type a hex value such
   as `#272822`. The picker has basic colors, a hue and saturation field, a
   brightness bar and a field for the hex value. The window shows the change
   immediately.
4. Choose **Syntax colors** for highlighted source code in previews.
5. Select **Dark scheme** if the scheme has a dark background.
6. Click **Save**.

Changes to a built-in scheme are saved as a new scheme; the built-in scheme
stays unchanged. To delete one of your own schemes, open it with **Edit…**
and click **Delete**.

### Scheme files

Your color schemes are stored as TOML files in the `color-schemes` folder of
the settings folder (see below). You can copy these files to another
computer, or write them yourself. A scheme file looks like this:

```toml
name = "harbor"
dark = true
pygments_style = "monokai"

background = "#1e2127"
surface = "#262a31"
foreground = "#d7dae0"
accent = "#e5c07b"
border_active = "#61afef"
```

Colors you leave out are taken from the default dark or light scheme,
depending on `dark`. The available colors are:

| Key | Used for |
| --- | --- |
| `background` | Window background |
| `surface` | Panel background |
| `foreground` | Text |
| `muted` | Secondary text |
| `accent` | Headers and titles |
| `border` | Border |
| `border_active` | Border of the active panel |
| `cursor_bg` | Cursor |
| `cursor_fg` | Text under the cursor |
| `cursor_inactive_bg` | Cursor in the inactive panel |
| `dir` | Folders |
| `exec` | Executable files |
| `link` | Symbolic links |
| `hidden` | Hidden files |
| `marked` | Marked files |
| `error` | Error messages |
| `warning` | Warnings |
| `success` | Success messages |
| `footer_bg` | Function-key bar background |
| `footer_fg` | Function-key bar text |
| `footer_key` | Function-key bar keys |

`pygments_style` is the name of a Pygments style, which colors source code
in previews.

## Where settings are stored

| System | Settings folder |
| --- | --- |
| macOS | `~/Library/Application Support/Orchard Commander` |
| Windows | `%LOCALAPPDATA%\Orchard Commander` |
| Linux | `~/.config/orchard-commander` |

The folder contains:

| File | Contents |
| --- | --- |
| `settings.json` | Settings, last folders, window size and position |
| `bookmarks.json` | Bookmarks |
| `connections.json` | Saved server connections (without passwords) |
| `known_hosts` | SFTP server keys you have trusted |
| `color-schemes/` | Your color schemes |

Passwords are stored in the system keychain, not in these files.

To reset Orchard Commander to its defaults, quit the app and delete
`settings.json`.

## About and licenses

**Options → About Orchard Commander…** (on macOS **Orchard Commander →
About Orchard Commander**) shows the version and the third-party components.
Click **Licenses…** to open the folder with their license texts.
