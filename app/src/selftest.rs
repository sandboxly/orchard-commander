//! `--self-test`: does this build work on this machine? CI runs it on the
//! installed package; it prints each check and exits non-zero on failure.

use std::io::Write;
use std::process::ExitCode;
use std::time::Instant;

use oc_grid::{FontConfig, FontLibrary, Frame, HeadlessRenderer, Rgba, Style};

pub fn run() -> ExitCode {
    let mut failed = 0;
    let mut check = |name: &str, result: Result<String, String>| {
        match &result {
            Ok(detail) => println!("ok    {name}: {detail}"),
            Err(e) => {
                println!("FAIL  {name}: {e}");
                failed += 1;
            }
        }
        let _ = std::io::stdout().flush();
    };
    println!("orchard-commander {} self-test", env!("CARGO_PKG_VERSION"));
    check("config dir", config_dir());
    check("fonts", fonts());
    check("renderer", renderer());
    check("archives", archives());
    check("syntax highlighting", highlighting());
    check("server connections", connection_types());
    check("certificates", certificates());
    if failed == 0 {
        println!("all checks passed");
        ExitCode::SUCCESS
    } else {
        println!("{failed} check(s) failed");
        ExitCode::FAILURE
    }
}

fn timed<T>(f: impl FnOnce() -> Result<T, String>) -> Result<(T, f64), String> {
    let t = Instant::now();
    f().map(|v| (v, t.elapsed().as_secs_f64() * 1e3))
}

fn config_dir() -> Result<String, String> {
    let paths = oc_config::ConfigPaths::platform();
    let dir = paths.dir();
    if dir.exists() {
        std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let settings = paths.settings_file();
        if settings.exists() {
            std::fs::read(&settings).map_err(|e| format!("{}: {e}", settings.display()))?;
        }
        Ok(format!("{} readable", dir.display()))
    } else {
        Ok(format!("{} (not created yet; defaults apply)", dir.display()))
    }
}

fn fonts() -> Result<String, String> {
    let ((family, n), ms) = timed(|| {
        let lib = FontLibrary::system();
        let family = lib.resolve_family("").ok_or("no monospace font installed")?;
        Ok((family, lib.families(true).len()))
    })?;
    Ok(format!("default monospace \"{family}\", {n} monospace families ({ms:.0} ms)"))
}

fn renderer() -> Result<String, String> {
    let ((w, h, lit), ms) = timed(|| {
        let mut r = HeadlessRenderer::new(FontConfig::default(), 1.0).map_err(|e| e.to_string())?;
        let mut f = Frame::new(12, 2, Rgba::BLACK);
        f.put_str(0, 0, "Orchard ╭─╮", Style::fg(Rgba::WHITE));
        let img = r.render(&f).map_err(|e| e.to_string())?;
        let lit = img.data.chunks(4).filter(|p| p[0] > 128).count();
        if lit == 0 {
            return Err("rendered frame is empty".into());
        }
        Ok((img.width, img.height, lit))
    })?;
    Ok(format!("offscreen GPU device, {w}x{h} px frame, {lit} lit pixels ({ms:.0} ms)"))
}

fn archives() -> Result<String, String> {
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = tmp.path().join("self-test.zip");
    std::fs::write(&path, tiny_zip(b"hello.txt", b"hello")).map_err(|e| e.to_string())?;
    let file = oc_local::local(&path);
    let root = oc_vfs::open_archive(&file).ok_or("archive opener not registered")?.map_err(|e| e.to_string())?;
    let entries = root.list_dir(Default::default()).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut *root.child(&["hello.txt"]).open_read().map_err(|e| e.to_string())?, &mut data)
        .map_err(|e| e.to_string())?;
    if entries.len() != 1 || data != b"hello" {
        return Err(format!("unexpected contents: {entries:?}"));
    }
    let rar = if cfg!(feature = "rar") { "rar included" } else { "built without rar" };
    Ok(format!("zip opened and read through the registered opener ({rar})"))
}

fn highlighting() -> Result<String, String> {
    let (n, ms) = timed(|| {
        let set = oc_preview::highlight::syntax_set();
        let syntax = oc_preview::highlight::syntax_for_file("x.rs").ok_or("no Rust syntax")?;
        let lines =
            oc_preview::highlight::highlight_lines("fn main() { let x = 1; }\n", syntax, &|| false).ok_or("highlighting failed")?;
        if lines.is_empty() {
            return Err("no lines".into());
        }
        Ok(set.syntaxes().len())
    })?;
    Ok(format!("{n} syntaxes loaded, Rust highlighted ({ms:.0} ms)"))
}

/// The connection types are registered (in 1.x's order) and read and write
/// their addresses; the keychain is left alone (it may prompt).
fn connection_types() -> Result<String, String> {
    const EXPECTED: [&str; 6] = ["sftp", "smb", "webdavs", "webdav", "s3", "s3+http"];
    let schemes: Vec<&str> = oc_provider::providers().iter().map(|p| p.scheme()).collect();
    if schemes != EXPECTED {
        return Err(format!("registered {schemes:?}, expected {EXPECTED:?}"));
    }
    for url in [
        "sftp://me@example.com:2222/srv/www",
        "smb://me@nas/media/films",
        "webdavs://me@cloud.example.com/remote.php/dav/files/me",
        "webdav://me@nas:8080/dav",
        "s3://key@minio.local:9000/bucket/folder",
        "s3+http://key@192.168.1.20:9000/bucket",
    ] {
        let scheme = oc_provider::scheme_of(url).ok_or(format!("{url}: no connection type"))?;
        let provider = oc_provider::get(&scheme).ok_or(format!("{scheme}: not registered"))?;
        let params = provider.parse_url(url).map_err(|e| format!("{url}: {e}"))?;
        let back = provider.format_url(&params);
        if back != url {
            return Err(format!("{url} came back as {back}"));
        }
    }
    Ok(format!("{} ({})", EXPECTED.join(", "), oc_provider::address_examples()))
}

/// The system's trusted certificates load (WebDAV / S3 over HTTPS).
fn certificates() -> Result<String, String> {
    let (_, ms) = timed(|| oc_remote_http::tls::system_config().map_err(|e| e.to_string()))?;
    Ok(format!("system trust store loaded ({ms:.0} ms)"))
}

/// A one-file stored zip.
fn tiny_zip(name: &[u8], body: &[u8]) -> Vec<u8> {
    let crc = {
        let mut c = 0xffff_ffffu32;
        for &b in body {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 };
            }
        }
        !c
    };
    let (n, l) = (name.len() as u16, body.len() as u32);
    let mut out = Vec::new();
    let put16 = |v: &mut Vec<u8>, x: u16| v.extend(x.to_le_bytes());
    let local = {
        let mut h = Vec::new();
        h.extend(0x04034b50u32.to_le_bytes());
        for x in [20u16, 0, 0, 0, 0x21] {
            put16(&mut h, x);
        }
        h.extend(crc.to_le_bytes());
        h.extend(l.to_le_bytes());
        h.extend(l.to_le_bytes());
        put16(&mut h, n);
        put16(&mut h, 0);
        h.extend(name);
        h
    };
    out.extend(&local);
    out.extend(body);
    let cd_off = out.len() as u32;
    let mut c = Vec::new();
    c.extend(0x02014b50u32.to_le_bytes());
    for x in [20u16, 20, 0, 0, 0, 0x21] {
        put16(&mut c, x);
    }
    c.extend(crc.to_le_bytes());
    c.extend(l.to_le_bytes());
    c.extend(l.to_le_bytes());
    for x in [n, 0, 0, 0, 0] {
        put16(&mut c, x);
    }
    c.extend(0u32.to_le_bytes());
    c.extend(0u32.to_le_bytes());
    c.extend(name);
    let cd_len = c.len() as u32;
    out.extend(&c);
    out.extend(0x06054b50u32.to_le_bytes());
    for x in [0u16, 0, 1, 1] {
        put16(&mut out, x);
    }
    out.extend(cd_len.to_le_bytes());
    out.extend(cd_off.to_le_bytes());
    put16(&mut out, 0);
    out
}
