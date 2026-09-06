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
