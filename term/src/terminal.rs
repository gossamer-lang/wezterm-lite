use super::*;
use crate::terminalstate::performer::Performer;
use std::sync::Arc;
use wezterm_escape_parser::parser::Parser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum ClipboardSelection {
    Clipboard,
    PrimarySelection,
}

pub trait Clipboard: Send + Sync {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()>;
}

impl Clipboard for Box<dyn Clipboard> {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()> {
        self.as_ref().set_contents(selection, data)
    }
}

pub trait DeviceControlHandler: Send + Sync {
    fn handle_device_control(&mut self, _control: wezterm_escape_parser::DeviceControlMode);
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Progress {
    #[default]
    None,
    Percentage(u8),
    Error(u8),
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Alert {
    Bell,
    ToastNotification {
        /// The title text for the notification.
        title: Option<String>,
        /// The message body
        body: String,
        /// Whether clicking on the notification should focus the
        /// window/tab/pane that generated it
        focus: bool,
    },
    CurrentWorkingDirectoryChanged,
    IconTitleChanged(Option<String>),
    WindowTitleChanged(String),
    TabTitleChanged(Option<String>),
    /// When the color palette has been updated
    PaletteChanged,
    /// A UserVar has changed value
    SetUserVar {
        name: String,
        value: String,
    },
    /// When something bumps the seqno in the terminal model and
    /// the terminal is not focused
    OutputSinceFocusLost,
    /// A change to the progress bar state
    Progress(Progress),
}

pub trait AlertHandler: Send + Sync {
    fn alert(&mut self, alert: Alert);
}

pub trait DownloadHandler: Send + Sync {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>);
}

/// Represents an instance of a terminal emulator.
pub struct Terminal {
    /// The terminal model/state
    state: TerminalState,
    /// Baseline terminal escape sequence parser
    parser: Parser,
}

impl Deref for Terminal {
    type Target = TerminalState;

    fn deref(&self) -> &TerminalState {
        &self.state
    }
}

impl DerefMut for Terminal {
    fn deref_mut(&mut self) -> &mut TerminalState {
        &mut self.state
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, FromDynamic, ToDynamic)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct TerminalSize {
    pub rows: usize,
    pub cols: usize,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: u32,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        }
    }
}

/// The text `bytes` begins with that prints as it is: valid UTF-8 up to the
/// first control. A C1 control written as UTF-8 is a control to the parser,
/// and an invalid or unfinished character is the parser's to replace or
/// complete, so the run stops before either.
fn text_run(bytes: &[u8]) -> &str {
    let end = run_end(bytes);
    if end == 0 {
        return "";
    }
    let candidate = &bytes[..end];
    if candidate.is_ascii() {
        // SAFETY: every byte is ASCII, and ASCII is valid UTF-8.
        return unsafe { std::str::from_utf8_unchecked(candidate) };
    }
    match simdutf8::compat::from_utf8(candidate) {
        Ok(text) => text,
        Err(error) => std::str::from_utf8(&candidate[..error.valid_up_to()]).unwrap_or_default(),
    }
}

// Where the first C0 control, DEL or C1 control in UTF-8 (0xC2 followed by
// 0x80..=0x9F) starts, or the length of `bytes` without one.
fn run_end(bytes: &[u8]) -> usize {
    const ONES: u64 = u64::from_le_bytes([1; 8]);
    const HIGHS: u64 = u64::from_le_bytes([0x80; 8]);
    // The high bit of each byte of `word` below `n` (0x20 at most) is set.
    // The borrow out of such a byte can set the bits of bytes after it too,
    // so each byte flagged is checked.
    let below = |word: u64, n: u8| word.wrapping_sub(ONES * u64::from(n)) & !word & HIGHS;
    let holds = |word: u64, b: u8| below(word ^ (ONES * u64::from(b)), 1);
    let stops = |i: usize| {
        let b = bytes[i];
        b < 0x20
            || b == 0x7f
            || (b == 0xc2 && bytes.get(i + 1).is_some_and(|n| (0x80..=0x9f).contains(n)))
    };
    let (words, _) = bytes.as_chunks::<8>();
    for (n, word) in words.iter().enumerate() {
        let word = u64::from_le_bytes(*word);
        let mut flagged = below(word, 0x20) | holds(word, 0x7f) | holds(word, 0xc2);
        while flagged != 0 {
            let i = n * 8 + flagged.trailing_zeros() as usize / 8;
            if stops(i) {
                return i;
            }
            flagged &= flagged - 1;
        }
    }
    (words.len() * 8..bytes.len())
        .find(|&i| stops(i))
        .unwrap_or(bytes.len())
}

impl Terminal {
    /// Construct a new Terminal.
    /// `physical_rows` and `physical_cols` describe the dimensions
    /// of the visible portion of the terminal display in terms of
    /// the number of text cells.
    ///
    /// `pixel_width` and `pixel_height` describe the dimensions of
    /// that same visible area but in pixels.
    ///
    /// `term_program` and `term_version` are required to identify
    /// the host terminal program; they are used to respond to the
    /// terminal identification sequence `\033[>q`.
    ///
    /// `writer` is anything that implements `std::io::Write`; it
    /// is used to send input to the connected program; both keyboard
    /// and mouse input is encoded and written to that stream, as
    /// are answerback responses to a number of escape sequences.
    /// The terminal writes to it on the thread that feeds it output,
    /// so a writer that may block (a pty) belongs behind a queue.
    pub fn new(
        size: TerminalSize,
        config: Arc<dyn TerminalConfiguration + Send + Sync>,
        term_program: &str,
        term_version: &str,
        // writing to the writer sends data to input of the pty
        writer: Box<dyn std::io::Write + Send>,
    ) -> Terminal {
        Terminal {
            state: TerminalState::new(size, config, term_program, term_version, writer),
            parser: Parser::new(),
        }
    }

    /// Feed the terminal parser a slice of bytes from the output
    /// of the associated program.
    /// The slice is not required to be a complete sequence of escape
    /// characters; it is valid to feed in chunks of data as they arrive.
    /// The output is parsed and applied to the terminal model.
    pub fn advance_bytes<B: AsRef<[u8]>>(&mut self, bytes: B) {
        self.state.increment_seqno();
        {
            let mut rest = bytes.as_ref();

            let mut performer = Performer::new(&mut self.state);

            while !rest.is_empty() {
                // Text between sequences prints as it is, without a parser
                // action for each character.
                if self.parser.is_ground() {
                    let text = text_run(rest);
                    if !text.is_empty() {
                        performer.print_str(text);
                        rest = &rest[text.len()..];
                        continue;
                    }
                }
                // The parser takes a sequence or control whole, up to where
                // it is between sequences again.
                let taken = self
                    .parser
                    .parse_until_ground(rest, |action| performer.perform(action));
                rest = &rest[taken..];
            }
        }
        self.trigger_unseen_output_notif();
    }

    pub fn perform_actions(&mut self, actions: Vec<wezterm_escape_parser::Action>) {
        self.state.increment_seqno();
        {
            let mut performer = Performer::new(&mut self.state);
            for action in actions {
                performer.perform(action);
            }
        }
        self.trigger_unseen_output_notif();
    }
}

#[cfg(test)]
mod tests {
    use super::run_end;

    #[test]
    fn a_run_ends_at_the_first_control_wherever_it_falls_in_a_word() {
        let stops = |bytes: &[u8]| {
            (0..bytes.len())
                .find(|&i| {
                    let b = bytes[i];
                    b < 0x20
                        || b == 0x7f
                        || (b == 0xc2 && matches!(bytes.get(i + 1), Some(0x80..=0x9f)))
                })
                .unwrap_or(bytes.len())
        };
        let marks: [&[u8]; 9] = [
            b"\x00",
            b"\x1b",
            b"\x1f",
            b"\x7f",
            b"\xc2\x9b",
            b"\xc2\x80",
            b"\xc2\xa9",
            b"\xc3\xa9",
            b"\xc2",
        ];
        for len in 0..24 {
            for at in 0..=len {
                for mark in marks {
                    let mut bytes = vec![b'a'; len];
                    bytes.splice(at..at, mark.iter().copied());
                    assert_eq!(run_end(&bytes), stops(&bytes), "{:?}", bytes);
                }
            }
        }
    }
}
