//! Holds a program on a pseudo-terminal for a person or an agent to drive
//! by hand: the live checks of the chat's UI (implementation plan WP 3.14).
//!
//!     cargo run -p viy-testkit --example ptyctl -- DIR ROWS COLS -- viy chat
//!
//! Lines written to the FIFO `DIR/in` are commands:
//!
//! - `keys TEXT`: TEXT sent as typed, with `\r` (Enter), `\n`, `\t`, `\e`
//!   (Esc), `\xHH` and `\\` unescaped;
//! - `paste FILE`: FILE's contents as one bracketed paste;
//! - `resize` is not offered: the size is fixed for the run.
//!
//! `DIR/screen.txt` is the screen, rewritten every 250 ms; `DIR/status` says `running`, then `exited CODE`
//! once the program has ended (ptyctl then exits too). Nothing here goes
//! anywhere but the program and these files.

use std::ffi::CString;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use viy_testkit::pty::Pty;
use viy_testkit::screen::dump;

fn main() {
    if let Err(e) = run() {
        eprintln!("ptyctl: {e}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    const USAGE: &str = "usage: ptyctl DIR ROWS COLS -- PROGRAM [ARGS...]";
    let args: Vec<String> = std::env::args().skip(1).collect();
    let split = args.iter().position(|a| a == "--").ok_or(USAGE)?;
    let [dir, rows, cols] = &args[..split] else {
        return Err(USAGE.into());
    };
    let program = args
        .get(split + 1..)
        .filter(|p| !p.is_empty())
        .ok_or(USAGE)?;
    let size = |n: &str| n.parse::<u16>().map_err(|_| format!("not a size: {n}"));
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let fifo = dir.join("in");
    let _ = std::fs::remove_file(&fifo);
    let path = CString::new(fifo.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: a valid NUL-terminated path.
    if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
        return Err(format!(
            "mkfifo {}: {}",
            fifo.display(),
            std::io::Error::last_os_error()
        ));
    }

    let pty = Arc::new(Pty::open(size(rows)?, size(cols)?).map_err(|e| format!("a pty: {e}"))?);
    let mut child = pty
        .spawn(Command::new(&program[0]).args(&program[1..]))
        .map_err(|e| format!("{}: {e}", program[0]))?;
    write(&dir.join("status"), "running\n");

    let commands = Arc::clone(&pty);
    let fifo_path = fifo.clone();
    std::thread::spawn(move || {
        loop {
            // Reopened at each EOF: every writer's lines are read.
            let Ok(file) = std::fs::File::open(&fifo_path) else {
                return;
            };
            for line in BufReader::new(file).lines() {
                let Ok(line) = line else { break };
                command(&commands, &line);
            }
        }
    });

    loop {
        write(&dir.join("screen.txt"), &screen(&pty));
        if let Ok(Some(status)) = child.try_wait() {
            std::thread::sleep(Duration::from_millis(300));
            write(&dir.join("screen.txt"), &screen(&pty));
            write(
                &dir.join("status"),
                &format!("exited {}\n", status.code().unwrap_or(-1)),
            );
            let _ = std::fs::remove_file(&fifo);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn screen(pty: &Pty) -> String {
    pty.with_screen(|p| format!("{}\n", dump(p.screen())))
}

fn write(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn command(pty: &Pty, line: &str) {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    match verb {
        "keys" => {
            let _ = pty.send(&unescape(rest));
        }
        "paste" => match std::fs::read(rest) {
            Ok(text) => {
                let _ = pty.send(b"\x1b[200~");
                for chunk in text.chunks(4096) {
                    let _ = pty.send(chunk);
                    std::thread::sleep(Duration::from_millis(2));
                }
                let _ = pty.send(b"\x1b[201~");
            }
            Err(e) => eprintln!("paste {rest}: {e}"),
        },
        _ => eprintln!("unknown command: {line}"),
    }
}

fn unescape(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1;
            match bytes[i] {
                b'r' => out.push(b'\r'),
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'e' => out.push(0x1b),
                b'x' if i + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                    out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                    i += 2;
                }
                other => out.push(other),
            }
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    out
}
