//! The media player in the Preview pane, driven without a window (ports of
//! 1.x's `test_ui.py` media tests, `test_waveform.py` and `test_spectrum.py`):
//! audio with waveform and spectrum, video frames, keys, remembered mute and
//! autoplay, playing the next file. Audio goes to a silent output.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use oc_media::PlaybackState;
use oc_ui::features::preview::PreviewView;
use oc_ui::features::preview::media::{self, MediaView};
use oc_ui::images::Images;
use oc_ui::main_window::{self, SideMode, view_as, with_view_as};
use oc_ui::testing::{headless_app, press, settle, wait_until};
use oc_ui::{App, Event, Key, MAIN_WINDOW, Mods, MouseButton, MouseEvent, MouseKind};

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../oc-media/tests/data").join(name)
}

struct Env {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    src: PathBuf,
}

/// src/{1.mp3, 2.mp3, 3.mp3, notes.txt, clip.mp4, bad.mp3} (1.x's `playlist`
/// fixture plus a video and a broken file).
fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = oc_local::absolute(&fs::canonicalize(tmp.path()).unwrap());
    let (config, src) = (base.join("config"), base.join("src"));
    for d in [&config, &src, &base.join("dst")] {
        fs::create_dir_all(d).unwrap();
    }
    let tone = fs::read(data("tone-then-silence.mp3")).unwrap();
    for name in ["1.mp3", "2.mp3", "3.mp3"] {
        fs::write(src.join(name), &tone).unwrap();
    }
    fs::write(src.join("notes.txt"), "not audio").unwrap();
    fs::copy(data("clip-160x90.mp4"), src.join("clip.mp4")).unwrap();
    fs::write(src.join("bad.mp3"), vec![0x5au8; 4096]).unwrap();
    Env { _tmp: tmp, config, src }
}

fn app(e: &Env) -> App {
    let mut app = headless_app(&e.config, &e.src, &e.src.parent().unwrap().join("dst"));
    media::silence(&mut app);
    app.resize(MAIN_WINDOW, 120, 40);
    settle(&mut app);
    press(&mut app, Key::F(3));
    assert_eq!(app.main.sides[1].mode_id(), "preview");
    app
}

fn pane(app: &App) -> &PreviewView {
    view_as::<PreviewView>(app, 1, "preview").expect("the preview view")
}

fn player_view(app: &App) -> Option<&MediaView> {
    pane(app).content_as::<MediaView>()
}

fn view(app: &App) -> &MediaView {
    player_view(app).expect("the player")
}

/// Run `f` with the shown player view.
fn with_player<R>(app: &mut App, f: impl FnOnce(&mut MediaView, &mut App) -> R) -> R {
    with_view_as::<PreviewView, _>(app, 1, "preview", |p, app| f(p.content_mut_as::<MediaView>().expect("the player"), app)).unwrap()
}

fn select(app: &mut App, name: &str) {
    let p = &mut app.main.sides[0].panel;
    let row = p.row_of(name).unwrap_or_else(|| panic!("{name} not listed"));
    p.set_cursor(row);
    main_window::cursor_moved(app, 0);
}

fn wait_for(app: &mut App, what: &str, done: impl FnMut(&App) -> bool) {
    wait_until(app, what, Duration::from_secs(20), done);
}

/// The preview shows `name` in the player, opened.
fn show(app: &mut App, name: &str) {
    select(app, name);
    let n = name.to_owned();
    wait_for(app, name, move |app| {
        player_view(app).is_some_and(|v| v.name == n && v.player().is_some_and(|p| p.info().is_some()))
    });
}

fn cursor(app: &App) -> &str {
    app.main.sides[0].panel.cursor_name().unwrap_or("")
}

fn state(app: &App) -> PlaybackState {
    view(app).player().map_or(PlaybackState::Stopped, |p| p.state())
}

fn saved_extra(app: &App, key: &str) -> Option<serde_json::Value> {
    app.flush_settings();
    oc_config::Settings::load(&app.paths.settings_file()).extra.get(key).cloned()
}

fn click(app: &mut App, col: u16, row: u16) {
    for kind in [MouseKind::Down(MouseButton::Left), MouseKind::Up(MouseButton::Left)] {
        app.handle_event(MAIN_WINDOW, Event::Mouse(MouseEvent { kind, col, row, mods: Mods::NONE }));
    }
}

/// Where the player's parts are on screen.
fn parts(app: &App, video: bool) -> media::Layout {
    let frame = main_window::layout(app).frames[1];
    let (_, area) = (frame, frame.inset(1, 1));
    // The chips row ("Player", "Hex") sits above the content.
    let area = area.split_top(1).1;
    media::layout(area, video, oc_ui::widgets::width(&view(app).time_text()))
}

#[test]
fn audio_shows_title_waveform_and_a_moving_spectrum() {
    let e = env();
    let mut app = app(&e);
    show(&mut app, "1.mp3");
    assert_eq!(pane(&app).current_id(), Some("media"));
    assert_eq!(pane(&app).chip_ids(), ["media", "hex"]);
    assert_eq!(view(&app).title(), "1.mp3", "no title tag: the file name");
    assert!(!view(&app).video);
    assert!(pane(&app).info.ends_with("audio"));
    // Autoplay is on until turned off; the waveform is the whole file's.
    assert_eq!(state(&app), PlaybackState::Playing);
    wait_for(&mut app, "the waveform", |app| view(app).waveform_levels().is_some_and(|l| l.complete && !l.peaks.is_empty()));
    assert_eq!(view(&app).duration_ms() / 100, 20);
    assert!(view(&app).time_text().ends_with("/ 00:02"));
    wait_for(&mut app, "the spectrum to rise", |app| view(app).animator.bars.iter().copied().fold(0.0, f32::max) > 0.3);
    assert!(view(&app).animator.peaks.iter().zip(&view(&app).animator.bars).all(|(p, b)| p >= b));
    // Paused: the bars fall and the animation stops by itself.
    press(&mut app, Key::Tab);
    press(&mut app, Key::Space);
    assert_eq!(state(&app), PlaybackState::Paused);
    wait_for(&mut app, "the spectrum to settle", |app| {
        let v = view(app);
        v.animator.bars.iter().all(|&b| b == 0.0) && v.animator.peaks.iter().all(|&p| p <= 0.001)
    });
    assert_eq!(app.next_deadline(), None, "no timers once nothing moves");
    // Drawing works at any size.
    for (w, h) in [(120, 40), (40, 10), (12, 5)] {
        app.resize(MAIN_WINDOW, w, h);
        let mut f = oc_grid::Frame::new(w, h, app.theme.colors.background);
        app.draw(MAIN_WINDOW, &mut f, app.cell_px);
    }
}

#[test]
fn clicking_the_waveform_seeks() {
    let e = env();
    let mut app = app(&e);
    show(&mut app, "2.mp3");
    wait_for(&mut app, "the waveform", |app| view(app).waveform_levels().is_some_and(|l| l.complete));
    with_player(&mut app, |v, app| {
        v.toggle_autoplay(app); // keep it from ending meanwhile
        v.player().unwrap().pause();
    });
    let l = parts(&app, false);
    click(&mut app, l.waveform.x + l.waveform.w / 2, l.waveform.y + 2);
    wait_for(&mut app, "the seek", |app| (view(app).position_ms() - 1000).abs() <= 100);
    assert_eq!(app.main.active, 1, "clicking the preview focuses it");
    // Hovering shows the time under the mouse above the waveform.
    let (col, row) = (l.waveform.x + l.waveform.w / 4, l.waveform.y + 2);
    app.handle_event(MAIN_WINDOW, Event::Mouse(MouseEvent { kind: MouseKind::Move, col, row, mods: Mods::NONE }));
    let mut f = oc_grid::Frame::new(120, 40, app.theme.colors.background);
    app.draw(MAIN_WINDOW, &mut f, app.cell_px);
    assert!(f.row_text(l.waveform.y).contains(" 00:00 "), "{:?}", f.row_text(l.waveform.y));
    // ←/→ move by 5 s, within the file.
    press(&mut app, Key::Left);
    wait_for(&mut app, "back to the start", |app| view(app).position_ms() == 0);
    press(&mut app, Key::Right);
    wait_for(&mut app, "the end", |app| view(app).position_ms() >= 1900);
}

#[test]
fn video_frames_reach_the_image_store() {
    let e = env();
    let mut app = app(&e);
    show(&mut app, "clip.mp4");
    assert!(view(&app).video);
    assert!(pane(&app).info.ends_with("video"));
    wait_for(&mut app, "frames", |app| view(app).frames_shown >= 5);
    assert_eq!(view(&app).frame_size(), Some((160, 90)), "never enlarged by the decoder");
    assert_eq!(app.main.active, 0, "the file list keeps the focus");
    assert!(app.try_state::<Images>().is_some_and(|s| s.len() == 1));
    let mut f = oc_grid::Frame::new(120, 40, app.theme.colors.background);
    app.draw(MAIN_WINDOW, &mut f, app.cell_px);
    assert_eq!(f.images.len(), 1, "the frame is drawn");
    // Leaving the preview stops playback and frees the frame.
    main_window::set_mode(&mut app, 1, SideMode::Files);
    assert!(app.try_state::<Images>().is_none_or(|s| s.is_empty()));
    assert_eq!(app.next_deadline(), None);
}

#[test]
fn sound_in_a_video_container_gets_the_audio_screen_and_its_waveform() {
    let e = env();
    fs::copy(data("tone-then-silence.mp3"), e.src.join("sound.mp4")).unwrap();
    let mut app = app(&e);
    app.update_settings(|s| s.play_next = Some(false));
    show(&mut app, "sound.mp4");
    assert!(!view(&app).video, "the probe found no picture");
    wait_for(&mut app, "the waveform", |app| view(app).waveform_levels().is_some_and(|l| l.complete && !l.peaks.is_empty()));
    assert_eq!(view(&app).frames_shown, 0);
    // Played to its end: no timer keeps polling for frames.
    wait_for(&mut app, "the end", |app| view(app).player().is_some_and(|p| p.state() != PlaybackState::Playing));
    wait_until(&mut app, "the bars to settle", Duration::from_secs(10), |app| app.next_deadline().is_none());
}

#[test]
fn videos_loop_only_without_play_next() {
    let e = env();
    let mut app = app(&e);
    show(&mut app, "clip.mp4");
    assert!(!view(&app).player().unwrap().is_looping(), "play_next is on by default");
    app.update_settings(|s| s.play_next = Some(false)); // Settings → Preview
    assert!(view(&app).player().unwrap().is_looping());
    app.update_settings(|s| s.play_next = None);
    assert!(!view(&app).player().unwrap().is_looping());
}

#[test]
fn mute_is_remembered() {
    let e = env();
    let mut app = app(&e);
    show(&mut app, "1.mp3");
    assert!(view(&app).player().unwrap().is_muted(), "muted until unmuted once");
    press(&mut app, Key::Tab);
    press(&mut app, Key::Char('m'));
    assert!(!view(&app).player().unwrap().is_muted());
    assert_eq!(saved_extra(&app, "preview.media.muted"), Some(serde_json::Value::Bool(false)));
    press(&mut app, Key::Tab);
    show(&mut app, "2.mp3");
    assert!(!view(&app).player().unwrap().is_muted(), "still unmuted for the next file");
    // The speaker button too.
    let l = parts(&app, false);
    click(&mut app, l.mute.x + 1, l.mute.y);
    assert!(view(&app).player().unwrap().is_muted());
    assert_eq!(saved_extra(&app, "preview.media.muted"), Some(serde_json::Value::Bool(true)));
}

#[test]
fn autoplay_toggle_is_remembered_and_moving_on_still_plays() {
    let e = env();
    let mut app = app(&e);
    show(&mut app, "1.mp3");
    assert_eq!(MediaView::autoplay_label(&app), "[x] Autoplay");
    assert_eq!(state(&app), PlaybackState::Playing);
    press(&mut app, Key::Tab);
    press(&mut app, Key::Char('a'));
    assert_eq!(MediaView::autoplay_label(&app), "[ ] Autoplay");
    assert_eq!(saved_extra(&app, "preview.media.autoplay"), Some(serde_json::Value::Bool(false)));
    press(&mut app, Key::Tab);
    show(&mut app, "2.mp3");
    assert_eq!(state(&app), PlaybackState::Stopped, "waits for Space now");
    // The button turns it on again (for the next file).
    let l = parts(&app, false);
    click(&mut app, l.autoplay.x + 2, l.autoplay.y);
    assert_eq!(MediaView::autoplay_label(&app), "[x] Autoplay");
    with_player(&mut app, |v, app| v.toggle_autoplay(app));
    // Played to the end: the next playable file starts even with autoplay off.
    with_player(&mut app, |v, app| {
        v.seek_to(app, 1700);
        v.player().unwrap().play();
    });
    wait_for(&mut app, "3.mp3 playing", |app| {
        cursor(app) == "3.mp3" && player_view(app).is_some_and(|v| v.name == "3.mp3" && v.is_playing())
    });
}

#[test]
fn play_next_skips_other_files_and_stops_at_the_last() {
    let e = env();
    let mut app = app(&e);
    // Sorted by name: 1.mp3 2.mp3 3.mp3 bad.mp3 clip.mp4 notes.txt.
    show(&mut app, "3.mp3");
    with_player(&mut app, |v, app| v.seek_to(app, 1800));
    // bad.mp3 is next by name (it is an .mp3): the player tries it.
    wait_for(&mut app, "bad.mp3", |app| cursor(app) == "bad.mp3");
    wait_for(&mut app, "the error", |app| player_view(app).and_then(MediaView::error).is_some());
    assert!(view(&app).error().unwrap().starts_with("Can't play this file:\n"));
    // A video is played by the same previewer: after the clip comes nothing (notes.txt).
    show(&mut app, "clip.mp4");
    with_player(&mut app, |v, app| v.seek_to(app, 1900));
    wait_for(&mut app, "the clip to end", |app| view(app).player().unwrap().state() == PlaybackState::Stopped);
    settle(&mut app);
    assert_eq!(cursor(&app), "clip.mp4", "the last playable file: stay");

    // Moved away meanwhile: the cursor stays where the user put it.
    show(&mut app, "1.mp3");
    with_player(&mut app, |v, app| v.seek_to(app, 1900));
    let p = &mut app.main.sides[0].panel;
    p.set_cursor(p.row_of("notes.txt").unwrap()); // without telling the preview
    wait_for(&mut app, "1.mp3 to end", |app| view(app).player().unwrap().state() == PlaybackState::Stopped);
    settle(&mut app);
    assert_eq!(cursor(&app), "notes.txt");

    // "Play the next file when one ends" off: audio stops at its end.
    app.update_settings(|s| s.play_next = Some(false));
    show(&mut app, "2.mp3");
    with_player(&mut app, |v, app| v.seek_to(app, 1900));
    wait_for(&mut app, "2.mp3 to end", |app| view(app).player().unwrap().state() == PlaybackState::Stopped);
    settle(&mut app);
    assert_eq!(cursor(&app), "2.mp3");
}

#[test]
fn moving_on_stops_the_sound_and_closing_drops_the_player() {
    let e = env();
    let mut app = app(&e);
    show(&mut app, "1.mp3");
    assert_eq!(state(&app), PlaybackState::Playing);
    select(&mut app, "notes.txt");
    wait_for(&mut app, "the load to start", |app| player_view(app).is_none_or(|v| !v.is_playing()));
    wait_for(&mut app, "notes.txt", |app| pane(app).current_id() == Some("text"));
    show(&mut app, "2.mp3");
    press(&mut app, Key::F(3));
    assert_eq!(app.main.sides[1].mode, SideMode::Files);
    assert_eq!(app.next_deadline(), None, "no player timers left");
}
