//! The real terminal for the chat (docs/ui-plan.md 11, Codex's `tui.rs`):
//! raw mode, so Ctrl-C and Ctrl-Z are keys; the alternate screen, so the
//! shell's screen and history come back untouched (kb d038), with line wrap
//! off on it (see [`set_modes`]); bracketed
//! paste; mouse capture for the wheel unless tmux has `mouse off`; and,
//! when the chat sets the tab title, the shell's title saved on xterm's
//! title stack (rpctl `termtitle.py`).
//!
//! Every exit path gives the terminal back: [`restore`] runs from
//! [`Tui`]'s drop (a normal exit and an error), from the panic hook (before
//! the panic message, so it lands on the shell's screen), and around Ctrl-Z.
//! It undoes only the modes that are on, so a second call does nothing.
//! SIGTERM and SIGHUP close the chat through the app loop
//! ([`crate::app::run`]), which then drops the `Tui`.

use std::io;
use std::io::IsTerminal;
use std::io::Write;
use std::sync::Once;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crossterm::cursor::Show;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableMouseCapture;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::EnableMouseCapture;
use crossterm::execute;
use crossterm::terminal::DisableLineWrap;
use crossterm::terminal::EnableLineWrap;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Size;

use crate::tabtitle;
use crate::terminal::Terminal;
use crate::tmux;

/// The chat's backend: crossterm into [`ScreenWriter`].
pub type Backend = CrosstermBackend<ScreenWriter>;

/// The modes in force, so [`restore`] (from the panic hook too, which can't
/// reach the [`Tui`]) undoes exactly those.
static RAW: AtomicBool = AtomicBool::new(false);
static SCREEN: AtomicBool = AtomicBool::new(false);
static NOWRAP: AtomicBool = AtomicBool::new(false);
static PASTE: AtomicBool = AtomicBool::new(false);
static MOUSE: AtomicBool = AtomicBool::new(false);
static TITLE: AtomicBool = AtomicBool::new(false);

/// Enough for a whole frame of a large window, so each frame reaches the
/// terminal in one write: GNOME Terminal (VTE) may show a frame cut over
/// several writes half drawn, synchronized output or not (kb f056).
const FRAME_BUFFER: usize = 1 << 20;

/// The terminal, taken for the chat; given back on drop.
pub struct Tui {
    terminal: Terminal<Backend>,
    mouse: bool,
    /// The shell's title is saved, to be given back.
    title: bool,
}

impl Tui {
    /// Takes the terminal. Fails when stdin or stdout is not a terminal:
    /// the caller then uses the plain renderer (ui-plan 10).
    pub fn init() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::other("stdin or stdout is not a terminal"));
        }
        install_panic_hook();
        let mouse = tmux::mouse_capture(tmux::probe().as_deref());
        if let Err(e) = set_modes(mouse) {
            let _ = restore();
            return Err(e);
        }
        // Before anything reads input: the header's ghost blends into
        // the terminal's own colours.
        crate::palette::probe();
        match Terminal::new(CrosstermBackend::new(ScreenWriter::default())) {
            Ok(terminal) => Ok(Self {
                terminal,
                mouse,
                title: false,
            }),
            Err(e) => {
                let _ = restore();
                Err(e)
            }
        }
    }

    pub fn terminal(&mut self) -> &mut Terminal<Backend> {
        &mut self.terminal
    }

    /// Saves the shell's title, before the chat sets its own; every
    /// restore gives it back.
    pub fn save_title(&mut self) -> io::Result<()> {
        self.title = true;
        push_title()
    }

    /// Ctrl-Z: gives the shell its terminal, stops (SIGTSTP, as the
    /// terminal itself sends without raw mode), and takes it back on
    /// SIGCONT. The next draw rewrites every cell at the size the window
    /// has now; resizes while stopped were never seen.
    pub fn suspend(&mut self) -> io::Result<()> {
        restore()?;
        signal_hook::low_level::raise(signal_hook::consts::SIGTSTP)?;
        set_modes(self.mouse)?;
        if self.title {
            push_title()?;
        }
        let (width, height) = crossterm::terminal::size()?;
        self.terminal.resize(Size::new(width, height));
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = restore();
    }
}

fn set_modes(mouse: bool) -> io::Result<()> {
    let mut out = io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    RAW.store(true, Ordering::SeqCst);
    SCREEN.store(true, Ordering::SeqCst);
    execute!(out, EnterAlternateScreen)?;
    // Line wrap off (DECAWM): a frame laid out for a wider window can
    // still be on its way when the window narrows. With wrap on, its rows,
    // now longer than the screen, wrap onto the next and at the bottom
    // scroll the whole screen up, and GNOME Terminal's view stays scrolled
    // (seen 2026-07-20, kb d093). With it off a long row is only cut at the
    // edge, until the next frame rewrites it.
    NOWRAP.store(true, Ordering::SeqCst);
    execute!(out, DisableLineWrap)?;
    PASTE.store(true, Ordering::SeqCst);
    execute!(out, EnableBracketedPaste)?;
    if mouse {
        MOUSE.store(true, Ordering::SeqCst);
        execute!(out, EnableMouseCapture)?;
    }
    Ok(())
}

fn push_title() -> io::Result<()> {
    TITLE.store(true, Ordering::SeqCst);
    let mut out = io::stdout();
    out.write_all(tabtitle::PUSH.as_bytes())?;
    out.flush()
}

/// Gives the shell its terminal back: its title, mouse and paste modes
/// off, line wrap on, the main screen with the cursor shown, raw mode off. Each mode is undone only if
/// it is on, so this may run twice (the panic hook, then the drop). The
/// first error is returned after every step has been tried.
pub fn restore() -> io::Result<()> {
    let mut out = io::stdout();
    let mut result = Ok(());
    let mut step = |r: io::Result<()>| {
        if result.is_ok() {
            result = r;
        }
    };
    if TITLE.swap(false, Ordering::SeqCst) {
        step(
            out.write_all(tabtitle::POP.as_bytes())
                .and_then(|()| out.flush()),
        );
    }
    if MOUSE.swap(false, Ordering::SeqCst) {
        step(execute!(out, DisableMouseCapture));
    }
    if PASTE.swap(false, Ordering::SeqCst) {
        step(execute!(out, DisableBracketedPaste));
    }
    if NOWRAP.swap(false, Ordering::SeqCst) {
        step(execute!(out, EnableLineWrap));
    }
    if SCREEN.swap(false, Ordering::SeqCst) {
        step(execute!(out, LeaveAlternateScreen, Show));
    }
    if RAW.swap(false, Ordering::SeqCst) {
        step(crossterm::terminal::disable_raw_mode());
    }
    result
}

fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore();
            previous(info);
        }));
    });
}

/// Stdout, written once per flush; whatever is still buffered when the
/// alternate screen has been left is dropped, so a frame cut short by a
/// panic never reaches the shell's screen.
#[derive(Default)]
pub struct ScreenWriter {
    buffer: Vec<u8>,
}

impl Write for ScreenWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.buffer.capacity() == 0 {
            self.buffer.reserve(FRAME_BUFFER);
        }
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let frame = std::mem::take(&mut self.buffer);
        if !SCREEN.load(Ordering::SeqCst) || frame.is_empty() {
            return Ok(());
        }
        let mut out = io::stdout().lock();
        let result = out.write_all(&frame).and_then(|()| out.flush());
        self.buffer = frame;
        self.buffer.clear();
        result
    }
}

#[cfg(test)]
#[path = "tui_tests.rs"]
mod tests;
