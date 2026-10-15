//! The audio/video player (port of `preview/builtin/media.py`'s `MediaView`
//! with its waveform and spectrum widgets), on top of oc-media's [`Player`].
//!
//! * Audio: title and "artist · album", a live spectrum while playing, and
//!   the whole file's waveform as the seek bar (click or drag; the played
//!   part is lit; hovering shows the time there).
//! * Video: the frames through the image store (one key, `set` per new
//!   frame) and a seek slider.
//! * Below: ▶/❚❚, the time, `[x] Autoplay` and the mute switch.
//! * Keys (when the preview has the focus): Space/K play/pause, ←/→ ±5 s,
//!   M mute, A autoplay. Mute and autoplay are remembered in
//!   `Settings.extra` as `preview.media.muted` / `preview.media.autoplay`.
//! * At the end of a file the pane plays the next playable file in the
//!   panel (Settings → "Play the next file when one ends", `play_next`).
//!
//! The player's threads wake the UI through [`Tasks::post`](crate::tasks::Tasks::post)
//! (coalesced: one wake-up in flight at a time); the view then reads the
//! player's events and the latest video frame. Timers run only while
//! something moves: the spectrum at 60 fps while it animates, the video
//! frame timer at the video's frame rate while playing. Dropping the player
//! or the waveform analyzer never waits for their threads.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use orchard_grid::{Flags, Frame, ImagePlacement, Style};
use oc_media::prefs::{AUTOPLAY, MUTED, extra_key};
use oc_media::waveform::REMOTE_NOTE;
use oc_media::{
    AudioOutput, Event as MediaEvent, MediaInfo, MediaPrefs, Player, PlayerOptions, SEEK_STEP_MS, SpectrumAnimator, WaveformAnalyzer,
    WaveformLevels, WaveformOptions, error_message, format_time, should_loop,
};
use oc_preview::MediaData;
use oc_vfs::VPath;
use serde_json::Value;

use super::image::{area_px, fit};
use super::media_paint::{WaveState, hover_ms, paint_slider, paint_spectrum, paint_waveform, spectrum_metrics};
use super::message::MessageView;
use super::views::{ContentView, Handle};
use super::{ID, PreviewView};
use crate::app::{App, Outcome};
use crate::geom::Rect;
use crate::images::{self, ImageKey};
use crate::input::{Event, Key, Mods, MouseButton, MouseKind};
use crate::main_window::with_view_as;
use crate::widgets::{DrawCtx, fill, put, width};

/// How the player's audio is played (an app state; tests and `--screenshot`
/// use [`AudioOutput::Null`]).
#[derive(Default)]
pub struct MediaOptions {
    pub output: AudioOutput,
}

/// Play silently from now on (tests, screenshots).
pub fn silence(app: &mut App) {
    app.state::<MediaOptions>().output = AudioOutput::Null;
}

/// The spectrum's animation step.
const ANIMATION_STEP: Duration = Duration::from_micros(1_000_000 / oc_media::spectrum::FPS as u64);
/// Rows of the waveform (1.x: 4.5 lines).
const WAVEFORM_ROWS: u16 = 5;
const AUTOPLAY_W: u16 = 14;
const BUTTON_W: u16 = 4;

/// Where the parts go inside the content area.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub title: Rect,
    pub subtitle: Rect,
    pub spectrum: Rect,
    pub waveform: Rect,
    /// The video picture.
    pub screen: Rect,
    pub play: Rect,
    pub slider: Rect,
    pub time: Rect,
    pub autoplay: Rect,
    pub mute: Rect,
}

/// The layout for `area`; `time_w`: the time readout's width.
pub fn layout(area: Rect, video: bool, time_w: u16) -> Layout {
    let mut l = Layout::default();
    if area.is_empty() {
        return l;
    }
    let (body, controls) = area.split_bottom(1);
    // A spacer row above the controls when there is room.
    let body = if body.h >= 4 { body.split_bottom(1).0 } else { body };
    let row = controls;
    l.play = Rect::new(row.x, row.y, BUTTON_W.min(row.w), 1);
    l.mute = Rect::new(row.right().saturating_sub(BUTTON_W), row.y, BUTTON_W.min(row.w), 1);
    l.autoplay = Rect::new(l.mute.x.saturating_sub(AUTOPLAY_W + 1).max(l.play.right()), row.y, AUTOPLAY_W, 1);
    l.autoplay.w = l.autoplay.w.min(l.mute.x.saturating_sub(l.autoplay.x));
    if video {
        let tx = l.autoplay.x.saturating_sub(time_w + 2).max(l.play.right() + 1);
        l.time = Rect::new(tx, row.y, time_w.min(l.autoplay.x.saturating_sub(tx)), 1);
        let sx = l.play.right() + 1;
        l.slider = Rect::new(sx, row.y, tx.saturating_sub(sx + 1), 1);
        l.screen = body;
    } else {
        let tx = l.play.right() + 1;
        l.time = Rect::new(tx, row.y, time_w.min(l.autoplay.x.saturating_sub(tx)), 1);
        // A blank row below the chips when there is room (1.x's margin).
        let body = if body.h >= 16 { body.split_top(1).1 } else { body };
        l.title = body.row(0);
        l.subtitle = body.row(1);
        let wave_h = WAVEFORM_ROWS.min(body.h.saturating_sub(3));
        l.waveform = Rect::new(body.x, body.bottom() - wave_h, body.w, wave_h);
        let top = body.y + 3;
        let bottom = l.waveform.y.saturating_sub(1);
        if bottom > top {
            l.spectrum = Rect::new(body.x + 1, top, body.w.saturating_sub(2), bottom - top);
        }
    }
    l
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drag {
    Waveform,
    Slider,
}

/// The video frame in the image store.
struct Shown {
    key: ImageKey,
    width: u32,
    height: u32,
    serial: u64,
}

pub struct MediaView {
    pub path: VPath,
    pub name: String,
    /// Shows the video screen (from the extension until the file is open).
    pub video: bool,
    /// Tags and streams (probed while loading, then the player's).
    pub info: Option<MediaInfo>,
    /// Until shown: what to play.
    data: Option<MediaData>,
    player: Option<Player>,
    waveform: Option<WaveformAnalyzer>,
    note: &'static str,
    /// "Can't play this file: …" in place of the player.
    error: Option<MessageView>,
    pub animator: SpectrumAnimator,
    anim_timer: Option<u64>,
    last_tick: Instant,
    frame_timer: Option<u64>,
    frame: Option<Shown>,
    /// Video frames put on screen so far (tests).
    pub frames_shown: u64,
    hover: Option<u16>,
    drag: Option<Drag>,
    /// One wake-up from the player's threads at a time.
    wake_queued: Arc<AtomicBool>,
}

impl MediaView {
    pub fn new(mut data: MediaData, path: VPath) -> Self {
        let info = data.info.take().map(|b| *b);
        // Without a waveform source: a big file on a server (the loader
        // opens a second stream only for small audio files), or sound in a
        // video container there (no second stream either; nothing to say).
        let missing_waveform = if data.video { "" } else { REMOTE_NOTE };
        // The extension guesses; the probe knows (an .mp4 with only sound
        // gets the audio screen and its waveform).
        if let Some(i) = &info {
            data.video = i.has_video();
        }
        MediaView {
            path,
            name: data.name.clone(),
            video: data.video,
            info,
            data: Some(data),
            player: None,
            waveform: None,
            note: missing_waveform,
            error: None,
            animator: SpectrumAnimator::new(),
            anim_timer: None,
            last_tick: Instant::now(),
            frame_timer: None,
            frame: None,
            frames_shown: 0,
            hover: None,
            drag: None,
            wake_queued: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn player(&self) -> Option<&Player> {
        self.player.as_ref()
    }

    pub fn is_playing(&self) -> bool {
        self.player.as_ref().is_some_and(Player::is_playing)
    }

    pub fn is_muted(&self, app: &App) -> bool {
        self.player.as_ref().map_or_else(|| MediaPrefs::from_extra(&app.settings.extra).muted, Player::is_muted)
    }

    /// The size of the frame on screen.
    pub fn frame_size(&self) -> Option<(u32, u32)> {
        self.frame.as_ref().map(|f| (f.width, f.height))
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_ref().map(|m| m.message.as_str())
    }

    /// What the waveform has so far.
    pub fn waveform_levels(&self) -> Option<WaveformLevels> {
        self.waveform.as_ref().map(WaveformAnalyzer::snapshot)
    }

    pub fn title(&self) -> String {
        self.info.as_ref().map_or_else(|| self.name.clone(), |i| i.display_title(&self.name))
    }

    pub fn subtitle(&self) -> String {
        self.info.as_ref().map(MediaInfo::display_subtitle).unwrap_or_default()
    }

    pub fn duration_ms(&self) -> i64 {
        self.player.as_ref().and_then(Player::duration).map(|d| d.as_millis() as i64).or_else(|| self.info.as_ref()?.duration_ms()).unwrap_or(0)
    }

    pub fn position_ms(&self) -> i64 {
        self.player.as_ref().map_or(0, |p| p.position().as_millis() as i64)
    }

    pub fn time_text(&self) -> String {
        format!("{} / {}", format_time(self.position_ms()), format_time(self.duration_ms()))
    }

    pub fn play_label(&self) -> &'static str {
        if self.is_playing() { "❚❚" } else { "▶" }
    }

    pub fn autoplay_label(app: &App) -> String {
        let on = MediaPrefs::from_extra(&app.settings.extra).autoplay;
        format!("[{}] Autoplay", if on { 'x' } else { ' ' })
    }

    pub fn mute_label(&self, app: &App) -> &'static str {
        if self.is_muted(app) { "🔇" } else { "🔊" }
    }

    fn layout(&self, area: Rect) -> Layout {
        layout(area, self.video, width(&self.time_text()))
    }

    /// The video picture's size in physical pixels (what frames are scaled to).
    fn screen_px(&self, app: &App, area: Rect) -> (u32, u32) {
        let px = area_px(self.layout(area).screen, app.cell_px);
        ((px.w as u32).max(16), (px.h as u32).max(16))
    }

    // -- the player's side ----------------------------------------------------

    /// Wakes the UI from the player's threads to look at the player.
    fn waker(&self, app: &App, h: Handle) -> oc_media::Waker {
        let tasks = app.tasks.clone();
        let queued = self.wake_queued.clone();
        Arc::new(move || {
            if !queued.swap(true, Ordering::AcqRel) {
                let queued = queued.clone();
                tasks.post(move |app| {
                    queued.store(false, Ordering::Release);
                    h.with::<MediaView, _>(app, |v, app, area| v.pump(app, h, area));
                });
            }
        })
    }

    /// Read the player's events and the latest frame.
    fn pump(&mut self, app: &mut App, h: Handle, area: Rect) {
        let mut finished = false;
        if let Some(p) = &self.player {
            while let Some(ev) = p.poll_event() {
                match ev {
                    MediaEvent::Loaded(info) => {
                        if info.has_video() && !self.video {
                            self.video = true; // e.g. an .m4a with video: frames for the screen
                            p.set_video_size(Some(self.screen_px(app, area)));
                        }
                        self.video = info.has_video();
                        if !self.video && self.waveform.is_none() {
                            // Sound only after all (the probe failed while
                            // loading): no waveform source was opened.
                            self.waveform = Some(WaveformAnalyzer::none());
                        }
                        self.info = Some(*info);
                    }
                    MediaEvent::Finished => finished = true,
                    MediaEvent::Error(msg) => self.error = Some(MessageView::new(error_message(&msg), None)),
                    MediaEvent::StateChanged(_) | MediaEvent::Position(_) => {}
                }
            }
        }
        self.poll_frame(app);
        self.sync(app, h);
        app.redraw();
        // 1.x: only the shown view, only with "play the next file" on.
        if finished && self.error.is_none() && app.settings.play_next_enabled() {
            let (side, path) = (h.side, self.path.clone());
            app.later(move |app| super::finished_playing(app, side, &path));
        }
    }

    /// Put a new video frame into the image store.
    fn poll_frame(&mut self, app: &mut App) {
        let Some(frame) = self.player.as_ref().and_then(Player::video_frame) else { return };
        if self.frame.as_ref().is_some_and(|f| f.serial == frame.serial) {
            return;
        }
        let (w, h, serial) = (frame.width, frame.height, frame.serial);
        let store = images::store(app);
        let key = match &self.frame {
            Some(f) => {
                store.set(f.key, w, h, frame);
                f.key
            }
            None => store.insert(w, h, frame),
        };
        self.frame = Some(Shown { key, width: w, height: h, serial });
        self.frames_shown += 1;
        app.redraw();
    }

    /// Start or stop the timers to match the player.
    fn sync(&mut self, app: &mut App, h: Handle) {
        let playing = self.is_playing() && self.error.is_none();
        if self.error.is_some() {
            self.stop_timers(app);
            return;
        }
        if self.video {
            if playing && self.frame_timer.is_none() {
                self.schedule_frame(app, h);
            }
        } else {
            if let Some(t) = self.frame_timer.take() {
                app.cancel_timer(t);
            }
            self.animator.active = playing;
            let moving = playing || self.animator.peaks.iter().any(|&p| p > 0.001);
            if moving && self.anim_timer.is_none() {
                self.last_tick = Instant::now();
                self.schedule_animation(app, h);
            }
        }
    }

    fn schedule_frame(&mut self, app: &mut App, h: Handle) {
        // No frame queued (decoding behind, or the picture ended before the
        // sound): look again a frame later rather than spinning.
        let frame_interval = self.info.as_ref().and_then(|i| i.video.as_ref()?.fps).filter(|f| *f > 1.0).map_or(Duration::from_millis(33), |f| {
            Duration::from_secs_f64(1.0 / f)
        });
        let due = self.player.as_ref().and_then(Player::next_frame_in).unwrap_or(frame_interval);
        let due = due.clamp(Duration::from_millis(4), Duration::from_millis(50));
        self.frame_timer = Some(app.schedule(due, move |app| {
            h.with::<MediaView, _>(app, |v, app, _| {
                v.frame_timer = None;
                v.poll_frame(app);
                if v.video && v.is_playing() && v.error.is_none() {
                    v.schedule_frame(app, h);
                }
            });
        }));
    }

    fn schedule_animation(&mut self, app: &mut App, h: Handle) {
        self.anim_timer = Some(app.schedule(ANIMATION_STEP, move |app| {
            h.with::<MediaView, _>(app, |v, app, area| v.animate(app, h, area));
        }));
    }

    /// One step of the spectrum's bars.
    fn animate(&mut self, app: &mut App, h: Handle, area: Rect) {
        self.anim_timer = None;
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        let spectrum = self.layout(area).spectrum;
        let count = spectrum_metrics(area_px(spectrum, app.cell_px).w, app.cell_px.0).2;
        let playing = self.is_playing();
        self.animator.active = playing;
        let levels = if playing { self.player.as_ref().and_then(|p| p.spectrum().levels(count)) } else { None };
        let more = self.animator.tick(dt, levels.as_deref(), count);
        app.redraw();
        if more {
            self.schedule_animation(app, h);
        }
    }

    fn stop_timers(&mut self, app: &mut App) {
        for t in [self.anim_timer.take(), self.frame_timer.take()].into_iter().flatten() {
            app.cancel_timer(t);
        }
    }

    // -- controls ---------------------------------------------------------------

    /// Space / K / the ▶ button.
    pub fn toggle_play(&mut self, app: &mut App, h: Handle) {
        if let Some(p) = &self.player {
            p.toggle_play();
        }
        self.sync(app, h);
        app.redraw();
    }

    /// ← / →.
    pub fn seek_by(&mut self, app: &mut App, delta_ms: i64) {
        if let Some(p) = &self.player {
            p.seek_by(delta_ms);
        }
        app.redraw();
    }

    pub fn seek_to(&mut self, app: &mut App, ms: i64) {
        if let Some(p) = &self.player {
            p.seek(Duration::from_millis(ms.max(0) as u64));
        }
        app.redraw();
    }

    /// M / the speaker button: for this file, the next ones and the next run.
    pub fn toggle_mute(&mut self, app: &mut App) {
        let muted = match &self.player {
            Some(p) => p.toggle_mute(),
            None => !MediaPrefs::from_extra(&app.settings.extra).muted,
        };
        store_pref(app, MUTED, muted);
    }

    /// A / the "[x] Autoplay" button: for the next file and the next run.
    pub fn toggle_autoplay(&mut self, app: &mut App) {
        let on = !MediaPrefs::from_extra(&app.settings.extra).autoplay;
        store_pref(app, AUTOPLAY, on);
    }

    /// Settings changed elsewhere (Settings → Preview, another player).
    pub fn apply_settings(&mut self, app: &App) {
        let prefs = MediaPrefs::from_extra(&app.settings.extra);
        if let Some(p) = &self.player {
            p.set_muted(prefs.muted);
            p.set_looping(should_loop(self.video, app.settings.play_next_enabled()));
        }
    }

    /// A seek bar's time at column `col` (waveform or slider).
    fn seek_at(&mut self, app: &mut App, l: &Layout, drag: Drag, col: u16) {
        let (bar, total) = match drag {
            Drag::Waveform => {
                let total = self.waveform.as_ref().map_or(0, |w| w.with_levels(|lv| lv.total_ms(self.duration_ms())));
                (l.waveform, total)
            }
            Drag::Slider => (l.slider, self.duration_ms()),
        };
        if total > 0 && !bar.is_empty() {
            let col = col.clamp(bar.x, bar.right() - 1);
            self.seek_to(app, hover_ms(bar, col, total));
        }
    }

    fn mouse(&mut self, app: &mut App, h: Handle, m: &crate::input::MouseEvent, area: Rect) -> Outcome {
        let l = self.layout(area);
        match m.kind {
            MouseKind::Down(MouseButton::Left) => {
                if l.play.contains(m.col, m.row) {
                    self.toggle_play(app, h);
                } else if l.mute.contains(m.col, m.row) {
                    self.toggle_mute(app);
                } else if l.autoplay.contains(m.col, m.row) {
                    self.toggle_autoplay(app);
                } else if l.waveform.contains(m.col, m.row) {
                    self.drag = Some(Drag::Waveform);
                    self.seek_at(app, &l, Drag::Waveform, m.col);
                } else if l.slider.contains(m.col, m.row) {
                    self.drag = Some(Drag::Slider);
                    self.seek_at(app, &l, Drag::Slider, m.col);
                } else {
                    return Outcome::Ignored;
                }
                Outcome::Consumed
            }
            MouseKind::Move => {
                let hover = l.waveform.contains(m.col, m.row).then_some(m.col);
                if hover != self.hover {
                    self.hover = hover;
                    app.redraw();
                }
                if let Some(d) = self.drag {
                    // The button went up outside the pane: no Up arrives then.
                    let bar = if d == Drag::Waveform { l.waveform } else { l.slider };
                    if m.row + 1 < bar.y || m.row > bar.bottom() {
                        self.drag = None;
                    } else {
                        self.seek_at(app, &l, d, m.col);
                    }
                }
                Outcome::Consumed
            }
            MouseKind::Up(MouseButton::Left) => {
                self.drag = None;
                Outcome::Consumed
            }
            _ => Outcome::Ignored,
        }
    }
}

/// Remember a player switch in `Settings.extra` (only when it changes).
fn store_pref(app: &mut App, key: &'static str, value: bool) {
    if MediaPrefs::set(&app.settings.extra, key, value).is_some() {
        app.update_settings(move |s| {
            s.extra.insert(extra_key(key), Value::Bool(value));
        });
    }
    app.redraw();
}

/// Settings changed (hook): the shown players follow mute, autoplay and
/// "play the next file" (1.x's `set_continuous` / `store_changed`).
pub fn settings_changed(app: &mut App) {
    for side in 0..2 {
        if app.main.sides[side].mode_id() == ID {
            with_view_as::<PreviewView, _>(app, side, ID, |pane, app| {
                if let Some(v) = pane.content_mut_as::<MediaView>() {
                    v.apply_settings(app);
                }
            });
        }
    }
}

impl ContentView for MediaView {
    fn kind(&self) -> &'static str {
        "media"
    }

    fn draw(&self, app: &App, f: &mut Frame, area: Rect, ctx: &DrawCtx, _focused: bool) {
        if let Some(e) = &self.error {
            e.draw(app, f, area, ctx, false);
            return;
        }
        let c = &ctx.theme.colors;
        let time = self.time_text();
        let l = layout(area, self.video, width(&time));
        let centered = |f: &mut Frame, r: Rect, text: &str, style: Style| {
            if !r.is_empty() {
                let w = width(text).min(r.w);
                put(f, r.x + (r.w - w) / 2, r.y, text, style, r.w);
            }
        };
        if self.video {
            if let Some(shown) = &self.frame {
                let dest = fit(shown.width, shown.height, area_px(l.screen, ctx.cell_px), true);
                f.add_image(ImagePlacement::new(shown.key.id(), dest));
            }
            paint_slider(f, l.slider, ctx, self.position_ms(), self.duration_ms());
        } else {
            centered(f, l.title, &self.title(), Style::new(c.accent, c.surface).with(Flags::BOLD));
            centered(f, l.subtitle, &self.subtitle(), Style::new(c.muted, c.surface));
            paint_spectrum(f, l.spectrum, ctx, &self.animator.bars, &self.animator.peaks);
            let empty = WaveformLevels::default();
            let draw_wave = |f: &mut Frame, levels: &WaveformLevels| {
                let state = WaveState {
                    levels,
                    note: self.note,
                    duration_ms: self.duration_ms(),
                    position_ms: self.position_ms(),
                    hover: self.hover,
                };
                paint_waveform(f, l.waveform, ctx, &state);
            };
            match &self.waveform {
                Some(w) => w.with_levels(|levels| draw_wave(f, levels)),
                None => draw_wave(f, &empty),
            }
        }
        // The controls.
        let button = Style::new(c.foreground, c.border);
        fill(f, l.play, button.bg);
        centered(f, l.play, self.play_label(), button.with(Flags::BOLD));
        put(f, l.time.x, l.time.y, &time, Style::new(c.foreground, c.surface), l.time.w);
        let autoplay = MediaPrefs::from_extra(&app.settings.extra).autoplay;
        let style = if autoplay { Style::new(c.accent, c.border) } else { Style::new(c.muted, c.border) };
        fill(f, l.autoplay, style.bg);
        centered(f, l.autoplay, &Self::autoplay_label(app), style);
        fill(f, l.mute, button.bg);
        centered(f, l.mute, self.mute_label(app), button);
    }

    fn handle_event(&mut self, app: &mut App, h: Handle, ev: &Event, area: Rect) -> Outcome {
        if self.error.is_some() {
            return Outcome::Ignored;
        }
        match ev {
            Event::Key(k) if k.mods == Mods::NONE || k.mods == Mods::SHIFT => {
                match k.key {
                    Key::Space => self.toggle_play(app, h),
                    Key::Left => self.seek_by(app, -SEEK_STEP_MS),
                    Key::Right => self.seek_by(app, SEEK_STEP_MS),
                    Key::Char(ch) => match ch.to_ascii_lowercase() {
                        'k' => self.toggle_play(app, h),
                        'm' => self.toggle_mute(app),
                        'a' => self.toggle_autoplay(app),
                        _ => return Outcome::Ignored,
                    },
                    _ => return Outcome::Ignored,
                }
                Outcome::Consumed
            }
            Event::Mouse(m) => self.mouse(app, h, m, area),
            _ => Outcome::Ignored,
        }
    }

    fn shown(&mut self, app: &mut App, h: Handle, area: Rect) {
        let Some(data) = self.data.take() else { return };
        let prefs = MediaPrefs::from_extra(&app.settings.extra);
        let output = app.try_state::<MediaOptions>().map_or_else(AudioOutput::default, |o| o.output);
        let waker = self.waker(app, h);
        let (source, waveform_source) = data.into_sources();
        // Until the file is open, a video's frames are scaled to the screen
        // as laid out from its extension.
        let video_size = self.video.then(|| self.screen_px(app, area));
        self.player = Some(Player::open(
            source,
            PlayerOptions {
                output,
                autoplay: prefs.autoplay,
                muted: prefs.muted,
                looping: should_loop(self.video, app.settings.play_next_enabled()),
                video_size,
                waker: Some(waker.clone()),
                ..PlayerOptions::default()
            },
        ));
        if !self.video {
            self.waveform = Some(match waveform_source {
                Some(src) => {
                    self.note = "";
                    WaveformAnalyzer::start(src, WaveformOptions { waker: Some(waker), ..WaveformOptions::default() })
                }
                None => WaveformAnalyzer::none(),
            });
        }
        self.sync(app, h);
    }

    fn resized(&mut self, app: &mut App, _h: Handle, area: Rect) {
        if self.video {
            let size = self.screen_px(app, area);
            if let Some(p) = &self.player {
                p.set_video_size(Some(size));
            }
        }
    }

    fn leaving(&mut self, app: &mut App) {
        // Another file is on its way: stop the sound now (1.x stopped the
        // player when a load started); the picture stays until it arrives.
        if let Some(p) = &self.player {
            p.pause();
        }
        self.animator.active = false;
        app.redraw();
    }

    fn play(&mut self, app: &mut App, h: Handle) {
        if let Some(p) = &self.player {
            p.play();
        }
        self.sync(app, h);
    }

    fn clear(&mut self, app: &mut App) {
        self.stop_timers(app);
        if let Some(f) = self.frame.take() {
            images::store(app).remove(f.key);
        }
        // Neither waits for its threads (streams close there).
        self.player = None;
        self.waveform = None;
        if let Some(data) = self.data.take() {
            // Never shown: an open stream may take a while to close.
            let _ = std::thread::Builder::new().name("oc-media drop".into()).spawn(move || drop(data));
        }
    }

    fn text(&self) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(e.message.clone());
        }
        Some(format!("{}\n{}\n{}", self.title(), self.subtitle(), self.time_text()))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_layout_stacks_title_spectrum_waveform_controls() {
        let l = layout(Rect::new(10, 5, 60, 30), false, 13);
        assert_eq!((l.title.y, l.subtitle.y), (6, 7));
        assert_eq!(l.play, Rect::new(10, 34, 4, 1));
        assert_eq!(l.time, Rect::new(15, 34, 13, 1));
        assert_eq!(l.mute, Rect::new(66, 34, 4, 1));
        assert_eq!(l.autoplay, Rect::new(51, 34, 14, 1));
        assert_eq!(l.waveform, Rect::new(10, 28, 60, 5));
        assert_eq!((l.spectrum.y, l.spectrum.bottom()), (9, 27));
        assert!(l.slider.is_empty() && l.screen.is_empty());
    }

    #[test]
    fn video_layout_has_a_screen_and_a_slider() {
        let l = layout(Rect::new(0, 0, 60, 20), true, 13);
        assert_eq!(l.screen, Rect::new(0, 0, 60, 18));
        assert_eq!(l.time.right() + 2, l.autoplay.x);
        assert_eq!(l.slider.x, 5);
        assert_eq!(l.slider.right() + 1, l.time.x);
        assert!(l.waveform.is_empty() && l.spectrum.is_empty());
    }

    #[test]
    fn tiny_areas_do_not_panic() {
        for (w, h) in [(0, 0), (1, 1), (3, 2), (10, 3), (30, 6)] {
            for video in [false, true] {
                let _ = layout(Rect::new(0, 0, w, h), video, 13);
            }
        }
    }
}
