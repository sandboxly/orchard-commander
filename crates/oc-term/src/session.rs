//! One shell in a pseudo-terminal (portable-pty: openpty on Unix, ConPTY on
//! Windows), its output fed into a [`Screen`] on a reader thread.
//!
//! Thread layout: a reader thread parses output under the screen lock and
//! then notifies the UI; a writer thread owns the pty's input so neither the
//! UI nor the reader ever blocks on a full pty buffer (a big paste while the
//! shell is busy printing would otherwise deadlock them against each other).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Instant;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::screen::Screen;
use crate::shell;

/// What the session tells the UI. Sent from the session's own threads (the
/// reader, mostly): hop to the UI thread before touching widgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// New output was processed: redraw (coalesce these, they come per read).
    Output,
    /// The shell exited, with its status (`None`: killed by a signal / unknown).
    Exited(Option<i32>),
}

pub type Notify = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// Why a shell couldn't be started.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("already started")]
    AlreadyStarted,
    #[error("no shell command")]
    NoCommand,
    #[error("{0}")]
    Pty(String),
}

struct Process {
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    pid: Option<u32>,
    input: Sender<Vec<u8>>,
}

struct Inner {
    screen: Mutex<Screen>,
    process: Mutex<Option<Process>>,
    notify: Notify,
    started: AtomicBool,
    exited: AtomicBool,
    exit_status: Mutex<Option<i32>>,
    /// Windows: a command runs (Enter typed, no prompt since).
    busy: AtomicBool,
    commands: AtomicU64,
    /// Set (under this lock) before the reader reaps the shell: after that its
    /// pid may belong to another process, so `close` must not SIGKILL it.
    reaping: Mutex<bool>,
    /// A thread waits to apply a synchronized update that never ended.
    sync_flush: AtomicBool,
}

impl Inner {
    fn process(&self) -> MutexGuard<'_, Option<Process>> {
        self.process.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, data: Vec<u8>) {
        if let Some(p) = self.process().as_ref() {
            let _ = p.input.send(data);
        }
    }
}

/// One shell in a pseudo-terminal. Thread-safe: read the screen through
/// [`Session::lock`]; every other method may be called from any thread.
pub struct Session {
    argv: Vec<String>,
    cwd: PathBuf,
    inner: Arc<Inner>,
}

impl Session {
    /// A session for `argv` (see [`shell::default_shell`]) to start in `cwd`.
    /// Nothing runs until [`Session::start`]; the screen exists already, so
    /// a failure message can be fed into it.
    pub fn new(
        argv: Vec<String>,
        cwd: impl Into<PathBuf>,
        columns: usize,
        lines: usize,
        notify: Notify,
    ) -> Self {
        Self {
            argv,
            cwd: cwd.into(),
            inner: Arc::new(Inner {
                screen: Mutex::new(Screen::new(columns, lines)),
                process: Mutex::new(None),
                notify,
                started: AtomicBool::new(false),
                exited: AtomicBool::new(false),
                exit_status: Mutex::new(None),
                busy: AtomicBool::new(false),
                commands: AtomicU64::new(0),
                reaping: Mutex::new(false),
                sync_flush: AtomicBool::new(false),
            }),
        }
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The shell's name ("zsh", "pwsh").
    pub fn title(&self) -> String {
        shell::title(&self.argv)
    }

    /// The screen, locked. Keep the guard short: the reader thread waits for it.
    pub fn lock(&self) -> MutexGuard<'_, Screen> {
        self.inner
            .screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Spawn the shell in its folder (or home if that's gone) at the screen's size.
    pub fn start(&self) -> Result<(), StartError> {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return Err(StartError::AlreadyStarted);
        }
        // PowerShell (on any platform) reports its folder only with the prompt hook.
        let argv = shell::with_integration(&self.argv);
        if argv.is_empty() {
            return Err(StartError::NoCommand);
        }
        let (columns, lines) = {
            let screen = self.lock();
            (screen.columns(), screen.lines())
        };
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: lines.min(u16::MAX as usize) as u16,
                cols: columns.min(u16::MAX as usize) as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(pty)?;
        let mut cmd = CommandBuilder::from_argv(argv.iter().map(Into::into).collect());
        let cwd = if self.cwd.is_dir() {
            self.cwd.clone()
        } else {
            home_dir()
        };
        cmd.cwd(cwd);
        for (key, value) in shell::environment_overrides(|k| std::env::var_os(k)) {
            cmd.env(key, value);
        }
        let mut child = pair.slave.spawn_command(cmd).map_err(pty)?;
        drop(pair.slave); // only the child keeps the terminal open: its exit ends our reads
        let reader = pair.master.try_clone_reader().map_err(pty);
        let writer = pair.master.take_writer().map_err(pty);
        let (reader, writer) = match (reader, writer) {
            (Ok(r), Ok(w)) => (r, w),
            (Err(e), _) | (_, Err(e)) => {
                let _ = child.kill();
                return Err(e);
            }
        };
        let (input, queue) = mpsc::channel::<Vec<u8>>();
        let title = self.title();
        *self.inner.process() = Some(Process {
            killer: child.clone_killer(),
            pid: child.process_id(),
            master: pair.master,
            input: input.clone(),
        });
        let spawned = thread::Builder::new()
            .name(format!("terminal {title} writer"))
            .spawn(move || {
                let mut writer = writer;
                for data in queue {
                    if writer
                        .write_all(&data)
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            });
        let inner = Arc::clone(&self.inner);
        let spawned = spawned.and_then(|_| {
            thread::Builder::new()
                .name(format!("terminal {title}"))
                .spawn(move || read_loop(inner, reader, child, input))
        });
        if let Err(e) = spawned {
            self.close();
            return Err(StartError::Pty(e.to_string()));
        }
        Ok(())
    }

    /// Whether the shell runs.
    pub fn alive(&self) -> bool {
        self.inner.process().is_some() && !self.inner.exited.load(Ordering::SeqCst)
    }

    /// The exit status once the shell ended (`None` while running or when killed by a signal).
    pub fn exit_status(&self) -> Option<i32> {
        *self
            .inner
            .exit_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn pid(&self) -> Option<u32> {
        self.inner.process().as_ref().and_then(|p| p.pid)
    }

    /// Enter presses so far: the window refreshes panels when a command finished.
    pub fn commands(&self) -> u64 {
        self.inner.commands.load(Ordering::Relaxed)
    }

    /// Where the shell is now: what it reported (OSC 7), else its process's directory.
    pub fn current_dir(&self) -> Option<PathBuf> {
        if let Some(path) = self.lock().reported_cwd() {
            return Some(path.to_path_buf());
        }
        let pid = self.pid()?;
        if !self.alive() {
            return None;
        }
        shell::process_cwd(pid)
    }

    /// Whether the shell itself is in the foreground (no program running),
    /// so typing a `cd` is safe.
    pub fn at_prompt(&self) -> bool {
        if !self.alive() {
            return false;
        }
        if cfg!(windows) {
            return !self.inner.busy.load(Ordering::SeqCst);
        }
        foreground_is_shell(self.inner.process().as_ref())
    }

    /// Type a cd into the shell (check [`Session::at_prompt`] first).
    pub fn change_dir(&self, folder: &Path) {
        self.write(&shell::cd_command(&self.argv, folder));
    }

    /// Process output of our own (a message) as if the program printed it.
    pub fn feed(&self, data: &[u8]) {
        let replies = {
            let mut screen = self.lock();
            screen.feed(data);
            schedule_sync_flush(&self.inner, &screen);
            screen.take_replies()
        };
        if !replies.is_empty() {
            self.inner.send(replies);
        }
        (self.inner.notify)(SessionEvent::Output);
    }

    /// Send keystrokes / pasted text. Counts Enter presses (see [`Session::commands`]).
    pub fn write(&self, text: &str) {
        self.send_input(text.as_bytes(), true);
    }

    /// Send what a key press produces (see [`crate::keys::encode_key`]);
    /// Enter counts as a command, like typed text.
    pub fn send_key(&self, data: &[u8]) {
        self.send_input(data, true);
    }

    /// Send raw bytes (mouse reports) without counting commands.
    pub fn write_bytes(&self, data: &[u8]) {
        self.send_input(data, false);
    }

    fn send_input(&self, data: &[u8], typed: bool) {
        if data.is_empty() {
            return;
        }
        if typed && data.contains(&b'\r') {
            self.inner.commands.fetch_add(1, Ordering::Relaxed);
            if cfg!(windows) {
                self.inner.busy.store(true, Ordering::SeqCst); // until the next prompt reports in
            }
        }
        self.inner.send(data.to_vec());
    }

    /// Resize the screen (reflowing) and the pty; sizes below 2 are raised to 2.
    pub fn resize(&self, columns: usize, lines: usize) {
        let (columns, lines) = (columns.max(2), lines.max(2));
        if !self.lock().resize(columns, lines) {
            return;
        }
        if let Some(p) = self.inner.process().as_ref() {
            let _ = p.master.resize(PtySize {
                rows: lines.min(u16::MAX as usize) as u16,
                cols: columns.min(u16::MAX as usize) as u16,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    /// End the shell: SIGHUP, like closing a terminal window, and SIGKILL a
    /// moment later if it ignored that (as Python's `terminate(force=True)`).
    /// Returns at once; [`SessionEvent::Exited`] follows from the reader.
    pub fn close(&self) {
        let process = self.inner.process().take();
        if let Some(mut p) = process
            && !self.inner.exited.load(Ordering::SeqCst)
        {
            let _ = p.killer.kill();
            #[cfg(unix)]
            if let Some(pid) = p.pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
                force_kill_later(Arc::clone(&self.inner), pid);
            }
        }
        // dropping the master (and the input sender) hangs up the terminal
    }
}

/// What the panel prints when a shell ended (status shown unless 0/unknown),
/// byte for byte as the Python version did.
pub fn ended_message(title: &str, status: Option<i32>) -> String {
    let status = match status {
        Some(code) if code != 0 => format!(" (exit status {code})"),
        _ => String::new(),
    };
    format!("\r\n\x1b[2m[{title} ended{status} \u{2014} press Enter for a new one]\x1b[0m\r\n")
}

/// What the panel prints when the shell couldn't be started.
pub fn start_failed_message(argv: &[String], error: &StartError) -> String {
    format!("Cannot start {}: {error}\r\n", argv.join(" "))
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("argv", &self.argv)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

/// SIGCONT (a stopped shell can't act on SIGHUP), then SIGKILL after a grace
/// period unless the shell hung up by then. The pid is only signalled while
/// the reader hasn't started reaping it, so it can't hit a recycled pid.
#[cfg(unix)]
fn force_kill_later(inner: Arc<Inner>, pid: libc::pid_t) {
    // SAFETY: plain signal delivery to our own, not yet reaped child.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    let _ = thread::Builder::new()
        .name("terminal hangup".into())
        .spawn(move || {
            thread::sleep(KILL_GRACE);
            let reaping = inner.reaping.lock().unwrap_or_else(PoisonError::into_inner);
            if !*reaping {
                // SAFETY: as above; `reaping` is held, so the reader can't reap meanwhile.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        });
}

/// How long a closed shell gets to exit on SIGHUP before it is killed.
#[cfg(unix)]
const KILL_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// Output held back by a synchronized update (`CSI ? 2026 h`) must show once
/// its timeout passes even if the program never ends it and prints nothing
/// more. The reader is blocked in `read` then, so a short-lived thread waits
/// for the deadline (one at a time; called with the screen locked).
fn schedule_sync_flush(inner: &Arc<Inner>, screen: &Screen) {
    let Some(mut deadline) = screen.sync_deadline() else {
        return;
    };
    if inner.sync_flush.swap(true, Ordering::SeqCst) {
        return; // the waiting thread picks up later deadlines itself
    }
    let waiter = Arc::clone(inner);
    let spawned = thread::Builder::new()
        .name("terminal sync".into())
        .spawn(move || {
            let inner = waiter;
            loop {
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
                let (flushed, replies, done) = {
                    let mut screen = inner.screen.lock().unwrap_or_else(PoisonError::into_inner);
                    let flushed = screen.flush_sync();
                    let replies = screen.take_replies();
                    let next = screen.sync_deadline();
                    if let Some(next) = next {
                        deadline = next;
                    } else {
                        // cleared under the lock: the reader schedules the next one
                        inner.sync_flush.store(false, Ordering::SeqCst);
                    }
                    (flushed, replies, next.is_none())
                };
                if !replies.is_empty() {
                    inner.send(replies);
                }
                if flushed {
                    (inner.notify)(SessionEvent::Output);
                }
                if done {
                    return;
                }
            }
        });
    if spawned.is_err() {
        inner.sync_flush.store(false, Ordering::SeqCst); // the UI's flush_sync still works
    }
}

#[cfg(unix)]
fn foreground_is_shell(process: Option<&Process>) -> bool {
    let Some(p) = process else { return false };
    match (p.master.process_group_leader(), p.pid) {
        (Some(leader), Some(pid)) => leader as i64 == pid as i64,
        _ => false,
    }
}

#[cfg(not(unix))]
fn foreground_is_shell(_process: Option<&Process>) -> bool {
    false
}

fn read_loop(
    inner: Arc<Inner>,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    input: Sender<Vec<u8>>,
) {
    let mut buf = vec![0u8; 65536];
    let mut prompts = 0;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break, // EIO: the terminal was hung up
        };
        let (replies, seen) = {
            let mut screen = inner.screen.lock().unwrap_or_else(PoisonError::into_inner);
            screen.feed(&buf[..n]);
            schedule_sync_flush(&inner, &screen);
            (screen.take_replies(), screen.prompts())
        };
        if seen != prompts {
            prompts = seen;
            inner.busy.store(false, Ordering::SeqCst); // a prompt was drawn
        }
        if !replies.is_empty() {
            let _ = input.send(replies);
        }
        (inner.notify)(SessionEvent::Output);
    }
    drop(input);
    // From here on the pid may be reaped and reused: close() must not kill it.
    *inner.reaping.lock().unwrap_or_else(PoisonError::into_inner) = true;
    let status = child
        .wait()
        .ok()
        .and_then(|s| s.signal().is_none().then(|| s.exit_code() as i32));
    *inner
        .exit_status
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = status;
    inner.exited.store(true, Ordering::SeqCst);
    (inner.notify)(SessionEvent::Exited(status));
}

fn home_dir() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// portable-pty reports errors as `anyhow::Error`; we only need their text.
fn pty(e: impl std::fmt::Display) -> StartError {
    StartError::Pty(e.to_string())
}
