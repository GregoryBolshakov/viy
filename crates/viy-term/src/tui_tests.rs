//! The real terminal under a pseudo-terminal (Codex's `tui_panic_tests.rs`):
//! this test binary runs again as the child, takes the terminal with
//! [`Tui::init`] and draws, then panics, suspends or exits; the parent reads
//! what reached the terminal into a vt100 emulator and checks the pty's
//! line discipline after.

use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use super::*;

const CHILD: &str = "VIY_TUI_PTY_CHILD";
const SHELL_LINE: &str = "shell history before viy";

struct Pty {
    master: File,
    slave: File,
}

fn open_pty() -> Pty {
    let (mut master, mut slave) = (-1, -1);
    let size = libc::winsize {
        ws_row: 10,
        ws_col: 40,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty writes two descriptors and reads `size`; no name
    // buffer and no termios are passed.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(result, 0, "openpty: {}", io::Error::last_os_error());
    // SAFETY: both descriptors are new and owned by nobody else.
    unsafe {
        Pty {
            master: File::from_raw_fd(master),
            slave: File::from_raw_fd(slave),
        }
    }
}

/// The child in its own process group, so its SIGTSTP stops it alone.
fn spawn(pty: &Pty, mode: &str) -> Child {
    let mut slave = pty.slave.try_clone().unwrap();
    // What the shell printed before viy started.
    write!(slave, "{SHELL_LINE}\r\n").unwrap();
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tui::tests::pty_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, mode)
        .env_remove("TMUX")
        .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
        .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
        .stderr(Stdio::from(pty.slave.try_clone().unwrap()))
        .process_group(0)
        .spawn()
        .unwrap()
}

/// Everything the terminal receives, read by a thread.
fn record(pty: &Pty) -> Arc<Mutex<Vec<u8>>> {
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut master = pty.master.try_clone().unwrap();
    let sink = Arc::clone(&out);
    std::thread::spawn(move || {
        let mut buf = [0; 4096];
        // EIO once no slave is open; the parent keeps one, so this thread
        // ends with the test binary.
        while let Ok(n @ 1..) = master.read(&mut buf) {
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    out
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What the terminal shows once `bytes` reached it. Taller than the pty,
/// so the test harness's lines push nothing out of view.
fn emulate(bytes: &[u8]) -> vt100::Parser {
    let mut parser = vt100::Parser::new(40, 40, 0);
    parser.process(bytes);
    parser
}

/// The pty is back in the shell's mode: line editing and echo on.
fn cooked(slave: &File) -> bool {
    // SAFETY: an all-zero termios is valid storage for tcgetattr to fill.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: a live descriptor and a termios to write.
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut termios) },
        0
    );
    termios.c_lflag & (libc::ICANON | libc::ECHO) == libc::ICANON | libc::ECHO
}

fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle)
}

/// The shell's screen as it was: the line printed before viy, nothing of
/// what viy drew, every mode viy set undone.
fn assert_given_back(screen: &vt100::Screen) {
    let text = screen.contents();
    assert!(!screen.alternate_screen(), "{text}");
    assert!(text.contains(SHELL_LINE), "{text}");
    assert!(!text.contains("viy frame"), "{text}");
    assert!(!screen.hide_cursor());
    assert!(!screen.bracketed_paste());
    assert_eq!(screen.mouse_protocol_mode(), vt100::MouseProtocolMode::None);
}

/// The panic hook alone gives the terminal back (the child never drops its
/// `Tui`), and before the message, which lands on the shell's screen.
#[test]
fn a_panic_gives_the_terminal_back_before_its_message() {
    let pty = open_pty();
    let out = record(&pty);
    let status = spawn(&pty, "panic").wait().unwrap();
    assert!(!status.success());
    std::thread::sleep(Duration::from_millis(50));
    let bytes = out.lock().unwrap().clone();
    // The child did take the terminal and draw.
    assert!(contains(&bytes, b"\x1b[?1049h"));
    assert!(contains(&bytes, b"viy frame"));
    let parser = emulate(&bytes);
    assert_given_back(parser.screen());
    assert!(parser.screen().contents().contains("forced panic"));
    assert!(cooked(&pty.slave));
}

#[test]
fn a_normal_exit_gives_the_terminal_back() {
    let pty = open_pty();
    let out = record(&pty);
    let status = spawn(&pty, "exit").wait().unwrap();
    assert!(status.success());
    std::thread::sleep(Duration::from_millis(50));
    let bytes = out.lock().unwrap().clone();
    let parser = emulate(&bytes);
    assert_given_back(parser.screen());
    // The exit line is the one thing written to the shell's screen.
    assert!(parser.screen().contents().contains("the exit line"));
    assert!(cooked(&pty.slave));
}

#[test]
fn ctrl_z_gives_the_terminal_back_while_stopped_and_takes_it_again() {
    let pty = open_pty();
    let out = record(&pty);
    let mut child = spawn(&pty, "suspend");
    let pid = child.id();
    let state = || {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let after_name = stat.rfind(')').map_or("", |i| &stat[i + 1..]);
        after_name
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    };
    wait_for("the child to stop", || state() == "T");
    std::thread::sleep(Duration::from_millis(50));
    let stopped = out.lock().unwrap().len();
    {
        let bytes = out.lock().unwrap().clone();
        assert!(contains(&bytes, b"viy frame"));
        assert_given_back(emulate(&bytes).screen());
        assert!(cooked(&pty.slave));
    }
    // SAFETY: a plain signal to the child this test started.
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGCONT) }, 0);
    let status = child.wait().unwrap();
    assert!(status.success());
    std::thread::sleep(Duration::from_millis(50));
    let bytes = out.lock().unwrap().clone();
    let resumed = &bytes[stopped..];
    assert!(contains(resumed, b"\x1b[?1049h"), "{resumed:?}");
    assert!(contains(resumed, b"after resume"), "{resumed:?}");
    assert_given_back(emulate(&bytes).screen());
    assert!(cooked(&pty.slave));
}

/// The child of the tests above; does nothing unless one of them started it.
#[test]
#[ignore]
fn pty_child() {
    let Ok(mode) = std::env::var(CHILD) else {
        return;
    };
    let mut tui = Tui::init().unwrap();
    let draw = |tui: &mut Tui, text: &'static str| {
        tui.terminal()
            .draw(|frame| {
                frame.render_widget(Paragraph::new(text), Rect::new(0, 0, 20, 1));
                frame.set_cursor_position((2, 2));
            })
            .unwrap();
    };
    draw(&mut tui, "viy frame");
    match mode.as_str() {
        "panic" => {
            // Never dropped: only the panic hook can give the terminal back.
            let _held = Box::leak(Box::new(tui));
            panic!("forced panic");
        }
        "suspend" => {
            tui.suspend().unwrap();
            draw(&mut tui, "after resume");
        }
        _ => {}
    }
    drop(tui);
    println!("the exit line");
}
