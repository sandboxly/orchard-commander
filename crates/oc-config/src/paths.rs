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
//! Nothing here is a global: every store takes a [`ConfigPaths`], so tests
//! (and a portable install) can point everything at another directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The app's name: the config directory on macOS and Windows, and the keychain service.
pub const APP_NAME: &str = "Orchard Commander";
/// The command name: the config directory on Linux.
pub const COMMAND: &str = "orchard-commander";
/// The name before the rename; its config directory is migrated once.
pub const LEGACY_NAME: &str = "fcmd";

/// Overrides the config directory (tests, portable installs). Not in the Python version.
pub const CONFIG_DIR_ENV: &str = "ORCHARD_COMMANDER_CONFIG_DIR";

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
        if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|d| !d.is_empty()) {
            return ConfigPaths {
                dir: PathBuf::from(dir),
                legacy_dir: None,
            };
        }
        let base = user_config_base();
        let name = if cfg!(target_os = "linux") {
            COMMAND
        } else {
            APP_NAME
        };
        ConfigPaths {
            dir: base.join(name),
            legacy_dir: Some(base.join(LEGACY_NAME)),
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

/// Replace `path`'s contents without a reader ever seeing a half-written
/// file: other app instances re-read settings and connections before each
/// save, and a torn file would read as "nothing saved" and be written back
/// that way. Writes go through a symlinked file (dotfile setups) to its
/// target. If staging is refused (a read-only directory; on Windows, a file
/// another process has open), the file is written in place as the Python
/// version does.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = target.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let staged = fs::write(&tmp, contents).and_then(|()| fs::rename(&tmp, &target));
    if staged.is_err() {
        let _ = fs::remove_file(&tmp);
        fs::write(&target, contents)?;
    }
    Ok(())
}

/// platformdirs' (>= 4.12, as the Python version requires) `user_config_dir()`
/// without the app name.
fn user_config_base() -> PathBuf {
    if cfg!(windows) {
        // The local (not roaming) AppData, unless platformdirs' override is set.
        let override_dir = env_trimmed("WIN_PD_OVERRIDE_LOCAL_APPDATA")
            .filter(|p| is_fully_qualified_nt(p))
            .map(PathBuf::from);
        return override_dir
            .or_else(dirs::data_local_dir)
            .unwrap_or_default();
    }
    unix_config_base(
        env_trimmed("XDG_CONFIG_HOME"),
        dirs::home_dir,
        cfg!(target_os = "macos"),
    )
}

/// On every Unix, macOS included, an absolute `$XDG_CONFIG_HOME` wins; a
/// relative or blank one is ignored (the XDG spec says so).
fn unix_config_base(
    xdg: Option<String>,
    home: impl FnOnce() -> Option<PathBuf>,
    macos: bool,
) -> PathBuf {
    if let Some(xdg) = xdg.filter(|p| p.starts_with('/')) {
        return PathBuf::from(xdg);
    }
    let home = home().unwrap_or_default();
    if macos {
        home.join("Library/Application Support")
    } else {
        home.join(".config")
    }
}

/// An environment variable with surrounding whitespace removed (Python's `.strip()`).
fn env_trimmed(name: &str) -> Option<String> {
    let value = std::env::var_os(name)?;
    Some(value.to_string_lossy().trim().to_owned())
}

/// platformdirs' `_is_fully_qualified` for Windows paths: a UNC path or a
/// drive with a root ("C:\x"), not "C:x" or "\x".
#[cfg_attr(not(windows), allow(dead_code))]
fn is_fully_qualified_nt(path: &str) -> bool {
    let is_sep = |c: char| c == '\\' || c == '/';
    let mut chars = path.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(a), Some(b), _) if is_sep(a) && is_sep(b) => true,
        (Some(_), Some(':'), Some(c)) => is_sep(c),
        _ => false,
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
    fn xdg_config_home_like_platformdirs() {
        let home = || Some(PathBuf::from("/home/me"));
        let base = |xdg: Option<&str>, macos| unix_config_base(xdg.map(String::from), home, macos);
        assert_eq!(base(None, false), Path::new("/home/me/.config"));
        assert_eq!(
            base(None, true),
            Path::new("/home/me/Library/Application Support")
        );
        assert_eq!(base(Some("/xdg"), false), Path::new("/xdg"));
        assert_eq!(base(Some("/xdg"), true), Path::new("/xdg")); // macOS too
        assert_eq!(base(Some("rel/dir"), false), Path::new("/home/me/.config"));
        assert_eq!(
            base(Some(""), true),
            Path::new("/home/me/Library/Application Support")
        );
    }

    #[test]
    fn windows_override_must_be_fully_qualified() {
        assert!(is_fully_qualified_nt("C:\\Data"));
        assert!(is_fully_qualified_nt("d:/data"));
        assert!(is_fully_qualified_nt("\\\\server\\share"));
        assert!(!is_fully_qualified_nt("C:data"));
        assert!(!is_fully_qualified_nt("\\data"));
        assert!(!is_fully_qualified_nt("data"));
        assert!(!is_fully_qualified_nt(""));
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
    fn atomic_writes_replace_and_leave_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/b/settings.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_writes_go_through_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("dotfiles/settings.json");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "old").unwrap();
        let link = tmp.path().join("cfg/settings.json");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
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
