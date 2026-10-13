# The terminal panel

The terminal panel runs your command-line shell below the two sides. Each
side has its own shell session, which follows that side's folder.

## Opening and closing the terminal

| Action | macOS | Windows / Linux |
| --- | --- | --- |
| Show the terminal, or move the keyboard focus into it | ⌘J or ⌃\` | Ctrl+\` |
| Hide the terminal (while typing in it) | ⌘J or ⌃\` | Ctrl+\` |

On macOS, ⌃ is the Control key. On keyboard layouts where \` is a dead key,
such as German, use **⌘J**.

You can also choose **Commands → Terminal**.

When the terminal is open, click a panel to work in it again; the terminal
stays open. To change the height of the terminal, drag the line between the
panels and the terminal. Orchard Commander remembers the height and whether
the terminal was open.

## One session per side

Each side has its own shell. The terminal shows the session of the active
side: when you switch sides with **Tab** or by clicking a panel, the
terminal switches to that side's session. The other session keeps running in
the background, so a command you started there continues.

The title of the terminal shows the shell and the side, for example
`Terminal · zsh · ◀ left`.

A side's shell starts the first time you show the terminal for that side, in
the folder of that side's panel.

## Panel and terminal folders

The shell and the panel of the same side follow each other:

- When you change the folder in the shell, for example with `cd`, the panel
  opens the same folder.
- When you open another folder in the panel, Orchard Commander types a `cd`
  command into the shell. Any text you had typed on the command line is
  cleared first.

Orchard Commander types the `cd` command only when the shell is waiting at
its prompt. While a program is running, such as an editor or a build, it
waits and changes the folder after the program ends.

The shell doesn't follow the panel to servers, into archives or into search
results.

After each command you run in the terminal, the panels showing local folders
are refreshed, so files you create, change or delete in the shell appear in
the panels.

To stop the panel and the shell from following each other, clear
**Link each side's terminal and panel folders** in **Settings → Terminal**.
You can then move the shell to the panel's folder at any time with
**⌃⇧\`** / **Ctrl+Shift+\`** (**Commands → Terminal: go to panel folder**).

## Keys in the terminal

While the terminal has the keyboard focus, all keys go to the shell and the
programs you run in it, including **Esc**, **Tab**, the function keys and
**Ctrl+C**. Orchard Commander's own shortcuts for these keys don't work until
you return to a panel.

These keys remain available to Orchard Commander:

- the terminal shortcut (**⌘J**, **⌃\`** / **Ctrl+\`**), which hides the
  terminal;
- on macOS, all ⌘ shortcuts, such as **⌘K** or **⌘P**.

| Action | macOS | Windows / Linux |
| --- | --- | --- |
| Copy the selected text | ⌘C | Ctrl+Shift+C |
| Paste | ⌘V | Ctrl+Shift+V |
| Send a control character, such as interrupt | ⌃C (Control key) | Ctrl+C |
| Move the cursor one word back / forward | ⌥← / ⌥→ | Alt+← / Alt+→ |
| Delete the previous word | ⌥⌫ | Alt+Backspace |

On macOS, the Option key types special characters, as in other Mac apps. On
Windows and Linux, the Alt key works as the Meta key.

Pasting several lines doesn't run them one by one if your shell supports
bracketed paste, which current versions of zsh, bash and fish do.

## Using the mouse

| Action | Result |
| --- | --- |
| Drag | Select text |
| Double-click | Select a word. File paths and web addresses count as one word. |
| Triple-click | Select the line |
| Middle-click | Paste |
| Scroll wheel | Scroll back through earlier output (up to 5,000 lines) |

Typing returns the view to the bottom.

Programs that support the mouse, such as `htop`, `vim` with mouse support
turned on, or `tmux`, receive clicks, drags and the scroll
wheel themselves. The mouse pointer changes to an arrow while a program uses
the mouse. To select text in such a program, hold **Shift** while you drag.

In full-screen programs that don't use the mouse, such as `less`, the scroll
wheel scrolls the program's content.

## Choosing a shell

By default the terminal runs:

- macOS and Linux: your login shell (the one set for your user account,
  usually zsh or bash), started as a login shell so your profile is loaded;
- Windows: PowerShell 7 (`pwsh`) if it is installed, otherwise Windows
  PowerShell.

To use a different shell, enter its command in **Settings → Terminal →
Shell**, for example `/opt/homebrew/bin/fish -l` or `cmd.exe`. The setting
applies to sessions started afterwards.

## Fonts and prompt icons

The terminal uses the font set in **Settings → Appearance**. Programming
fonts with ligatures, such as Fira Code, display their ligatures.

Prompt themes such as powerlevel10k, Starship and Oh My Posh use icons that
only Nerd Fonts contain. If your font lacks them, Orchard Commander uses an
installed Nerd Font for these icons, for example MesloLGS NF or FiraCode Nerd
Font. If no Nerd Font is installed, the icons appear as boxes; install one to
fix this.

## When the shell ends

When you leave the shell, for example with `exit`, the terminal shows that
the session has ended. Press **Enter** to start a new one.

Quitting Orchard Commander ends both shell sessions.
