//! Performance measurements on big data, compared with 1.x by hand (not run
//! by default): `OC_PERF_DATA=DIR cargo test --release -p oc-ui --test perf
//! -- --ignored --nocapture --test-threads 1 [case]`. DIR holds the data of
//! the perf generator (flat100k/, small20k/, tree50k/, rename5k/, big1g.bin,
//! big500m.zip, nested.zip, big_source.rs, big_image.png, doc300.pdf).
//!
//! The loop below is the platform loop without a window: tasks, timers and a
//! draw into a grid frame when something changed. Every call on the "UI
//! thread" is timed; `ui_max` is the longest one (what a user would feel as
//! a stall). Each line prints the median over the runs.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oc_grid::Frame;
use oc_ui::features::editor::{EditorState, EditorWindow, open_editor};
use oc_ui::features::preview::PreviewView;
use oc_ui::features::preview::image::ImageView;
use oc_ui::features::preview::pdf::PdfView;
use oc_ui::features::rename;
use oc_ui::features::search::{self, SearchState};
use oc_ui::main_window::{self, view_as};
use oc_ui::panel::{self, LoadOptions};
use oc_ui::testing::headless_app;
use oc_ui::{App, Event, Key, KeyEvent, MAIN_WINDOW, Mods};

const COLS: u16 = 160;
const ROWS: u16 = 45;

fn data() -> Option<PathBuf> {
    let d = std::env::var_os("OC_PERF_DATA").map(PathBuf::from);
    if d.is_none() {
        eprintln!("OC_PERF_DATA not set; skipped");
    }
    d
}

/// The UI thread: every call timed, the longest kept.
struct Ui {
    app: App,
    frame: Frame,
    max: Duration,
    _tmp: tempfile::TempDir,
}

impl Ui {
    fn new(left: &Path, right: &Path) -> Ui {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = headless_app(tmp.path(), left, right);
        app.cell_px = (16.0, 34.0);
        app.resize(MAIN_WINDOW, COLS, ROWS);
        let frame = Frame::new(COLS, ROWS, app.theme.colors.background);
        let mut ui = Ui { app, frame, max: Duration::ZERO, _tmp: tmp };
        ui.until("start", |app| app.tasks.pending() == 0 && app.main.sides.iter().all(|s| !s.panel.loading()));
        ui
    }

    fn timed<R>(&mut self, f: impl FnOnce(&mut App) -> R) -> R {
        let t = Instant::now();
        let r = f(&mut self.app);
        self.max = self.max.max(t.elapsed());
        r
    }

    fn step(&mut self) {
        self.timed(|app| app.process_tasks());
        self.timed(|app| app.tick(Instant::now()));
        if self.app.take_redraw() {
            self.draw();
        }
    }

    fn draw(&mut self) {
        let t = Instant::now();
        let win = self.app.focused;
        self.app.draw(win, &mut self.frame, self.app.cell_px);
        self.max = self.max.max(t.elapsed());
    }

    fn until(&mut self, what: &str, mut done: impl FnMut(&App) -> bool) {
        let end = Instant::now() + Duration::from_secs(900);
        loop {
            self.step();
            if done(&self.app) {
                return;
            }
            assert!(Instant::now() < end, "timed out: {what}");
            std::thread::sleep(Duration::from_micros(500));
        }
    }

    fn settle(&mut self) {
        self.until("listings", |app| app.tasks.pending() == 0 && app.main.sides.iter().all(|s| !s.panel.loading()));
    }

    fn key(&mut self, ev: KeyEvent) {
        let win = self.app.focused;
        self.timed(|app| app.handle_event(win, Event::Key(ev)));
    }

    fn load(&mut self, side: usize, dir: &Path) {
        let loc = oc_local::local(dir);
        self.timed(|app| panel::load(app, side, loc, LoadOptions::default()));
        self.settle();
    }

    fn cursor_on(&mut self, side: usize, name: &str) {
        let row = self.app.main.sides[side].panel.row_of(name).unwrap_or_else(|| panic!("{name} not listed"));
        self.timed(|app| {
            app.main.sides[side].panel.set_cursor(row);
            main_window::cursor_moved(app, side);
        });
    }

    /// Time `f` (started on the UI thread) until `done`: (wall ms, longest UI call ms).
    fn measure(&mut self, what: &str, start: impl FnOnce(&mut Ui), done: impl FnMut(&App) -> bool) -> (f64, f64) {
        self.max = Duration::ZERO;
        let t = Instant::now();
        start(self);
        self.until(what, done);
        let wall = t.elapsed().as_secs_f64() * 1e3;
        (wall, self.max.as_secs_f64() * 1e3)
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn report(case: &str, runs: Vec<(f64, f64)>) {
    let walls: Vec<f64> = runs.iter().map(|r| r.0).collect();
    let ui_max = runs.iter().map(|r| r.1).fold(0.0, f64::max);
    let all: Vec<String> = walls.iter().map(|w| format!("{w:.0}")).collect();
    println!("PERF\t{case}\t{:.1}\t{}\tui_max={ui_max:.1}ms", median(walls), all.join(" "));
}

fn empty_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
#[ignore]
fn list_sort_scroll_quick_search_100k() {
    let Some(d) = data() else { return };
    let empty = empty_dir();
    let mut ui = Ui::new(empty.path(), empty.path());
    let big = d.join("flat100k");
    let runs = (0..5)
        .map(|_| {
            ui.load(0, empty.path());
            let loc = oc_local::local(&big);
            ui.measure("100k listing", |ui| ui.timed(|app| panel::load(app, 0, loc, LoadOptions::default())), |app| {
                !app.main.sides[0].panel.loading() && app.main.sides[0].panel.entries.len() >= 100_000
            })
        })
        .collect();
    report("list100k", runs);

    // Re-sort: the Size column, then Name again (a header click each).
    let runs = (0..6)
        .map(|i| {
            let key = if i % 2 == 0 { oc_vfs::SortKey::Size } else { oc_vfs::SortKey::Name };
            ui.measure("re-sort", |ui| ui.timed(|app| panel::sort_by(app, 0, key)), |app| !app.main.sides[0].panel.loading())
        })
        .collect();
    report("resort100k", runs);

    // Scrolling: PageDown + a draw, per key.
    let runs = (0..5)
        .map(|_| {
            let t = Instant::now();
            for _ in 0..20 {
                ui.key(KeyEvent::plain(Key::PageDown));
                ui.step();
                ui.draw();
            }
            (t.elapsed().as_secs_f64() * 1e3 / 20.0, 0.0)
        })
        .collect();
    report("pagedown_redraw", runs);

    // Quick search: a prefix far down the list.
    let runs = (0..5)
        .map(|_| {
            ui.key(KeyEvent::plain(Key::Home));
            std::thread::sleep(panel::QUICK_SEARCH_TIMEOUT + Duration::from_millis(50));
            let t = Instant::now();
            for c in "omega_09".chars() {
                ui.key(KeyEvent::typed(&c.to_string()));
                ui.step();
            }
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let name = ui.app.main.sides[0].panel.cursor_name().unwrap_or_default().to_lowercase();
            assert!(name.starts_with("omega_09"), "{name}");
            (ms, 0.0)
        })
        .collect();
    report("quicksearch", runs);
}

fn copy(case: &str, d: &Path, name: &str, n: usize) {
    let work = tempfile::tempdir_in(std::env::var_os("OC_PERF_WORK").map_or_else(std::env::temp_dir, PathBuf::from)).unwrap();
    let runs = (0..n)
        .map(|i| {
            let dst = work.path().join(format!("dst{i}"));
            std::fs::create_dir_all(&dst).unwrap();
            let mut ui = Ui::new(d, &dst);
            ui.cursor_on(0, name);
            let r = ui.measure(
                case,
                |ui| {
                    ui.key(KeyEvent::plain(Key::F(5)));
                    ui.key(KeyEvent::plain(Key::Enter));
                },
                |app| app.operation.is_none() && app.modal_titles().is_empty(),
            );
            ui.app.shutdown();
            assert!(dst.join(name).exists(), "{name} copied");
            std::fs::remove_dir_all(&dst).unwrap();
            r
        })
        .collect();
    report(case, runs);
}

#[test]
#[ignore]
fn copy_1g() {
    let Some(d) = data() else { return };
    copy("copy1g", &d, "big1g.bin", 3);
}

#[test]
#[ignore]
fn copy_20k() {
    let Some(d) = data() else { return };
    copy("copy20k", &d, "small20k", 3);
}

#[test]
#[ignore]
fn open_archives() {
    let Some(d) = data() else { return };
    oc_archive::register();
    let empty = empty_dir();
    let mut ui = Ui::new(&d, empty.path());
    let runs = (0..5)
        .map(|_| {
            ui.load(0, &d);
            ui.cursor_on(0, "big500m.zip");
            ui.measure(
                "zip",
                |ui| ui.key(KeyEvent::plain(Key::Enter)),
                |app| {
                    let p = &app.main.sides[0].panel;
                    !p.loading() && !p.location.is_local()
                },
            )
        })
        .collect();
    report("open_zip500m", runs);

    let enter = |ui: &mut Ui, name: &str, then: &'static str| {
        ui.cursor_on(0, name);
        ui.key(KeyEvent::plain(Key::Enter));
        ui.until(then, move |app| {
            let p = &app.main.sides[0].panel;
            !p.loading() && p.row_of(then).is_some()
        });
    };
    let runs = (0..5)
        .map(|_| {
            ui.load(0, &d);
            ui.measure(
                "nested",
                |ui| {
                    enter(ui, "nested.zip", "inner.tar.gz");
                    enter(ui, "inner.tar.gz", "inner2.zip");
                    enter(ui, "inner2.zip", "deep");
                    ui.cursor_on(0, "deep");
                    ui.key(KeyEvent::plain(Key::Enter));
                },
                |app| {
                    let p = &app.main.sides[0].panel;
                    !p.loading() && p.entries.len() == 2000
                },
            )
        })
        .collect();
    report("open_nested", runs);
}

#[test]
#[ignore]
fn find_files() {
    let Some(d) = data() else { return };
    let root = d.join("tree50k");
    let empty = empty_dir();
    let mut ui = Ui::new(&root, empty.path());
    for (case, names, text) in [("find_name", "*omega*", ""), ("find_text", "*", "NEEDLE_XYZZY")] {
        let mut hits = 0;
        let runs = (0..5)
            .map(|_| {
                ui.load(0, &root);
                let mut q = oc_search::Query::new(vec![oc_local::local(&root)]);
                q.names = names.into();
                q.text = text.into();
                let r = ui.measure(case, |ui| { ui.timed(|app| search::start_search(app, q, 0)); }, |app| {
                    let Some(r) = app.try_state::<SearchState>().and_then(|s| s.last_results().cloned()) else { return false };
                    let p = &app.main.sides[0].panel;
                    !r.is_running()
                        && std::ptr::addr_eq(std::sync::Arc::as_ptr(&p.location.fs), std::sync::Arc::as_ptr(&r))
                        && !p.loading()
                        && p.entries.len() == r.hit_count()
                });
                hits = ui.app.main.sides[0].panel.entries.len();
                r
            })
            .collect();
        report(&format!("{case}(hits={hits})"), runs);
    }
}

#[test]
#[ignore]
fn highlight_1mb() {
    let Some(d) = data() else { return };
    let empty = empty_dir();
    let mut ui = Ui::new(&d, empty.path());
    let file = oc_local::local(d.join("big_source.rs"));
    let runs = (0..3)
        .map(|_| {
            let r = ui.measure(
                "highlighting",
                |ui| {
                    let id = ui.timed(|app| open_editor(app, file.clone()));
                    ui.app.resize(id, COLS, ROWS);
                },
                |app| {
                    let ids: Vec<_> = app.try_state::<EditorState>().map(|s| s.windows.iter().map(|(id, _)| *id).collect()).unwrap_or_default();
                    ids.iter().any(|id| app.window::<EditorWindow>(*id).is_some_and(|w| w.loaded && w.hl.done()))
                },
            );
            let ids: Vec<_> = ui.app.try_state::<EditorState>().map(|s| s.windows.iter().map(|(id, _)| *id).collect()).unwrap_or_default();
            for id in ids {
                ui.timed(|app| app.handle_event(id, Event::Key(KeyEvent::new(Key::Char('w'), Mods::primary()))));
            }
            ui.settle();
            ui.app.focused = MAIN_WINDOW;
            r
        })
        .collect();
    report("highlight1m_editor", runs);
}

#[test]
#[ignore]
fn previews() {
    let Some(d) = data() else { return };
    let empty = empty_dir();
    let mut ui = Ui::new(&d, empty.path());
    ui.key(KeyEvent::plain(Key::F(3)));
    for (case, name) in [("preview_image20m", "big_image.png"), ("preview_pdf300", "doc300.pdf")] {
        if name.ends_with(".pdf") && !oc_pdf::available() {
            eprintln!("PDFium not found; {case} skipped");
            continue;
        }
        let runs = (0..5)
            .map(|_| {
                ui.cursor_on(0, "big_source.rs");
                ui.until("text", |app| view_as::<PreviewView>(app, 1, "preview").is_some_and(|p| p.current_id() == Some("text") && !p.loading()));
                ui.measure(case, |ui| ui.cursor_on(0, name), |app| {
                    let Some(p) = view_as::<PreviewView>(app, 1, "preview") else { return false };
                    p.content_as::<ImageView>().is_some_and(|v| v.scaled.is_some())
                        || p.content_as::<PdfView>().is_some_and(|v| v.shown.as_ref().is_some_and(|s| s.width != 1200))
                })
            })
            .collect();
        report(case, runs);
    }
}

#[test]
#[ignore]
fn multi_rename_5k() {
    let Some(d) = data() else { return };
    let root = d.join("rename5k");
    let empty = empty_dir();
    let mut ui = Ui::new(&root, empty.path());
    ui.timed(|app| app.main.sides[0].panel.set_all_marks(true));
    let (open, open_ui) = ui.measure(
        "the dialog",
        |ui| ui.key(KeyEvent::new(Key::Char('m'), Mods::primary())),
        |app| rename::dialog(app).is_some(),
    );
    println!("PERF\trename5k_open\t{open:.1}\t\tui_max={open_ui:.1}ms");
    let masks = ["[N]_x", "[N]_[C]", "photo_[C:5]", "[N2-5]"];
    let runs = (0..5)
        .map(|_| {
            let per: Vec<(f64, f64)> = masks
                .iter()
                .map(|m| {
                    let m = m.to_string();
                    ui.max = Duration::ZERO;
                    let t = Instant::now();
                    ui.timed(|app| {
                        rename::with_open_dialog(app, |d, app| {
                            d.name_mask.set_text(&m);
                            d.update(app);
                        })
                    });
                    ui.step();
                    ui.draw();
                    (t.elapsed().as_secs_f64() * 1e3, ui.max.as_secs_f64() * 1e3)
                })
                .collect();
            (median(per.iter().map(|p| p.0).collect()), per.iter().map(|p| p.1).fold(0.0, f64::max))
        })
        .collect();
    report("rename5k_preview_per_edit", runs);
}
