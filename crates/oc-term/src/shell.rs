//! Which shell to run, with which environment, and where it is.
//!
//! Linked folders need the shell's current directory: shells that send OSC 7
//! say so themselves (PowerShell gets a prompt hook for it, since its
//! `Set-Location` doesn't change the process's directory); for the rest we
//! ask the OS for the shell process's working directory.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// PowerShell's prompt, wrapped so each prompt reports the folder (OSC 7).
pub const POWERSHELL_INTEGRATION: &str = "$__orchardPrompt = $function:prompt; \
     function global:prompt { \
     $p = (Get-Location).ProviderPath -replace '\\\\', '/'; \
     [Console]::Write([char]27 + ']7;file:///' + $p + [char]7); & $__orchardPrompt }";

/// `argv[0]`'s file name without extension, lowercase ("pwsh", "zsh", …).
fn program_stem(argv: &[String]) -> String {
    argv.first()
        .and_then(|p| Path::new(p).file_stem())
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// The session's title: the shell's name ("zsh", "pwsh").
pub fn title(argv: &[String]) -> String {
    argv.first()
        .and_then(|p| Path::new(p).file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn is_powershell(argv: &[String]) -> bool {
    matches!(program_stem(argv).as_str(), "pwsh" | "powershell")
}

/// `argv` plus what makes the shell report its folder, where needed (PowerShell).
pub fn with_integration(argv: &[String]) -> Vec<String> {
    let mut argv = argv.to_vec();
    if is_powershell(&argv) && !argv.iter().any(|a| a == "-Command") {
        argv.extend([
            "-NoExit".into(),
            "-Command".into(),
            POWERSHELL_INTEGRATION.into(),
        ]);
    }
    argv
}

/// `file://host/path` (percent-encoded) → a path; `None` for anything else.
pub fn path_from_osc7(value: &str) -> Option<PathBuf> {
    // urlparse: the scheme is case-insensitive; the path ends at ? or #.
    let (scheme, rest) = value.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let path = match rest.strip_prefix("//") {
        Some(after_host) => &after_host[after_host.find('/').unwrap_or(after_host.len())..],
        None => rest,
    };
    if path.is_empty() {
        return None;
    }
    let bytes: Vec<u8> = percent_encoding::percent_decode_str(path).collect();
    decoded_path(bytes)
}

#[cfg(unix)]
fn decoded_path(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn decoded_path(bytes: Vec<u8>) -> Option<PathBuf> {
    let path = String::from_utf8_lossy(&bytes);
    let b = path.as_bytes();
    // /C:/Users → C:/Users
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        return Some(PathBuf::from(&path[1..]));
    }
    Some(PathBuf::from(path.into_owned()))
}

/// A process's working directory (macOS, Linux); `None` where it can't be read.
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    imp::process_cwd(pid)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    pub fn process_cwd(pid: u32) -> Option<PathBuf> {
        let pid = i32::try_from(pid).ok()?;
        // SAFETY: proc_vnodepathinfo is plain old data; all zeroes is a valid value.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY: the buffer is a properly sized, writable proc_vnodepathinfo.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        if written <= 0 {
            return None;
        }
        // The cwd's path: MAXPATHLEN chars, NUL-terminated (libc splits the array in 32 × 32).
        let raw = &info.pvi_cdir.vip_path;
        // SAFETY: [[c_char; 32]; 32] is 1024 contiguous bytes.
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(raw.as_ptr().cast(), 1024) };
        let path = CStr::from_bytes_until_nul(bytes).ok()?.to_bytes();
        (!path.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::path::PathBuf;

    pub fn process_cwd(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
mod imp {
    use std::path::PathBuf;

    pub fn process_cwd(_pid: u32) -> Option<PathBuf> {
        None
    }
}

/// `s` quoted for a POSIX shell exactly as Python's `shlex.quote` does.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if s.chars().all(safe) {
        return s.into();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// What to type so the shell changes to `folder` (leading space: kept out of
/// most histories).
pub fn cd_command(argv: &[String], folder: &Path) -> String {
    let folder = folder.to_string_lossy();
    if is_powershell(argv) {
        // Esc clears the line in PSReadLine
        return format!(
            "\x1b Set-Location -LiteralPath '{}'\r",
            folder.replace('\'', "''")
        );
    }
    if program_stem(argv) == "cmd" {
        return format!("\x1b cd /d \"{folder}\"\r");
    }
    format!("\x15 cd {}\r", shell_quote(&folder)) // Ctrl+U: clear what was typed first
}

/// `name` found on PATH, like `shutil.which`.
#[cfg(windows)]
fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// The user's shell as a command line: their login shell, or PowerShell on Windows.
#[cfg(windows)]
pub fn default_shell() -> Vec<String> {
    for name in ["pwsh.exe", "powershell.exe"] {
        if let Some(path) = which(name) {
            return vec![path.to_string_lossy().into_owned(), "-NoLogo".into()];
        }
    }
    vec![std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())]
}

/// The user's shell as a command line: their login shell, or PowerShell on Windows.
#[cfg(not(windows))]
pub fn default_shell() -> Vec<String> {
    let mut shell = std::env::var("SHELL").unwrap_or_default();
    if shell.is_empty() {
        shell = passwd_shell().unwrap_or_default();
    }
    if shell.is_empty() || !Path::new(&shell).exists() {
        shell = "/bin/sh".into();
    }
    // A login shell reads the user's profile: apps started from the Dock get a minimal PATH otherwise.
    vec![shell, "-l".into()]
}

/// The login shell from the password database.
#[cfg(not(any(unix, windows)))]
fn passwd_shell() -> Option<String> {
    None
}

/// The login shell from the password database.
#[cfg(unix)]
fn passwd_shell() -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: passwd is plain old data; getpwuid_r fills it, pointing into `buf`.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the sizes given.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_shell.is_null() {
        return None;
    }
    // SAFETY: pw_shell is a NUL-terminated string inside `buf`, alive here.
    let shell = unsafe { std::ffi::CStr::from_ptr(pwd.pw_shell) };
    Some(shell.to_string_lossy().into_owned())
}

/// The command for a configured shell ("" = the default), split like a
/// command line. Errors with Python's message on unbalanced quotes.
pub fn shell_command(configured: &str) -> Result<Vec<String>, String> {
    let configured = configured.trim();
    if configured.is_empty() {
        return Ok(default_shell());
    }
    if cfg!(windows) {
        return Ok(vec![configured.into()]);
    }
    let argv = split_posix(configured)?;
    Ok(if argv.is_empty() {
        default_shell()
    } else {
        argv
    })
}

/// Python's `shlex.split` (POSIX mode, no comments), error messages included.
/// The `shlex` crate differs where a configured shell would notice: it treats
/// `#` as a comment and unescapes `\$` and `` \` `` inside double quotes.
fn split_posix(s: &str) -> Result<Vec<String>, String> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Space,
        Word,
        Single,
        Double,
        /// After a backslash; the state to return to.
        Escape(Option<char>),
    }
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    let mut words = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut state = State::Space;
    for c in s.chars() {
        state = match state {
            State::Space | State::Word if is_space(c) => {
                if state == State::Word || quoted {
                    words.push(std::mem::take(&mut token));
                    quoted = false;
                }
                State::Space
            }
            State::Space | State::Word => match c {
                '\\' => State::Escape(None),
                '\'' => State::Single,
                '"' => State::Double,
                _ => {
                    token.push(c);
                    State::Word
                }
            },
            State::Single | State::Double => {
                quoted = true;
                match (state, c) {
                    (State::Single, '\'') | (State::Double, '"') => State::Word,
                    (State::Double, '\\') => State::Escape(Some('"')),
                    _ => {
                        token.push(c);
                        state
                    }
                }
            }
            State::Escape(inside) => {
                // In double quotes only \ and " are escaped; the backslash stays otherwise.
                if inside.is_some_and(|q| c != q && c != '\\') {
                    token.push('\\');
                }
                token.push(c);
                if inside.is_some() {
                    State::Double
                } else {
                    State::Word
                }
            }
        };
    }
    match state {
        State::Single | State::Double => Err("a quote isn't closed".into()),
        State::Escape(_) => Err("it ends with a lone backslash".into()),
        State::Word => {
            words.push(token);
            Ok(words)
        }
        State::Space => Ok(words),
    }
}

/// What the session sets on top of the inherited environment. `get` reads
/// the inherited value (so tests don't depend on the real environment).
pub fn environment_overrides(
    get: impl Fn(&str) -> Option<OsString>,
) -> Vec<(&'static str, &'static str)> {
    let mut env = vec![
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "OrchardCommander"),
    ];
    let has_locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .any(|k| get(k).is_some_and(|v| !v.is_empty()));
    if !cfg!(windows) && !has_locale {
        env.push(("LANG", "en_US.UTF-8")); // apps from the Dock have no locale: shells would fall back to ASCII
    }
    env
}

/// The shell's whole environment: ours plus the overrides above.
pub fn shell_environment() -> Vec<(OsString, OsString)> {
    let overrides = environment_overrides(|k| std::env::var_os(k));
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| !overrides.iter().any(|(o, _)| k == o))
        .collect();
    env.extend(overrides.into_iter().map(|(k, v)| (k.into(), v.into())));
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[cfg(unix)]
    #[test]
    fn default_shell_is_a_login_shell() {
        let shell = default_shell();
        assert!(Path::new(&shell[0]).exists());
        assert_eq!(shell[1..], ["-l"]); // a login shell: the user's PATH
        assert_eq!(shell_command("/bin/zsh -f").unwrap(), ["/bin/zsh", "-f"]);
        assert_eq!(shell_command("").unwrap(), shell);
        assert_eq!(shell_command("  ").unwrap(), shell);
        assert_eq!(
            shell_command("sh -c 'echo a b'").unwrap(),
            ["sh", "-c", "echo a b"]
        );
        assert!(shell_command("sh 'oops").is_err());
    }

    #[test]
    fn splits_like_python_shlex() {
        let split = |s: &str| split_posix(s);
        let ok = |parts: &[&str]| Ok(argv(parts));
        assert_eq!(split("zsh #x"), ok(&["zsh", "#x"])); // no comments
        assert_eq!(split(r#"a"\$b""#), ok(&[r"a\$b"]));
        assert_eq!(split(r"a\"), Err("it ends with a lone backslash".into()));
        assert_eq!(split(r#""""#), ok(&[""]));
        assert_eq!(split("a'b'c d"), ok(&["abc", "d"]));
        assert_eq!(split(r#"x "a\"b" y"#), ok(&["x", r#"a"b"#, "y"]));
        assert_eq!(split(r"'a\b'"), ok(&[r"a\b"]));
        assert_eq!(
            split(r#"fish -C "echo #hi""#),
            ok(&["fish", "-C", "echo #hi"])
        );
        assert_eq!(split(r#""a\\b""#), ok(&[r"a\b"]));
        assert_eq!(split(r"a\ b"), ok(&["a b"]));
        assert_eq!(split(" \tx\n "), ok(&["x"]));
        assert_eq!(split("'oops"), Err("a quote isn't closed".into()));
        assert_eq!(split("\"oops"), Err("a quote isn't closed".into()));
        assert_eq!(split("ä 'ö ü'"), ok(&["ä", "ö ü"]));
    }

    #[test]
    fn environment() {
        let env = environment_overrides(|_| None);
        assert!(env.contains(&("TERM", "xterm-256color")));
        assert!(env.contains(&("COLORTERM", "truecolor")));
        assert!(env.contains(&("TERM_PROGRAM", "OrchardCommander")));
        assert_eq!(env.iter().any(|(k, _)| *k == "LANG"), !cfg!(windows));
        let with_locale =
            environment_overrides(|k| (k == "LC_CTYPE").then(|| "de_DE.UTF-8".into()));
        assert!(!with_locale.iter().any(|(k, _)| *k == "LANG"));
        let empty_locale = environment_overrides(|k| (k == "LANG").then(OsString::new));
        assert_eq!(
            empty_locale.iter().any(|(k, _)| *k == "LANG"),
            !cfg!(windows)
        );
        let full = shell_environment();
        assert_eq!(full.iter().filter(|(k, _)| k == "TERM").count(), 1);
        assert!(
            full.iter()
                .any(|(k, v)| k == "TERM" && v == "xterm-256color")
        );
    }

    #[test]
    fn powershell_integration() {
        assert!(is_powershell(&argv(&["C:/x/pwsh.exe", "-NoLogo"])));
        assert!(is_powershell(&argv(&["PowerShell.EXE"])));
        assert!(!is_powershell(&argv(&["/bin/zsh"])));
        let integrated = with_integration(&argv(&["pwsh.exe", "-NoLogo"]));
        assert_eq!(
            integrated[..4],
            ["pwsh.exe", "-NoLogo", "-NoExit", "-Command"]
        );
        assert!(integrated[4].contains("]7;file:///"));
        assert_eq!(
            with_integration(&argv(&["pwsh", "-Command", "x"])),
            argv(&["pwsh", "-Command", "x"])
        );
        assert_eq!(with_integration(&argv(&["/bin/sh"])), argv(&["/bin/sh"]));
        assert_eq!(title(&argv(&["/usr/local/bin/zsh", "-l"])), "zsh");
    }

    #[test]
    fn osc7_paths() {
        assert_eq!(
            path_from_osc7("file://host/Users/me/My%20Dir"),
            Some(PathBuf::from("/Users/me/My Dir"))
        );
        assert_eq!(path_from_osc7("file:///tmp"), Some(PathBuf::from("/tmp")));
        assert_eq!(
            path_from_osc7("FILE:///tmp?x#y"),
            Some(PathBuf::from("/tmp"))
        );
        assert_eq!(path_from_osc7("file:/tmp/a"), Some(PathBuf::from("/tmp/a")));
        assert_eq!(path_from_osc7("file://host"), None);
        assert_eq!(path_from_osc7("http://host/x"), None);
        assert_eq!(path_from_osc7("garbage"), None);
        assert_eq!(path_from_osc7("file:///%C3%A4"), Some(PathBuf::from("/ä")));
        #[cfg(windows)]
        assert_eq!(
            path_from_osc7("file:///C:/Users"),
            Some(PathBuf::from("C:/Users"))
        );
    }

    #[test]
    fn quoting_and_cd() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("/a/b-c_d.e"), "/a/b-c_d.e");
        assert_eq!(shell_quote("my dir"), "'my dir'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_quote("ä"), "'ä'"); // shlex.quote is ASCII-only
        assert_eq!(
            cd_command(&argv(&["/bin/zsh"]), Path::new("/tmp/my dir")),
            "\x15 cd '/tmp/my dir'\r"
        );
        assert_eq!(
            cd_command(&argv(&["pwsh.exe"]), Path::new("C:/it's")),
            "\x1b Set-Location -LiteralPath 'C:/it''s'\r"
        );
        assert_eq!(
            cd_command(&argv(&["C:/Windows/cmd.exe"]), Path::new("D:/x")),
            "\x1b cd /d \"D:/x\"\r"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn cwd_of_this_process() {
        let here = std::env::current_dir().unwrap().canonicalize().unwrap();
        let cwd = process_cwd(std::process::id()).unwrap();
        assert_eq!(cwd.canonicalize().unwrap(), here);
        assert_eq!(process_cwd(u32::MAX), None);
    }
}
