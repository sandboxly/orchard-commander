//! Options > About Orchard Commander (macOS: the application menu): version,
//! license, the bundled third-party components and a button that opens the
//! folder with their license texts (`licenses/` of the package, see
//! licenses/README.md).

use std::path::{Path, PathBuf};

use crate::app::{App, Severity};
use crate::commands::{Command, MenuRole, Registry};
use crate::dialogs::MessageBox;
use crate::menus::Menu;

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("app.about", "About Orchard Commander", show)
            .help("Version and licenses")
            .menu(Menu::Options, "about")
            .menu_label("About Orchard Commander…")
            .role(MenuRole::About),
    );
}

fn show(app: &mut App) {
    let folder = licenses_dir();
    let buttons: &[&str] = if folder.is_some() { &["Close", "Show &licenses"] } else { &["Close"] };
    let mb = MessageBox::new("About Orchard Commander", &about_text(folder.as_deref()), buttons, 0).width(68).on_choice(
        move |app, choice| {
            if let (Some(1), Some(dir)) = (choice, folder) {
                app.tasks.run_in_background(
                    move |_| oc_local::open_with_default_app(&dir),
                    |app, result| {
                        if let Err(e) = result {
                            app.notify(format!("Can't open the licenses folder: {e}"), Severity::Error);
                        }
                    },
                );
            }
        },
    );
    app.show(mb);
}

pub fn about_text(licenses: Option<&Path>) -> String {
    let mut text = format!(
        "Orchard Commander {}\n\
         A two-panel file manager for macOS, Windows and Linux.\n\
         https://sandboxly.github.io/orchard-commander/\n\n\
         Copyright (c) 2026 Stephan Lengl. MIT license.\n\n\
         Includes FFmpeg (LGPL 3.0 or later), PDFium (BSD-3-Clause), UnRAR (RARLAB's \
         UnRAR license: freeware) and Rust crates under their own licenses.",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(dir) = licenses {
        text.push_str(&format!(" The license texts are in\n{}", dir.display()));
    }
    text
}

/// The packaged license folder: next to the executable (Windows, Linux),
/// in the macOS bundle's Resources, or the source tree's for development
/// builds.
pub fn licenses_dir() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let mut candidates = vec![exe_dir.join("licenses"), exe_dir.join("../Resources/licenses")];
    if cfg!(debug_assertions) {
        candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../licenses"));
    }
    candidates.into_iter().find(|p| p.is_dir()).map(|p| std::fs::canonicalize(&p).unwrap_or(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_names_version_and_folder() {
        let text = about_text(Some(Path::new("/opt/orchard-commander/licenses")));
        assert!(text.starts_with(&format!("Orchard Commander {}\n", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains("FFmpeg") && text.contains("UnRAR"));
        assert!(text.ends_with("/opt/orchard-commander/licenses"));
        assert!(!about_text(None).contains("license texts are in"));
    }

    #[test]
    fn development_builds_find_the_source_tree_licenses() {
        let dir = licenses_dir().expect("licenses folder");
        assert!(dir.join("UnRAR.txt").is_file(), "{}", dir.display());
    }
}
