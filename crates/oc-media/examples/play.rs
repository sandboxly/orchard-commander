//! Play a media file on the default output device and print what happens:
//! `cargo run -p oc-media --example play -- <file> [--muted] [--null]`.

use std::time::{Duration, Instant};

use oc_media::{AudioOutput, Event, MediaSource, Player, PlayerOptions, WaveformAnalyzer, WaveformOptions, format_time};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: play <file> [--muted] [--null]");
        std::process::exit(2);
    };
    let flags: Vec<String> = args.collect();
    let output = if flags.iter().any(|f| f == "--null") { AudioOutput::Null } else { AudioOutput::Default };
    let muted = flags.iter().any(|f| f == "--muted");

    let start = Instant::now();
    let waveform = WaveformAnalyzer::start(MediaSource::Path(path.clone().into()), WaveformOptions::default());
    let player = Player::open(MediaSource::Path(path.into()), PlayerOptions { output, muted, autoplay: true, ..Default::default() });
    let mut frames = 0u64;
    let mut last_serial = 0;
    loop {
        if let Some(f) = player.video_frame()
            && f.serial != last_serial
        {
            last_serial = f.serial;
            frames += 1;
        }
        match player.wait_event(Duration::from_millis(10)) {
            Some(Event::Loaded(info)) => println!("{:>6} ms  loaded: {info:?}", start.elapsed().as_millis()),
            Some(Event::Position(p)) => {
                let levels = player.spectrum().levels(16).unwrap_or_default();
                let bars: String = levels.iter().map(|l| [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][(l * 8.0) as usize]).collect();
                print!("\r{} / {}  {frames} frames  |{bars}|", format_time(p.as_millis() as i64), format_time(player.duration().map_or(0, |d| d.as_millis() as i64)));
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            Some(Event::Finished) => {
                println!("\n{:>6} ms  finished", start.elapsed().as_millis());
                break;
            }
            Some(Event::Error(e)) => {
                println!("{}", oc_media::error_message(&e));
                break;
            }
            Some(e) => println!("{:>6} ms  {e:?}", start.elapsed().as_millis()),
            None => {}
        }
    }
    let w = waveform.snapshot();
    println!("waveform: {} blocks ({} ms), complete: {}, error: {:?}", w.peaks.len(), w.decoded_ms(), w.complete, w.error);
    let watch = player.shutdown_watch();
    let t = Instant::now();
    drop(player);
    println!("drop: {:?}; threads done after {:?}", t.elapsed(), {
        watch.wait(Duration::from_secs(2));
        t.elapsed()
    });
}
