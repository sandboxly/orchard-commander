//! `--screenshot OUT.png`: the main window rendered offscreen, for reviewers
//! and docs. Reads the user's settings (theme, saved folders) but writes nothing.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_grid::{Frame, HeadlessRenderer};
use oc_ui::{App, AppConfig, MAIN_WINDOW};

use crate::Args;

pub fn run(args: &Args, out: &Path) -> ExitCode {
    let t0 = Instant::now();
    let mut cfg = AppConfig::new(oc_config::ConfigPaths::platform(), Arc::new(|| {}));
    cfg.left = args.left.clone();
    cfg.right = args.right.clone();
    cfg.watch = false;
    cfg.persist = false;
    cfg.clipboard = Box::new(oc_ui::clipboard::MemoryClipboard::default());
    let mut app = App::new(cfg);
    let (cols, rows) = args.size;
    app.resize(MAIN_WINDOW, cols, rows);
    let created = t0.elapsed();

    // Fonts and the GPU device in parallel with the listings.
    let font = app.theme.font.clone();
    let scale = args.scale;
    let renderer = std::thread::spawn(move || HeadlessRenderer::new(font, scale));
    let listed = wait_for_listings(&mut app, Duration::from_secs(60));
    for key in &args.keys {
        let Some(s) = oc_ui::Shortcut::parse(key) else {
            eprintln!("unknown key {key:?} (use names like Down, F5, Primary+P, Space, a)");
            return ExitCode::from(2);
        };
        app.handle_event(MAIN_WINDOW, oc_ui::Event::Key(s.to_event()));
        wait_for_listings(&mut app, Duration::from_secs(60));
    }
    let mut renderer = match renderer.join().expect("renderer thread") {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot render offscreen: {e}");
            return ExitCode::FAILURE;
        }
    };
    let m = renderer.renderer().metrics();
    app.cell_px = (m.width as f32, m.height as f32);
    app.resize(MAIN_WINDOW, cols, rows);
    let mut frame = Frame::new(cols, rows, app.theme.colors.background);
    let t_draw = Instant::now();
    app.draw(MAIN_WINDOW, &mut frame, app.cell_px);
    let drawn = t_draw.elapsed();
    let t_render = Instant::now();
    let image = match renderer.render(&frame) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("render failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let rendered = t_render.elapsed();
    if let Err(e) = image.save_png(out) {
        eprintln!("cannot write {}: {e}", out.display());
        return ExitCode::FAILURE;
    }
    let counts: Vec<usize> = app.main.sides.iter().map(|s| s.panel.entries.len()).collect();
    eprintln!(
        "wrote {} ({}x{} px, {cols}x{rows} cells): app {:.1} ms, listings {:.1} ms ({} + {} entries), draw {:.2} ms, render {:.1} ms",
        out.display(),
        image.width,
        image.height,
        created.as_secs_f64() * 1e3,
        listed.as_secs_f64() * 1e3,
        counts[0],
        counts[1],
        drawn.as_secs_f64() * 1e3,
        rendered.as_secs_f64() * 1e3,
    );
    app.shutdown();
    ExitCode::SUCCESS
}

/// Process task results until both panels are listed; returns how long it took.
fn wait_for_listings(app: &mut App, timeout: Duration) -> Duration {
    let t0 = Instant::now();
    while t0.elapsed() < timeout {
        app.process_tasks();
        app.tick(Instant::now());
        if app.tasks.pending() == 0 && app.main.sides.iter().all(|s| !s.panel.loading()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    t0.elapsed()
}
