//! What the pod functions tell the user as they go: rpctl's
//! `_say(msg, color)`, as a value the caller renders. The text is rpctl's,
//! with `viy` for `rp` where it names a command.

use std::fmt;

/// One line or paragraph for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub tone: Tone,
    pub text: String,
}

impl Notice {
    pub fn new(tone: Tone, text: impl Into<String>) -> Self {
        Self {
            tone,
            text: text.into(),
        }
    }
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// rpctl's colours: `C_DIM`, `C_OK`, `C_WARN`, `C_ERR`, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    /// Detail and hints.
    Dim,
    /// Something worked.
    Ok,
    Warn,
    Error,
}

/// Where notices go.
pub type Say<'a> = &'a mut (dyn FnMut(Notice) + Send);

/// A desktop notification for someone waiting on a billing pod in another
/// window (rpctl's `notify`), sent by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub title: String,
    pub body: String,
    pub urgent: bool,
}

/// Everything a long pod function reports while it runs, in order: what
/// it says, the clock of a wait, a desktop alert. A terminal prints them;
/// the chat turns them into events, so its `/up` can run in the
/// background (kb d041).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Notice(Notice),
    Clock(Clock),
    Alert(Alert),
}

/// Where reports go. Shared, not `&mut`: a wait reports its clock while
/// its probe reports too.
pub type Reporter<'a> = &'a (dyn Fn(Report) + Send + Sync);

/// A wait's clock, once a second: rpctl's `  MM:SS label (state)` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clock {
    /// Whole seconds since the wait began.
    pub elapsed: u64,
    pub label: &'static str,
    /// What the wait is waiting on.
    pub state: String,
}

impl fmt::Display for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (m, s) = (self.elapsed / 60, self.elapsed % 60);
        write!(f, "  {m:02}:{s:02} {} ({})", self.label, self.state)
    }
}

/// How a terminal shows the clock, as rpctl prints it: on a tty one line
/// redrawn every second; elsewhere a line when the wait or its state
/// changes, and every 30 s.
#[derive(Debug, Default)]
pub struct ClockPrinter {
    tty: bool,
    /// A redrawn line is on screen, unfinished.
    open: bool,
    /// The wait and state last printed, and when.
    shown: Option<(&'static str, String, u64)>,
}

impl ClockPrinter {
    pub fn new(tty: bool) -> Self {
        Self {
            tty,
            ..Self::default()
        }
    }

    /// What to write for `clock`, if anything.
    pub fn clock(&mut self, clock: &Clock) -> Option<String> {
        if self.tty {
            self.open = true;
            return Some(format!("\r{clock}   \x1b[K"));
        }
        let quiet = self.shown.as_ref().is_some_and(|(label, state, at)| {
            *label == clock.label && *state == clock.state && clock.elapsed < at + 30
        });
        if quiet {
            return None;
        }
        self.shown = Some((clock.label, clock.state.clone(), clock.elapsed));
        Some(format!("{clock}\n"))
    }

    /// What to write before anything else: the end of a redrawn line.
    pub fn close(&mut self) -> Option<&'static str> {
        std::mem::take(&mut self.open).then_some("\n")
    }
}
