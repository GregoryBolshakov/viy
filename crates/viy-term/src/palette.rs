//! The terminal's default colours, asked once at start (OSC 10 and 11, as
//! Codex's `terminal_probe` does), for art that blends into whatever the
//! terminal shows: the header's ghost (`header.rs`).
//!
//! The question goes out right after raw mode is on and before the chat
//! reads any input: OSC 10 (foreground), OSC 11 (background), then DA1,
//! which every terminal answers, so a terminal that ignores the colour
//! queries costs no wait. The answer is read for at most 250 ms. A key
//! typed in that window is lost (Codex replays them; this does not yet).

use std::sync::OnceLock;
use std::time::Duration;

pub type Rgb = (u8, u8, u8);

static COLORS: OnceLock<Option<DefaultColors>> = OnceLock::new();

/// What [`probe`] writes: OSC 10 and 11, each ended by ST, then DA1.
pub const QUESTION: &str = "\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c";

/// The terminal's default foreground and background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultColors {
    pub fg: Rgb,
    pub bg: Rgb,
}

/// What [`probe`] found, if it ran and the terminal answered.
pub fn default_colors() -> Option<DefaultColors> {
    COLORS.get().copied().flatten()
}

/// Asks the terminal once; later calls do nothing. Needs raw mode.
pub fn probe() {
    COLORS.get_or_init(|| query(Duration::from_millis(250)));
}

#[cfg(unix)]
fn query(timeout: Duration) -> Option<DefaultColors> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    let mut out = std::io::stdout();
    out.write_all(QUESTION.as_bytes()).ok()?;
    out.flush().ok()?;
    let fd = std::io::stdin().as_raw_fd();
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    while !answered(&buf) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = i32::try_from(left.as_millis()).unwrap_or(i32::MAX).max(1);
        // SAFETY: one valid pollfd, its count 1.
        if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
            break;
        }
        let mut chunk = [0u8; 512];
        // SAFETY: reads at most chunk.len() bytes into chunk; poll said the
        // descriptor is readable, so this does not block.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        let Ok(n) = usize::try_from(n) else { break };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(DefaultColors {
        fg: osc_color(&buf, b"10")?,
        bg: osc_color(&buf, b"11")?,
    })
}

#[cfg(not(unix))]
fn query(_timeout: Duration) -> Option<DefaultColors> {
    None
}

/// The DA1 answer (`ESC [ ? … c`) is in: everything before it is too.
fn answered(buf: &[u8]) -> bool {
    buf.windows(3).enumerate().any(|(i, w)| {
        w == b"\x1b[?"
            && buf[i + 3..]
                .iter()
                .find(|b| !(b.is_ascii_digit() || **b == b';'))
                .is_some_and(|&b| b == b'c')
    })
}

/// The colour in the answer to OSC `slot`: `ESC ] slot ; rgb:R/G/B`, each
/// component 1-4 hex digits, ended by BEL or ST.
pub(crate) fn osc_color(buf: &[u8], slot: &[u8]) -> Option<Rgb> {
    let head = [b"\x1b]".as_slice(), slot, b";"].concat();
    let start = buf.windows(head.len()).position(|w| w == head)? + head.len();
    let rest = &buf[start..];
    let end = rest.iter().position(|&b| b == 0x07 || b == 0x1b)?;
    let payload = std::str::from_utf8(&rest[..end]).ok()?;
    let mut parts = payload.strip_prefix("rgb:")?.split('/');
    let mut component = || -> Option<u8> {
        let hex = parts.next()?;
        if hex.is_empty() || hex.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        let max = (1u32 << (4 * hex.len())) - 1;
        u8::try_from((value * 255 + max / 2) / max).ok()
    };
    Some((component()?, component()?, component()?))
}

#[cfg(test)]
#[path = "palette_tests.rs"]
mod tests;
