//! Orchard Commander: `orchard-commander [LEFT] [RIGHT]`.
//!
//! Developer/CI flags:
//! - `--self-test`: check the renderer, fonts, archives, highlighting, PDFium and the
//!   config dir, print what was checked, exit 0 if all is well (CI runs it on
//!   installed packages).
//! - `--screenshot OUT.png [--size COLSxROWS] [LEFT] [RIGHT]`: build the main
//!   window for the folders, wait for the listings, render one frame
//!   offscreen and write it as PNG — the UI without a display.
//! - `OC_TIMING=1`: print startup-to-first-frame time to stderr (`OC_TIMING=exit`:
//!   and quit right after it).

mod geometry;
mod platform;
mod screenshot;
mod selftest;

#[cfg(target_os = "macos")]
mod macos;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "usage: orchard-commander [LEFT] [RIGHT]
       orchard-commander --self-test
       orchard-commander --screenshot OUT.png [--size COLSxROWS] [--scale F] [--keys KEYS] [LEFT] [RIGHT]
       orchard-commander --version";

pub struct Args {
    pub left: Option<PathBuf>,
    pub right: Option<PathBuf>,
    pub self_test: bool,
    pub screenshot: Option<PathBuf>,
    pub size: (u16, u16),
    pub scale: f32,
    /// `--keys "Down Down Space F5"`: keys pressed before the screenshot;
    /// `text:abc` types text, `wait:500` lets timers and output run for 500 ms.
    pub keys: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { left: None, right: None, self_test: false, screenshot: None, size: (140, 42), scale: 2.0, keys: Vec::new() };
    let mut it = std::env::args_os().skip(1);
    let mut folders = Vec::new();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--self-test") => args.self_test = true,
            Some("--screenshot") => args.screenshot = Some(it.next().ok_or("--screenshot needs a file name")?.into()),
            Some("--size") => {
                let v = it.next().ok_or("--size needs COLSxROWS")?;
                let v = v.to_string_lossy();
                let (c, r) = v.split_once(['x', 'X']).ok_or("--size needs COLSxROWS")?;
                let parse = |s: &str| s.parse::<u16>().ok().filter(|&n| n >= 20).ok_or(format!("bad --size {v}"));
                args.size = (parse(c)?, parse(r)?);
            }
            Some("--keys") => {
                let v = it.next().ok_or("--keys needs a list of keys")?;
                args.keys.extend(v.to_string_lossy().split_whitespace().map(str::to_owned));
            }
            Some("--scale") => {
                let v = it.next().ok_or("--scale needs a number")?;
                args.scale = v.to_string_lossy().parse().map_err(|_| "bad --scale".to_string())?;
            }
            Some("--version" | "-V") => {
                println!("orchard-commander {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Some("--help" | "-h") => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            Some(s) if s.starts_with("--") => return Err(format!("unknown option {s}\n{USAGE}")),
            // macOS passes -psn_… when launched from Finder on old systems.
            Some(s) if s.starts_with("-psn_") => {}
            _ => folders.push(PathBuf::from(a)),
        }
    }
    if folders.len() > 2 {
        return Err(USAGE.into());
    }
    let mut folders = folders.into_iter().map(|p| oc_local::absolute(&p));
    args.left = folders.next();
    args.right = folders.next();
    Ok(args)
}

fn main() -> ExitCode {
    let started = Instant::now();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    oc_archive::register();
    register_connection_types();
    let code = if args.self_test {
        selftest::run()
    } else if let Some(out) = &args.screenshot {
        screenshot::run(&args, out)
    } else {
        platform::run(args, started)
    };
    oc_archive::cleanup_cache_dir();
    code
}

/// The server connection types, in 1.x's order (the connection editor's
/// Type list): SFTP, SMB, WebDAV (HTTPS, HTTP), S3 (HTTPS, HTTP).
pub fn register_connection_types() {
    oc_sftp::register_builtin();
    oc_smb::register_builtin();
    oc_remote_http::register_builtin();
}
