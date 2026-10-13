//! Finding the PDFium shared library at run time.

use std::path::{Path, PathBuf};

/// Environment variable naming the PDFium library file, or a folder holding it.
pub const ENV_VAR: &str = "OC_PDFIUM_PATH";

/// The library's file name on this platform.
pub fn library_name() -> &'static str {
    if cfg!(windows) {
        "pdfium.dll"
    } else if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else {
        "libpdfium.so"
    }
}

/// The platform folder of `scripts/fetch-pdfium.sh` (bblanchon/pdfium-binaries
/// naming): "mac-arm64", "win-x64", "linux-x64", …
pub fn platform_dir() -> String {
    let os = if cfg!(windows) {
        "win"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else {
        "linux"
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "x86",
        other => other,
    };
    format!("{os}-{arch}")
}

/// Where the library is looked for: only `$OC_PDFIUM_PATH` if set (the file,
/// or a folder containing it), otherwise in order
///
/// 1. next to the executable (Windows, Linux tarballs, `cargo run`) and in
///    its `lib` folder (Linux packages: /opt/orchard-commander/lib)
/// 2. `../Frameworks` of the executable (a macOS app bundle) and `../lib`
///    (an FHS layout: bin/ and lib/ side by side)
/// 3. debug builds only: the workspace's `vendor/pdfium/<platform>/`
/// 4. the bare name, for the system's library search path (not on Windows,
///    where that search includes the current directory: DLL planting)
pub fn candidates() -> Vec<PathBuf> {
    candidates_for(std::env::var_os(ENV_VAR).filter(|v| !v.is_empty()).map(PathBuf::from))
}

fn candidates_for(env: Option<PathBuf>) -> Vec<PathBuf> {
    let name = library_name();
    if let Some(p) = env {
        return vec![if p.is_dir() { p.join(name) } else { p }];
    }
    let mut out = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        out.push(dir.join(name));
        // Linux packages: /opt/orchard-commander/lib (with the FFmpeg libraries).
        out.push(dir.join("lib").join(name));
        // Test binaries live in target/<profile>/deps; `cargo run` ones one level up.
        if dir.ends_with("deps")
            && let Some(up) = dir.parent()
        {
            out.push(up.join(name));
        }
        if let Some(up) = dir.parent() {
            out.push(up.join("Frameworks").join(name));
            out.push(up.join("lib").join(name));
        }
    }
    if cfg!(debug_assertions) {
        let vendor = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/pdfium").join(platform_dir());
        out.push(vendor.join(name));
    }
    if !cfg!(windows) {
        out.push(PathBuf::from(name));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_end_with_the_bare_name_except_on_windows() {
        let c = candidates_for(None);
        assert_eq!(c.last().unwrap() == Path::new(library_name()), !cfg!(windows));
        assert!(c.iter().any(|p| p.ends_with(Path::new("Frameworks").join(library_name()))));
        assert!(platform_dir().contains('-'));
    }

    #[test]
    fn the_environment_variable_overrides_everything() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(candidates_for(Some(dir.path().into())), [dir.path().join(library_name())]);
        let file = dir.path().join("custom-pdfium.so");
        assert_eq!(candidates_for(Some(file.clone())), [file]);
    }
}
