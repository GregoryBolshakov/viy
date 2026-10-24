//! Reading a terminal's screen in tests (ui-plan 13): a `vt100` emulator
//! whose [`Recorder`] notes every side effect beyond the cells (title,
//! clipboard, bells, reports, sequences it does not handle), and the
//! screen as text with the cursor marked, for assertions and for an agent
//! reading a failure.

/// The side effects the emulator saw, one line each.
#[derive(Debug, Default, Clone)]
pub struct Recorder {
    pub events: Vec<String>,
}

impl Recorder {
    /// The events, without viy's own that vt100 does not implement: the
    /// synchronized-output brackets around each frame, the underline
    /// colour's reset (SGR 59), the caret's blink and its end (SGR 5, 25),
    /// line wrap off and on (DECAWM, `?7`) and crossterm's urxvt mouse
    /// encoding (1015).
    pub fn foreign(&self) -> Vec<String> {
        const OWN: [&str; 9] = [
            "csi ?[[7]] l",
            "csi ?[[7]] h",
            "csi ?[[2026]] h",
            "csi ?[[2026]] l",
            "csi [[59]] m",
            "csi [[5]] m",
            "csi [[25]] m",
            "csi ?[[1015]] h",
            "csi ?[[1015]] l",
        ];
        self.events
            .iter()
            .filter(|e| !OWN.contains(&e.as_str()))
            .cloned()
            .collect()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

impl vt100::Callbacks for Recorder {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.events.push("bell".into());
    }

    fn visual_bell(&mut self, _: &mut vt100::Screen) {
        self.events.push("visual bell".into());
    }

    fn resize(&mut self, _: &mut vt100::Screen, request: (u16, u16)) {
        self.events.push(format!("resize {request:?}"));
    }

    fn set_window_icon_name(&mut self, _: &mut vt100::Screen, icon_name: &[u8]) {
        self.events.push(format!("icon name {:?}", text(icon_name)));
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.events.push(format!("title {:?}", text(title)));
    }

    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        self.events
            .push(format!("clipboard copy {:?} {:?}", text(ty), text(data)));
    }

    fn paste_from_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8]) {
        self.events.push(format!("clipboard paste {:?}", text(ty)));
    }

    fn unhandled_char(&mut self, _: &mut vt100::Screen, c: char) {
        self.events.push(format!("char {c:?}"));
    }

    fn unhandled_control(&mut self, _: &mut vt100::Screen, b: u8) {
        self.events.push(format!("control {b:#04x}"));
    }

    fn unhandled_escape(&mut self, _: &mut vt100::Screen, i1: Option<u8>, i2: Option<u8>, b: u8) {
        self.events
            .push(format!("escape {i1:?} {i2:?} {:?}", char::from(b)));
    }

    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let i1 = i1.map(char::from).map(String::from).unwrap_or_default();
        let i2 = i2.map(char::from).map(String::from).unwrap_or_default();
        self.events.push(format!("csi {i1}{i2}{params:?} {c}"));
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let params: Vec<String> = params.iter().map(|p| text(p)).collect();
        self.events.push(format!("osc {params:?}"));
    }
}

/// A parser of `rows` x `cols` that records side effects.
pub fn parser(rows: u16, cols: u16) -> vt100::Parser<Recorder> {
    vt100::Parser::new_with_callbacks(rows, cols, 0, Recorder::default())
}

/// vt100 does not print U+FFFD (it takes it for a decoding error) and
/// would shift the rest of the row; it is fed U+FFFC in its place, which
/// [`cell_text`] turns back.
const STAND_IN: [u8; 3] = *b"\xef\xbf\xbc";
const REPLACEMENT: [u8; 3] = *b"\xef\xbf\xbd";

/// Feeds bytes to a parser, U+FFFD as [`STAND_IN`], across writes: a
/// U+FFFD cut between two writes is held back until it is whole.
#[derive(Debug, Default)]
pub struct Feed {
    pending: Vec<u8>,
}

impl Feed {
    pub fn process(&mut self, parser: &mut vt100::Parser<Recorder>, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let keep = (1..REPLACEMENT.len())
            .rev()
            .find(|&n| data.ends_with(&REPLACEMENT[..n]))
            .unwrap_or(0);
        self.pending = data.split_off(data.len() - keep);
        let mut start = 0;
        let mut i = 0;
        while i + REPLACEMENT.len() <= data.len() {
            if data[i..].starts_with(&REPLACEMENT) {
                parser.process(&data[start..i]);
                parser.process(&STAND_IN);
                i += REPLACEMENT.len();
                start = i;
            } else {
                i += 1;
            }
        }
        parser.process(&data[start..]);
    }
}

/// Feeds a whole stream to `parser`, U+FFFD as [`STAND_IN`].
pub fn feed(parser: &mut vt100::Parser<Recorder>, bytes: &[u8]) {
    Feed::default().process(parser, bytes);
}

/// What a cell shows: a space when empty, U+FFFD for its stand-in.
pub fn cell_text(cell: &vt100::Cell) -> String {
    if !cell.has_contents() {
        return " ".into();
    }
    cell.contents().replace('\u{fffc}', "\u{fffd}")
}

/// The screen's rows without trailing blanks, `▮` at the cursor when it is
/// shown.
pub fn rows(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    (0..rows)
        .map(|row| {
            let mut text = String::new();
            for col in 0..cols {
                if row == cursor_row && col == cursor_col && !screen.hide_cursor() {
                    text.push('▮');
                    continue;
                }
                match screen.cell(row, col) {
                    Some(cell) if cell.is_wide_continuation() => {}
                    Some(cell) => text.push_str(&cell_text(cell)),
                    None => text.push(' '),
                }
            }
            text.trim_end_matches(' ').to_string()
        })
        .collect()
}

/// [`rows`] as one string, for a failure message.
pub fn dump(screen: &vt100::Screen) -> String {
    let rows = rows(screen);
    let width = usize::from(screen.size().1);
    let rule = "─".repeat(width);
    format!("┌{rule}\n│{}\n└{rule}", rows.join("\n│"))
}
