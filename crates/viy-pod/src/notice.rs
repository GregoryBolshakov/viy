//! What the pod functions tell the user as they go: rpctl's
//! `_say(msg, color)`, as a value the caller renders. The text is rpctl's,
//! with `viy` for `rp` where it names a command.

use std::fmt;
use std::pin::Pin;

use viy_protocol::Answer;
use viy_protocol::Question;

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
/// it says, the clock of a wait, a passing status line, a desktop alert.
/// A terminal prints them;
/// the chat turns them into events, so its `/up` can run in the
/// background (kb d041).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Notice(Notice),
    Clock(Clock),
    /// A line that stands only until the next report, such as `down`'s
    /// countdown; a terminal redraws it in place, then erases it.
    Passing(String),
    Alert(Alert),
}

/// Where reports go. Shared, not `&mut`: a wait reports its clock while
/// its probe reports too.
pub type Reporter<'a> = &'a (dyn Fn(Report) + Send + Sync);

/// Reports a notice.
pub(crate) fn say(report: Reporter<'_>, tone: Tone, text: impl Into<String>) {
    report(Report::Notice(Notice::new(tone, text)));
}

/// A question on its way to the user: resolves to the answer.
pub type Asking = Pin<Box<dyn Future<Output = Answer> + Send>>;

/// Who answers a pod command's questions (rpctl's `pod._ask`): a terminal,
/// the chat's question box, a test. `None` when nobody is there to ask,
/// and the command takes the question's `unattended` answer itself. As
/// Codex's `request_user_input`, the answer is awaited, not blocked on;
/// WP 3.1 turns it into a request event answered by an `Op` (kb d036).
pub type Asker<'a> = &'a (dyn Fn(&Question) -> Option<Asking> + Send + Sync);

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

/// How a terminal shows the clock and passing lines, as rpctl prints
/// them: on a tty the clock is one line redrawn every second, which the
/// next notice starts below, and a passing line is redrawn in place and
/// erased; elsewhere the clock is a line when the wait or its state
/// changes and every 30 s, and passing lines are left out.
#[derive(Debug, Default)]
pub struct ClockPrinter {
    tty: bool,
    /// What ends the redrawn line on screen, if one is.
    open: Option<&'static str>,
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
            self.open = Some("\n");
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

    /// What to write for a passing line, if anything.
    pub fn passing(&mut self, text: &str) -> Option<String> {
        if !self.tty {
            return None;
        }
        // A clock on screen stays, with its last state.
        let after_clock = if self.open == Some("\n") { "\n" } else { "" };
        self.open = Some("\r\x1b[K");
        Some(format!("{after_clock}\r{text}\x1b[K"))
    }

    /// What to write before anything else: the end of a redrawn line.
    pub fn close(&mut self) -> Option<&'static str> {
        self.open.take()
    }
}

/// Sends `alert` to the desktop with `notify-send`, in its own session and
/// not waited on, so it never delays what raised it. Best effort; the
/// caller checks there is a desktop to notify (`DISPLAY` or
/// `WAYLAND_DISPLAY`).
pub fn desktop_notify(exec: &dyn viy_exec::ExecBackend, alert: &Alert) {
    let Ok(null) = std::fs::File::create("/dev/null") else {
        return;
    };
    let urgency = if alert.urgent { "critical" } else { "normal" };
    let command = viy_exec::Command::new("notify-send")
        .args(["-a", "viy", "-u", urgency])
        .arg(&alert.title)
        .arg(&alert.body);
    let _ = exec.spawn(command, viy_exec::Spawn::Detached { stderr: null });
}
