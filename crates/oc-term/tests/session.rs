//! Real shells in real ptys (POSIX only; ConPTY on Windows isn't covered here).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oc_term::{Button, MouseAction, MouseInput, PointerEvent, Session, SessionEvent};

struct Sh {
    session: Session,
    exited: mpsc::Receiver<Option<i32>>,
    outputs: Arc<Mutex<usize>>,
    _dir: tempfile::TempDir,
    dir: PathBuf,
}

fn sh_in(argv: &[&str], columns: usize, lines: usize) -> Sh {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    let (tx, exited) = mpsc::channel();
    let tx = Mutex::new(tx);
    let outputs = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&outputs);
    let notify = Arc::new(move |event: SessionEvent| match event {
        SessionEvent::Output => *counter.lock().unwrap() += 1,
        SessionEvent::Exited(status) => {
            let _ = tx.lock().unwrap().send(status);
        }
    });
    let argv = argv.iter().map(|s| s.to_string()).collect();
    let session = Session::new(argv, &path, columns, lines, notify);
    session.start().unwrap();
    Sh {
        session,
        exited,
        outputs,
        _dir: dir,
        dir: path,
    }
}

fn sh() -> Sh {
    let s = sh_in(&["/bin/sh"], 60, 12);
    // Type only at the prompt: input written before the shell is ready is
    // echoed by the tty first, and the prompt then lands in front of the
    // command's first output line ("$ line0").
    eventually("prompt", || !screen_text(&s.session).trim().is_empty());
    s
}

fn screen_text(session: &Session) -> String {
    session.lock().text()
}

fn eventually(what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "condition not met in time: {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run(session: &Session, command: &str) {
    session.write(&format!("{command}\r"));
}

#[test]
fn echo_hi_shows_on_screen() {
    let s = sh_in(&["/bin/sh", "-c", "echo hi"], 40, 5);
    assert_eq!(
        s.exited.recv_timeout(Duration::from_secs(10)).unwrap(),
        Some(0)
    );
    assert!(screen_text(&s.session).lines().any(|l| l == "hi"));
    assert!(*s.outputs.lock().unwrap() > 0);
    assert!(!s.session.alive());
}

#[test]
fn shell_runs_in_folder_and_resizes() {
    let s = sh();
    run(&s.session, "pwd; echo sum=$((40+2))");
    eventually("sum", || screen_text(&s.session).contains("sum=42"));
    let text = screen_text(&s.session).replace('\n', "");
    assert!(text.contains(s.dir.to_str().unwrap()), "{text}");
    s.session.resize(40, 8);
    run(&s.session, "tput cols; tput lines");
    eventually("tput", || screen_text(&s.session).contains("\n40\n8"));
}

#[test]
fn environment_is_set() {
    let s = sh();
    run(&s.session, "echo \"T=$TERM C=$COLORTERM P=$TERM_PROGRAM\"");
    eventually("env", || {
        screen_text(&s.session).contains("T=xterm-256color C=truecolor P=OrchardCommander")
    });
}

#[test]
fn alternate_screen_restores_shell_output() {
    let s = sh();
    run(&s.session, "echo before");
    eventually("before", || screen_text(&s.session).contains("before"));
    run(
        &s.session,
        r"printf '\033[?1049h'; echo FULL$((1+1)); sleep 0.3; printf '\033[?1049l'; echo af$((0))ter",
    );
    eventually("after", || screen_text(&s.session).contains("af0ter"));
    let text = screen_text(&s.session);
    assert!(!text.contains("FULL2") && text.contains("before")); // like vim / less
}

#[test]
fn scrollback() {
    let s = sh();
    run(
        &s.session,
        "i=0; while [ $i -lt 40 ]; do echo line$i; i=$((i+1)); done",
    );
    eventually("line39", || screen_text(&s.session).contains("line39"));
    let screen = s.session.lock();
    assert!(screen.history_text().any(|h| h.starts_with("line0")));
}

#[test]
fn exit_status_and_commands() {
    let s = sh();
    assert!(s.session.alive());
    run(&s.session, "exit 3");
    assert_eq!(
        s.exited.recv_timeout(Duration::from_secs(10)).unwrap(),
        Some(3)
    );
    assert!(!s.session.alive());
    assert_eq!(s.session.exit_status(), Some(3));
    assert_eq!(s.session.commands(), 1);
    assert!(!s.session.at_prompt());
}

#[test]
fn shell_answers_cursor_queries_through_the_pty() {
    // The program asks where the cursor is and prints what it got back.
    let s = sh();
    run(
        &s.session,
        r#"stty -echo -icanon; printf 'ab\033[6n'; r=$(dd bs=1 count=6 2>/dev/null | od -An -c | tr -d ' \n'); stty sane; echo; echo "R=$r""#,
    );
    eventually("reply", || screen_text(&s.session).contains("R=033["));
}

#[test]
fn current_dir_follows_cd_and_at_prompt() {
    let s = sh();
    std::fs::create_dir(s.dir.join("sub")).unwrap();
    eventually("prompt", || s.session.at_prompt());
    let canon = |p: Option<PathBuf>| p.and_then(|p| p.canonicalize().ok());
    eventually("start dir", || {
        canon(s.session.current_dir()) == Some(s.dir.clone())
    });
    run(&s.session, "cd sub");
    eventually("cd", || {
        canon(s.session.current_dir()) == Some(s.dir.join("sub"))
    });
    run(&s.session, "sleep 1.5; echo SLEPT");
    eventually("busy", || !s.session.at_prompt()); // a program runs: no typing
    eventually("done", || screen_text(&s.session).contains("SLEPT"));
    eventually("back at prompt", || s.session.at_prompt());
    s.session.change_dir(&s.dir);
    eventually("change_dir", || {
        canon(s.session.current_dir()) == Some(s.dir.clone())
    });
}

#[test]
fn osc7_wins_over_the_process_directory() {
    let s = sh();
    run(
        &s.session,
        r"printf '\033]7;file://localhost/reported/dir\007'",
    );
    eventually("osc7", || {
        s.session.current_dir() == Some(PathBuf::from("/reported/dir"))
    });
}

#[test]
fn close_ends_the_shell() {
    let s = sh();
    s.session.close();
    assert!(!s.session.alive());
    let status = s.exited.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_ne!(status, Some(0)); // hung up
    s.session.write("ignored\r"); // no process: nothing happens
}

#[test]
fn missing_folder_starts_at_home_and_bad_shell_fails() {
    let notify = Arc::new(|_| {});
    let s = Session::new(
        vec!["/bin/sh".into()],
        "/definitely/not/here",
        40,
        5,
        notify.clone(),
    );
    s.start().unwrap();
    assert!(s.start().is_err()); // only once
    s.close();
    let bad = Session::new(vec!["/no/such/shell".into()], Path::new("/"), 40, 5, notify);
    let err = bad.start().unwrap_err();
    bad.feed(format!("Can't start /no/such/shell: {err}\r\n").as_bytes());
    assert!(bad.lock().text().starts_with("Can't start"));
    assert!(!bad.alive());
}

#[test]
fn close_kills_a_shell_that_ignores_hangups() {
    // Python escalated to SIGKILL (terminate(force=True)); a HUP-proof shell
    // must not outlive its panel (nor keep the reader thread waiting).
    let s = sh_in(
        &[
            "/bin/sh",
            "-c",
            "trap '' HUP; echo re''ady; while :; do sleep 0.2; done",
        ],
        40,
        5,
    );
    eventually("ready", || screen_text(&s.session).contains("ready"));
    s.session.close();
    let status = s.exited.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(status, None); // killed by a signal
}

#[test]
fn a_real_program_receives_clicks() {
    // End to end: a program in the pty turns on SGR mouse mode and reads a click.
    let s = sh();
    run(
        &s.session,
        r#"stty raw -echo; printf '\033[?1000h\033[?1006hRE''ADY\r\n'; r=$(dd bs=1 count=20 2>/dev/null | od -An -c | tr -d ' \n'); stty sane; echo; echo "G""OT $r""#,
    );
    eventually("ready", || screen_text(&s.session).contains("READY"));
    let mut mouse = MouseInput::new(8.0, 16.0);
    let at = PointerEvent::at(9.5 * 8.0, 4.5 * 16.0);
    let mut bytes = Vec::new();
    {
        let mut screen = s.session.lock();
        for action in [
            mouse.press(&mut screen, Button::Left, at),
            mouse.release(&mut screen, Button::Left, at),
        ] {
            if let MouseAction::Send(b) = action {
                bytes.extend(b);
            }
        }
    }
    assert_eq!(bytes, b"\x1b[<0;10;5M\x1b[<0;10;5m");
    s.session.write_bytes(&bytes);
    eventually("got", || {
        screen_text(&s.session).contains("GOT 033[<0;10;5M033[<0;10;5m")
    });
    assert_eq!(s.session.commands(), 1); // mouse reports aren't commands
}

#[test]
fn unfinished_synchronized_update_shows_by_itself() {
    // A program starts a synchronized update and goes quiet: what it drew
    // must still appear (vte's 150 ms timeout), without the UI polling.
    let s = sh();
    run(&s.session, r"printf '\033[?2026hSYN''CED'; sleep 5");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !screen_text(&s.session).contains("SYNCED") {
        assert!(Instant::now() < deadline, "held-back output never shown");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn panel_messages() {
    assert_eq!(
        oc_term::ended_message("sh", Some(3)),
        "\r\n\x1b[2m[sh ended (exit status 3) \u{2014} press Enter for a new one]\x1b[0m\r\n"
    );
    assert!(oc_term::ended_message("zsh", Some(0)).contains("[zsh ended \u{2014} press"));
    assert!(oc_term::ended_message("zsh", None).contains("[zsh ended \u{2014} press"));
    let err = oc_term::StartError::Pty("No such file or directory".into());
    assert_eq!(
        oc_term::start_failed_message(&["/no/sh".into(), "-l".into()], &err),
        "Can't start /no/sh -l: No such file or directory\r\n"
    );
}
