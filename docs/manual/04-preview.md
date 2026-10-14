# Previewing files

## Opening the Preview view

The Preview view shows the file under the cursor in the other side's panel
and follows the cursor as you move it.

- Press **F3** to show the preview on the side you are not working in.
  Press **F3** again to return that side to its file list.
- Or click the **Preview** tab at the top of a side. Click **Files** to go
  back.

Only one side can show a preview at a time. The preview's title shows the
file name, and the bottom line shows the file size and type.

To use the keys of a preview, such as page turning in a PDF or the player
keys, press **Tab** to move to the preview side. Press **Tab** again to return
to the file list.

## What the preview shows

| File type | Preview |
| --- | --- |
| Text and source code | The text with syntax highlighting. Only the first 1 MB is shown. |
| Markdown | The rendered document. |
| Images (PNG, JPEG, GIF, WebP, SVG, TIFF, BMP; HEIC on macOS) | The image scaled to fit. Animated GIF and WebP images play. |
| PDF | The pages, fitted to the preview. |
| Audio (MP3, FLAC, WAV, Ogg, M4A and others) | Title and artist, the waveform of the file, a spectrum analyzer during playback, and player controls. |
| Video (MP4, MOV, MKV, WebM, AVI and others) | The video with player controls. |
| Folders and archives | A list of the contents. |
| Other files | File information and a hex dump of the beginning of the file. |

### Choosing another view

Some files can be shown in more than one way, for example a Markdown file as
the rendered document or as source text, and any file as a hex dump. The
available views appear as buttons at the top of the preview, under
**View as**.

- Click a button to switch the view.
- Or press **Shift+F3** to cycle through the views.

Orchard Commander remembers your choice for files with the same extension
until you quit.

## Images

Right-click an image to:

- **Open** it in the default application, or
- **Copy** the full-size image to the clipboard.

Very large images are decoded at reduced resolution for the preview.

## PDF documents

| Key | Action |
| --- | --- |
| → ↓ Page Down Space | Next page |
| ← ↑ Page Up Backspace | Previous page |
| Home / End | First / last page |
| Scroll wheel | Next / previous page |

## Audio and video

The player controls are at the bottom of the preview: play/pause, the time,
the **Autoplay** switch and the mute button. For audio, click or drag in the
waveform to jump to a position; for video, use the slider.

| Key | Action |
| --- | --- |
| Space or K | Play / pause |
| ← / → | Back / forward 5 seconds |
| M | Mute / unmute |
| A | Turn autoplay on or off |

### Autoplay

With autoplay on (`[x] Autoplay`), a file starts playing as soon as you
select it. With autoplay off (`[ ] Autoplay`), press **Space** to start it.
You can also change this in **Settings → Preview**.

### Playing the next file

When a file finishes, the next audio or video file in the panel is selected
and starts playing. Other files in between are skipped. Playback stops after
the last playable file in the folder. If you move the cursor while a file is
playing, the cursor stays where you put it when the file ends.

To turn this off, clear **Play the next file when one ends** in
**Settings → Preview**. Videos then repeat until you select another file.

### Mute

The player starts muted. When you unmute it, it stays unmuted for the
following files and the next time you start the app.

## Files on servers and in archives

Previews of files on servers and inside archives load only as much of the
file as needed. Text and hex previews read the beginning of the file, and
audio, video and PDF files are streamed. Images and archives are read
completely, up to 512 MB. Audio files on servers show a waveform only if
they are 100 MB or smaller.

## Cloud files

Files in folders synced by a cloud storage app can be stored
online only (marked ☁ in the panel). Previewing such a file downloads it.

- Files up to 25 MB are downloaded automatically. While this happens, the
  preview shows "Downloading *name* from the cloud" and the time elapsed.
- For larger files, the preview shows **Download and preview**. Click it to
  download the file and preview it.

If the cloud application doesn't deliver the file in time, the preview says
the file hasn't arrived and shows **Try again**. The download continues in
the background. Check that the cloud application is running, signed in and
online, then click **Try again**.
