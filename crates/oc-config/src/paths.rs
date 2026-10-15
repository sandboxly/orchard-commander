//! Where everything is stored: the same directory the Python version gets from
//! `platformdirs.user_config_dir(...)`, so existing users keep their settings.
//!
//! - macOS: `~/Library/Application Support/Orchard Commander`
//! - Windows: `%LOCALAPPDATA%\Orchard Commander` (platformdirs' default is not roaming)
//! - Linux: `$XDG_CONFIG_HOME/orchard-commander` or `~/.config/orchard-commander`
//!   (config dirs there conventionally avoid spaces)
//!
//! platformdirs also honours an absolute `$XDG_CONFIG_HOME` on macOS
//! (`$XDG_CONFIG_HOME/Orchard Commander`), and `WIN_PD_OVERRIDE_LOCAL_APPDATA`
//! on Windows; so does this module.
//!
//! The platform directory is worked out in [`orchard_theme::dirs`], which
//! the other Orchard apps use to find the shared `color-schemes/` folder, so
//! they always agree with this one.
//!
//! Nothing here is a global: every store takes a [`ConfigPaths`], so tests
//! (and a portable install) can point everything at another directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub use orchard_theme::dirs::{APP_NAME, COMMAND, CONFIG_DIR_ENV, LEGACY_NAME};

/// The config directory and the files in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigPaths {
    dir: PathBuf,
    legacy_dir: Option<PathBuf>,
}

impl ConfigPaths {
    /// The platform's config directory, or `$ORCHARD_COMMANDER_CONFIG_DIR` if
    /// set (then there is no legacy directory to migrate from).
    pub fn platform() -> Self {
        ConfigPaths {
            dir: orchard_theme::dirs::commander_config_dir(),
            legacy_dir: orchard_theme::dirs::commander_legacy_dir(),
        }
    }

    /// Everything in `dir`, with no legacy directory.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        ConfigPaths {
            dir: dir.into(),
            legacy_dir: None,
        }
    }

    /// Everything in `dir`; `legacy_dir` is migrated by [`migrate_legacy_config`](Self::migrate_legacy_config).
    pub fn with_legacy(dir: impl Into<PathBuf>, legacy_dir: impl Into<PathBuf>) -> Self {
        ConfigPaths {
            dir: dir.into(),
            legacy_dir: Some(legacy_dir.into()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn legacy_dir(&self) -> Option<&Path> {
        self.legacy_dir.as_deref()
    }

    pub fn settings_file(&self) -> PathBuf {
        self.dir.join("settings.json")
    }

    pub fn bookmarks_file(&self) -> PathBuf {
        self.dir.join("bookmarks.json")
    }

    pub fn connections_file(&self) -> PathBuf {
        self.dir.join("connections.json")
    }

    /// User color schemes, one TOML file each.
    pub fn color_schemes_dir(&self) -> PathBuf {
        self.dir.join("color-schemes")
    }

    /// Where connection providers keep their files (e.g. SFTP's `known_hosts`).
    pub fn data_dir(&self) -> &Path {
        &self.dir
    }

    /// Copy settings, saved connections and trusted host keys from the old
    /// "fcmd" config directory, once, if the new one doesn't exist yet. The
    /// old one is kept as a backup. Returns whether anything was copied.
    ///
    /// The copy is made next to the new directory and renamed into place,
    /// so an interrupted migration is redone on the next start instead of
    /// leaving a half-copied directory that counts as migrated. Like
    /// `shutil.copytree`, a file that can't be copied doesn't stop the
    /// rest; the first such error is returned after the directory is in place.
    pub fn migrate_legacy_config(&self) -> io::Result<bool> {
        let Some(legacy) = &self.legacy_dir else {
            return Ok(false);
        };
        if self.dir.exists() || !legacy.is_dir() {
            return Ok(false);
        }
        let mut staging = self.dir.as_os_str().to_owned();
        staging.push(".migrating");
        let staging = PathBuf::from(staging);
        if staging.exists() {
            fs::remove_dir_all(&staging)?; // left over from an interrupted run
        }
        fs::create_dir_all(&staging)?;
        copy_permissions(legacy, &staging);
        let mut first_error = None;
        copy_tree(legacy, &staging, &mut first_error);
        if let Err(e) = fs::rename(&staging, &self.dir) {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(true),
        }
    }
}

/// `shutil.copytree` into the existing `dst`: symlinks are followed, and
/// failures are recorded (the first in `error`) rather than stopping the copy.
fn copy_tree(src: &Path, dst: &Path, error: &mut Option<io::Error>) {
    fn note(error: &mut Option<io::Error>, e: io::Error) {
        error.get_or_insert(e);
    }
    let entries = match fs::read_dir(src) {
        Ok(entries) => entries,
        Err(e) => return note(error, e),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                note(error, e);
                continue;
            }
        };
        let from = entry.path();
        let to = dst.join(entry.file_name());
        match fs::metadata(&from) {
            Ok(meta) if meta.is_dir() => match fs::create_dir(&to) {
                Ok(()) => {
                    copy_tree(&from, &to, error);
                    copy_permissions(&from, &to);
                }
                Err(e) => note(error, e),
            },
            Ok(_) => {
                if let Err(e) = fs::copy(&from, &to) {
                    note(error, e);
                }
            }
            Err(e) => note(error, e), // e.g. a dangling symlink
        }
    }
}

/// A directory's permissions onto its copy, as `copytree` does (best effort:
/// the files are what matters).
fn copy_permissions(from: &Path, to: &Path) {
    if let Ok(meta) = fs::metadata(from) {
        let _ = fs::set_permissions(to, meta.permissions());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_live_in_the_config_dir() {
        let p = ConfigPaths::new("/cfg");
        assert_eq!(p.settings_file(), Path::new("/cfg/settings.json"));
        assert_eq!(p.bookmarks_file(), Path::new("/cfg/bookmarks.json"));
        assert_eq!(p.connections_file(), Path::new("/cfg/connections.json"));
        assert_eq!(p.color_schemes_dir(), Path::new("/cfg/color-schemes"));
        assert_eq!(p.data_dir(), Path::new("/cfg"));
    }

    #[test]
    fn platform_dirs_agree_with_the_other_orchard_apps() {
        // The other apps find the shared color schemes through orchard_theme.
        let p = ConfigPaths::platform();
        assert_eq!(p.dir(), orchard_theme::dirs::commander_config_dir());
        assert_eq!(p.color_schemes_dir(), orchard_theme::dirs::schemes_dir());
        assert_eq!(
            p.legacy_dir().map(Path::to_path_buf),
            orchard_theme::dirs::commander_legacy_dir()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn platform_dir_matches_platformdirs_on_macos() {
        // platformdirs honours an absolute $XDG_CONFIG_HOME on macOS too.
        let xdg = std::env::var("XDG_CONFIG_HOME").unwrap_or_default();
        if std::env::var_os(CONFIG_DIR_ENV).is_some() || xdg.trim().starts_with('/') {
            return;
        }
        let home = dirs::home_dir().unwrap();
        let p = ConfigPaths::platform();
        assert_eq!(
            p.dir(),
            home.join("Library/Application Support/Orchard Commander")
        );
        assert_eq!(
            p.legacy_dir().unwrap(),
            home.join("Library/Application Support/fcmd")
        );
    }

    #[test]
    fn legacy_config_directory_is_migrated() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("fcmd");
        fs::create_dir_all(legacy.join("sub")).unwrap();
        fs::write(legacy.join("settings.json"), r#"{"theme": "nord"}"#).unwrap();
        fs::write(legacy.join("connections.json"), "[]").unwrap();
        fs::write(legacy.join("known_hosts"), "host key").unwrap();
        fs::write(legacy.join("sub/x"), "nested").unwrap();
        let paths = ConfigPaths::with_legacy(tmp.path().join("Orchard Commander"), &legacy);
        assert!(paths.migrate_legacy_config().unwrap());
        assert_eq!(
            crate::Settings::load(&paths.settings_file()).color_scheme,
            "nord"
        ); // was "theme"
        assert_eq!(
            fs::read_to_string(paths.dir().join("known_hosts")).unwrap(),
            "host key"
        );
        assert_eq!(
            fs::read_to_string(paths.dir().join("sub/x")).unwrap(),
            "nested"
        );
        assert!(legacy.exists()); // kept as a backup
        assert!(!paths.migrate_legacy_config().unwrap()); // only once
    }

    #[cfg(unix)]
    #[test]
    fn migration_copies_what_it_can_and_reports_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("fcmd");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("settings.json"), "{}").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("gone"), legacy.join("dangling")).unwrap();
        let paths = ConfigPaths::with_legacy(tmp.path().join("new"), &legacy);
        assert!(paths.migrate_legacy_config().is_err()); // like copytree's Error
        assert!(paths.settings_file().exists()); // everything else is there
        assert!(!tmp.path().join("new.migrating").exists());
        assert!(!paths.migrate_legacy_config().unwrap()); // done
    }

    #[test]
    fn an_interrupted_migration_is_redone() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("fcmd");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("settings.json"), "{}").unwrap();
        let staging = tmp.path().join("new.migrating");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("half"), "x").unwrap();
        let paths = ConfigPaths::with_legacy(tmp.path().join("new"), &legacy);
        assert!(paths.migrate_legacy_config().unwrap());
        assert!(paths.settings_file().exists());
        assert!(!paths.dir().join("half").exists() && !staging.exists());
    }

    #[test]
    fn nothing_to_migrate() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::with_legacy(tmp.path().join("new"), tmp.path().join("missing"));
        assert!(!paths.migrate_legacy_config().unwrap());
        assert!(!paths.dir().exists());
        assert!(
            !ConfigPaths::new(tmp.path().join("x"))
                .migrate_legacy_config()
                .unwrap()
        );
    }
}
