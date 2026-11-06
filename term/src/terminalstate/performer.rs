use crate::keys::KeyboardEncoding;
use crate::terminal::{Alert, Progress};
use crate::terminalstate::{
    default_color_map, CharSet, MouseEncoding, TabStop, UnicodeVersionStackEntry,
};
use crate::{ClipboardSelection, Position, TerminalState, VisibleRowIndex, DCS, ST};
use finl_unicode::grapheme_clusters::Graphemes;
use log::{debug, error};
use num_traits::FromPrimitive;
use ordered_float::NotNan;
use std::fmt::Write;
use std::io::Write as _;
use std::ops::{Deref, DerefMut};
use unicode_normalization::{is_nfc_quick, IsNormalized, UnicodeNormalization};
use wezterm_bidi::ParagraphDirectionHint;
use wezterm_cell::{
    char_column_width, grapheme_column_width, is_white_space_grapheme, text_char_column_width,
    Cell, CellAttributes, SemanticType, UnicodeVersion,
};
use wezterm_escape_parser::csi::{
    CharacterPath, EraseInDisplay, Keyboard, KittyKeyboardFlags, KittyKeyboardMode,
};
use wezterm_escape_parser::osc::{
    ChangeColorPair, ColorOrQuery, FinalTermSemanticPrompt, ITermProprietary,
    ITermUnicodeVersionOp, Selection,
};
use wezterm_escape_parser::{
    Action, ControlCode, DeviceControlMode, Esc, EscCode, OperatingSystemCommand, CSI,
};

/// A helper struct for implementing `vtparse::VTActor` while compartmentalizing
/// the terminal state and the embedding/host terminal interface
pub(crate) struct Performer<'a> {
    pub state: &'a mut TerminalState,
    print: String,
    /// The byte length and cell width of each grapheme in the run being
    /// placed, kept to spare an allocation per print.
    run: Vec<(usize, usize)>,
}

impl<'a> Deref for Performer<'a> {
    type Target = TerminalState;

    fn deref(&self) -> &TerminalState {
        self.state
    }
}

impl<'a> DerefMut for Performer<'a> {
    fn deref_mut(&mut self) -> &mut TerminalState {
        self.state
    }
}

impl<'a> Drop for Performer<'a> {
    fn drop(&mut self) {
        self.flush_print();
    }
}

/// The most codepoints one cell keeps: a longer cluster (a run of combining
/// marks) is cut here, so a cell's text stays bounded whatever a program sends.
const MAX_GRAPHEME_CHARS: usize = 32;

// Characters whose Grapheme_Cluster_Break neither joins them to a neighbor
// of this kind nor joins such a neighbor to them: two in a row always have a
// grapheme boundary between them. Hangul syllables join only conjoining
// jamo, which are not of this kind.
const STANDING_ALONE: [(u32, u32); 8] = [
    (0x20, 0x7e),
    (0xa0, 0x2ff),
    (0x370, 0x482),
    (0x48a, 0x52f),
    (0x3041, 0x3096),
    (0x30a1, 0x30ff),
    (0x4e00, 0x9fff),
    (0xac00, 0xd7a3),
];

// `STANDING_ALONE` as a bit a character, for the 16-bit characters it holds.
static STANDING_ALONE_BITS: [u64; 1024] = {
    let mut bits = [0u64; 1024];
    let mut range = 0;
    while range < STANDING_ALONE.len() {
        let (first, last) = STANDING_ALONE[range];
        let mut c = first;
        while c <= last {
            bits[(c >> 6) as usize] |= 1 << (c & 63);
            c += 1;
        }
        range += 1;
    }
    bits
};

fn stands_alone(c: char) -> bool {
    let c = c as u32;
    c < 0x10000 && STANDING_ALONE_BITS[(c >> 6) as usize] >> (c & 63) & 1 == 1
}

// The columns each 16-bit character standing alone takes as text, two bits
// a character: 0 for one that does not stand alone, 3 for one that takes no
// column.
struct AloneWidths([u8; 0x10000 / 4]);

impl AloneWidths {
    fn new(version: &UnicodeVersion) -> Box<Self> {
        let mut widths = Box::new(Self([0; 0x10000 / 4]));
        for (first, last) in STANDING_ALONE {
            for c in (first..=last).filter_map(char::from_u32) {
                let width = match text_char_column_width(c, version) {
                    0 => 3,
                    width => width as u8,
                };
                widths.0[c as usize >> 2] |= width << ((c as usize & 3) * 2);
            }
        }
        widths
    }

    fn get(&self, c: u32) -> u8 {
        match self.0.get((c >> 2) as usize) {
            Some(four) => four >> ((c & 3) * 2) & 3,
            None => 0,
        }
    }
}

// The widths table for `version`, when no per-character widths override it:
// its widths then follow from whether it is Unicode 9 or later and whether
// ambiguous characters are wide.
fn alone_widths(version: &UnicodeVersion) -> Option<&'static AloneWidths> {
    static TABLES: [std::sync::OnceLock<Box<AloneWidths>>; 4] =
        [const { std::sync::OnceLock::new() }; 4];
    if version.cell_widths.is_some() {
        return None;
    }
    let table = usize::from(version.version >= 9) << 1 | usize::from(version.ambiguous_are_wide);
    Some(TABLES[table].get_or_init(|| AloneWidths::new(version)))
}

// The character of `text` at byte `at`, a character boundary, and its length
// in bytes. Away from the end the length comes from the lead byte without a
// branch, as scripts of different lengths mix unpredictably.
#[inline(always)]
fn decode(text: &str, at: usize) -> Option<(u32, usize)> {
    let bytes = text.as_bytes();
    let Some(four) = bytes.get(at..at + 4) else {
        let c = text[at..].chars().next()?;
        return Some((c as u32, c.len_utf8()));
    };
    let word = u32::from_be_bytes([four[0], four[1], four[2], four[3]]);
    // The lead byte's leading ones: none for ASCII, else the length.
    let ones = (!word).leading_zeros();
    let len = ones.max(1);
    let lead = (word >> 24) & (0x7f >> ones);
    let all = lead << 18 | (word >> 16 & 0x3f) << 12 | (word >> 8 & 0x3f) << 6 | word & 0x3f;
    Some((all >> (6 * (4 - len)), len as usize))
}

// The grapheme `text` starts with, which must start at a grapheme boundary,
// and its character when it has only one. `ahead` carries whether the next
// character stands alone, when this call found out, to the call for it.
fn first_grapheme<'t>(text: &'t str, ahead: &mut Option<bool>) -> (&'t str, Option<char>) {
    let mut chars = text.chars();
    let Some(c) = chars.next() else {
        return (text, None);
    };
    let alone = ahead.take().unwrap_or_else(|| stands_alone(c));
    let single = &text[..c.len_utf8()];
    match chars.next() {
        None => (single, Some(c)),
        Some(next) if alone && stands_alone(next) => {
            *ahead = Some(true);
            (single, Some(c))
        }
        Some(_) => match Graphemes::new(text).next() {
            Some(g) if g.len() == single.len() => (g, Some(c)),
            Some(g) => (g, None),
            None => (text, None),
        },
    }
}

fn printable_ascii(b: u8) -> bool {
    (0x20..0x7f).contains(&b)
}

fn bounded_grapheme(g: &str) -> &str {
    // Each character takes a byte at least.
    if g.len() <= MAX_GRAPHEME_CHARS {
        return g;
    }
    match g.char_indices().nth(MAX_GRAPHEME_CHARS) {
        Some((end, _)) => &g[..end],
        None => g,
    }
}

impl<'a> Performer<'a> {
    pub fn new(state: &'a mut TerminalState) -> Self {
        Self {
            state,
            print: String::new(),
            run: Vec::new(),
        }
    }

    /// Apply character set related remapping to the input glyph if required
    // Whether the charset in use maps some characters to others.
    fn charset_remaps(&self) -> bool {
        let charset = if self.shift_out {
            self.g1_charset
        } else {
            self.g0_charset
        };
        matches!(charset, CharSet::DecLineDrawing | CharSet::Uk)
    }

    fn remap_grapheme<'b>(&self, g: &'b str) -> &'b str {
        if (self.shift_out && self.g1_charset == CharSet::DecLineDrawing)
            || (!self.shift_out && self.g0_charset == CharSet::DecLineDrawing)
        {
            match g {
                "`" => "◆",
                "a" => "▒",
                "b" => "␉",
                "c" => "␌",
                "d" => "␍",
                "e" => "␊",
                "f" => "°",
                "g" => "±",
                "h" => "␤",
                "i" => "␋",
                "j" => "┘",
                "k" => "┐",
                "l" => "┌",
                "m" => "└",
                "n" => "┼",
                "o" => "⎺",
                "p" => "⎻",
                "q" => "─",
                "r" => "⎼",
                "s" => "⎽",
                "t" => "├",
                "u" => "┤",
                "v" => "┴",
                "w" => "┬",
                "x" => "│",
                "y" => "≤",
                "z" => "≥",
                "{" => "π",
                "|" => "≠",
                "}" => "£",
                "~" => "·",
                _ => g,
            }
        } else if (self.shift_out && self.g1_charset == CharSet::Uk)
            || (!self.shift_out && self.g0_charset == CharSet::Uk)
        {
            match g {
                "#" => "£",
                _ => g,
            }
        } else {
            g
        }
    }

    /// Appends `marks` to the cell just printed, the one before the cursor,
    /// keeping the cell's width and attributes. False when there is none.
    fn extend_previous_cell(&mut self, marks: &str, seqno: wezterm_surface::SequenceNo) -> bool {
        let col = if self.wrap_next {
            self.cursor.x
        } else if self.cursor.x > 0 {
            self.cursor.x - 1
        } else {
            return false;
        };
        let y = self.cursor.y;
        let last_printed = self.last_printed;
        let screen = self.screen_mut();
        let phys = screen.phys_row(y);
        let line = screen.line_mut(phys);
        let Some((at, text, width, attrs)) = line
            .visible_cells()
            .find(|c| c.cell_index() <= col && col < c.cell_index() + c.width().max(1))
            .map(|c| {
                (
                    c.cell_index(),
                    c.str().to_string(),
                    c.width(),
                    c.attrs().clone(),
                )
            })
        else {
            return false;
        };
        // Marks extend the cell only when they join the character printed
        // before them, as they would in text printed whole.
        let before = match last_printed {
            Some(c) => String::from(c),
            None => text.clone(),
        };
        if Graphemes::new(&format!("{before}{marks}")).nth(1).is_some() {
            return false;
        }
        let joined = format!("{text}{marks}");
        line.set_cell_grapheme(at, bounded_grapheme(&joined), width, attrs, seqno);
        true
    }

    // Moves the cursor to the start of the next line, marking the line it
    // leaves as wrapped so a resize can reflow it.
    fn wrap_to_next_line(&mut self, seqno: wezterm_surface::SequenceNo) {
        // Since we're implicitly moving the cursor to the next
        // line, we need to tag the current position as wrapped
        // so that we can correctly reflow it if the window is
        // resized.
        {
            let y = self.cursor.y;
            let is_conpty = self.state.enable_conpty_quirks;
            let screen = self.screen_mut();
            let y = screen.phys_row(y);

            fn makes_sense_to_wrap(s: &str) -> bool {
                let len = s.len();
                match (len, s.chars().next()) {
                    (1, Some(c)) => c.is_alphanumeric() || c.is_ascii_punctuation(),
                    _ => true,
                }
            }

            let should_mark_wrapped = !is_conpty
                || screen
                    .line_mut(y)
                    .visible_cells()
                    .last()
                    .map(|cell| makes_sense_to_wrap(cell.str()))
                    .unwrap_or(false);
            if should_mark_wrapped {
                screen.line_mut(y).set_last_cell_was_wrapped(true, seqno);
            }
        }
        self.new_line(true);
    }

    // Prints `text`, printable ASCII with no character set remapping in
    // effect and insert mode off: every byte is one cell of its own, so runs
    // up to the right margin are set at once.
    fn print_ascii(&mut self, text: &str, seqno: wezterm_surface::SequenceNo) {
        let mut rest = text;
        while !rest.is_empty() {
            if self.wrap_next {
                self.wrap_to_next_line(seqno);
            }
            let x = self.cursor.x;
            let y = self.cursor.y;
            let width = self.left_and_right_margins.end;
            let room = width.saturating_sub(x).max(1);
            let (run, after) = rest.split_at(room.min(rest.len()));
            let pen = self.pen.clone();
            self.screen_mut().set_cells_ascii(x, y, run, pen, seqno);
            let end = x + run.len();
            if end < width {
                self.cursor.x = end;
                self.wrap_next = false;
            } else {
                // The last cell printed is at the margin, where the cursor stays.
                self.cursor.x = end - 1;
                self.wrap_next = self.dec_auto_wrap;
            }
            rest = after;
        }
    }

    fn flush_print(&mut self) {
        if self.print.is_empty() {
            return;
        }

        let seqno = self.seqno;
        let mut p = std::mem::take(&mut self.print);
        // Printable ASCII alone is a narrow cell a byte, with no character
        // that a neighbor could join.
        if !self.insert && self.ascii_prints_plainly() && p.bytes().all(printable_ascii) {
            self.print_ascii(&p, seqno);
        } else {
            self.print_graphemes(&p, seqno);
        }
        self.last_printed = p.chars().next_back();
        std::mem::swap(&mut self.print, &mut p);
        self.print.clear();
    }

    // Printable ASCII maps to itself, a cell each, in the current charset
    // and widths.
    fn ascii_prints_plainly(&self) -> bool {
        !self.charset_remaps()
            && self
                .unicode_version
                .cell_widths
                .as_ref()
                .is_none_or(|widths| (0x20..0x7f).all(|c| !widths.contains_key(&c)))
    }

    fn print_graphemes(&mut self, text: &str, seqno: wezterm_surface::SequenceNo) {
        if text.is_empty() {
            return;
        }
        let plain_ascii = self.ascii_prints_plainly();
        let remaps = self.charset_remaps();
        let normalized: String;
        let text = if self.config.normalize_output_to_unicode_nfc()
            && is_nfc_quick(text.chars()) != IsNormalized::Yes
        {
            normalized = text.nfc().collect();
            normalized.as_str()
        } else {
            text
        };

        // Graphemes that land on the cursor's row before its margin go to the
        // line together, as the span of `text` they cover; the others go one
        // at a time.
        let mut run_at = (0, 0);
        let mut span = 0..0;
        let mut at = 0;
        let mut ahead = None;
        while at < text.len() {
            // Printable ASCII followed by more of it is a cell a byte.
            if plain_ascii && !self.insert && !self.wrap_next {
                let bytes = &text.as_bytes()[at..];
                let room = self
                    .left_and_right_margins
                    .end
                    .saturating_sub(self.cursor.x + 1);
                let n = bytes
                    .windows(2)
                    .take(room)
                    .take_while(|pair| printable_ascii(pair[0]) && printable_ascii(pair[1]))
                    .count();
                if n > 0 {
                    ahead = None;
                    if span.end != at {
                        self.print_run(&text[span.clone()], run_at, seqno);
                    }
                    if self.run.is_empty() {
                        run_at = (self.cursor.x, self.cursor.y);
                        span = at..at;
                    }
                    self.run.extend(std::iter::repeat_n((1, 1), n));
                    at += n;
                    span.end = at;
                    self.cursor.x += n;
                    continue;
                }
            }
            // A character standing alone before another is a grapheme by
            // itself; a stretch of them joins the run here.
            if !remaps && !self.insert && !self.wrap_next {
                let took = match alone_widths(&self.unicode_version) {
                    Some(widths) => {
                        self.run_alone(text, &mut at, &mut span, &mut run_at, seqno, |c| {
                            widths.get(c)
                        })
                    }
                    None => {
                        let version = self.unicode_version.clone();
                        self.run_alone(text, &mut at, &mut span, &mut run_at, seqno, |c| {
                            match char::from_u32(c) {
                                Some(c) if stands_alone(c) => {
                                    match text_char_column_width(c, &version) {
                                        0 => 3,
                                        width => width as u8,
                                    }
                                }
                                _ => 0,
                            }
                        })
                    }
                };
                if took > 0 {
                    ahead = None;
                    self.cursor.x += took;
                    continue;
                }
            }
            let (whole, single) = first_grapheme(&text[at..], &mut ahead);
            let start = at;
            at += whole.len();
            let bounded = if single.is_some() {
                whole
            } else {
                bounded_grapheme(whole)
            };
            let g = if remaps {
                self.remap_grapheme(bounded)
            } else {
                bounded
            };

            let mut print_width = match single {
                Some(c) if std::ptr::eq(g, whole) => {
                    char_column_width(c, Some(&self.unicode_version))
                }
                _ => grapheme_column_width(g, Some(&self.unicode_version)),
            };
            // Output arrives in pieces, so a combining mark can come after its
            // base character was already printed; it joins that character.
            if print_width == 0
                && start == 0
                && !is_white_space_grapheme(g)
                && self.extend_previous_cell(g, seqno)
            {
                continue;
            }
            if print_width == 0 {
                // We got a zero-width grapheme.

                // Relevant reading:
                // <https://github.com/wezterm/wezterm/issues/1422>
                // <https://github.com/wezterm/wezterm/issues/6637>
                // <https://github.com/harfbuzz/harfbuzz/issues/4279>
                // <https://www.unicode.org/faq/unsup_char.html#2>
                //
                // For White_Space we want to ensure that we display as a space.
                // Other non-printing, zero-width characters can be elided
                // to avoid presentation problems, but may introduce potential
                // weirdness elsewhere. For example, U+2068 is a BIDI control
                // character and will be elided by this logic. A consequence
                // of that is that when the user copies the surrounding text
                // from the terminal, that BIDI control will not be present.
                // We do not currently have a solution for that.
                if is_white_space_grapheme(g) {
                    // Ensure that White_Space shows as a space
                    print_width = 1;
                } else {
                    log::trace!("Eliding zero-width grapheme {:?}", g);
                    continue;
                }
            }

            // Only a grapheme printed as it arrived is its own text in `text`.
            if !self.insert
                && !self.wrap_next
                && self.cursor.x + print_width < self.left_and_right_margins.end
                && std::ptr::eq(g, whole)
            {
                if span.end != start {
                    self.print_run(&text[span.clone()], run_at, seqno);
                }
                if self.run.is_empty() {
                    run_at = (self.cursor.x, self.cursor.y);
                    span = start..start;
                }
                self.run.push((g.len(), print_width));
                span.end = at;
                self.cursor.x += print_width;
                continue;
            }
            self.print_run(&text[span.clone()], run_at, seqno);

            if self.wrap_next {
                self.wrap_to_next_line(seqno);
            }

            let x = self.cursor.x;
            let y = self.cursor.y;
            let width = self.left_and_right_margins.end;

            let pen = self.pen.clone();

            let wrappable = x + print_width >= width;

            if self.insert {
                let margin = self.left_and_right_margins.end;
                let screen = self.screen_mut();
                for _ in x..x + print_width as usize {
                    screen.insert_cell(x, y, margin, seqno);
                }
            }

            // Assign the cell
            log::trace!(
                "print x={} y={} print_width={} width={} cell={} {:?}",
                x,
                y,
                print_width,
                width,
                g,
                self.pen
            );
            self.screen_mut()
                .set_cell_grapheme(x, y, g, print_width, pen, seqno);

            if !wrappable {
                self.cursor.x += print_width;
                self.wrap_next = false;
            } else {
                self.wrap_next = self.dec_auto_wrap;
            }
        }
        self.print_run(&text[span], run_at, seqno);
    }

    // Adds the characters standing alone from `at` on to the run while the
    // next one stands alone too and the row has room, answering the columns
    // they take. `widths` gives a character's columns as `AloneWidths` does.
    fn run_alone(
        &mut self,
        text: &str,
        at: &mut usize,
        span: &mut std::ops::Range<usize>,
        run_at: &mut (usize, VisibleRowIndex),
        seqno: wezterm_surface::SequenceNo,
        widths: impl Fn(u32) -> u8,
    ) -> usize {
        let end = self.left_and_right_margins.end;
        let mut took = 0;
        let Some((c, mut len)) = decode(text, *at) else {
            return 0;
        };
        let mut width = widths(c);
        while width != 0 {
            let Some((next, next_len)) = decode(text, *at + len) else {
                break;
            };
            let next_width = widths(next);
            // No character standing alone presents as an emoji.
            if width == 3 || next_width == 0 || self.cursor.x + took + usize::from(width) >= end {
                break;
            }
            if self.run.is_empty() || span.end != *at {
                self.print_run(&text[span.clone()], *run_at, seqno);
                *run_at = (self.cursor.x + took, self.cursor.y);
                *span = *at..*at;
            }
            self.run.push((len, usize::from(width)));
            *at += len;
            span.end = *at;
            took += usize::from(width);
            (len, width) = (next_len, next_width);
        }
        took
    }

    // Places the run, the graphemes of `text`, from `at` on in the pen, and
    // empties it.
    fn print_run(
        &mut self,
        text: &str,
        (x, y): (usize, VisibleRowIndex),
        seqno: wezterm_surface::SequenceNo,
    ) {
        if self.run.is_empty() {
            return;
        }
        let run = std::mem::take(&mut self.run);
        let pen = self.pen.clone();
        self.screen_mut()
            .set_cells_graphemes(x, y, text, &run, pen, seqno);
        self.run = run;
        self.run.clear();
    }

    /// ConPTY, at the time of writing, does something horrible to rewrite
    /// `ESC k TITLE ST` into something completely different and out-of-order,
    /// and critically, removes the ST.
    /// The result is that our hack to accumulate the tmux title gets stuck
    /// in a mode where all printable output is accumulated for the title.
    /// To combat this, we pop_tmux_title_state when we're obviously moving
    /// to different escape sequence parsing states.
    /// <https://github.com/wezterm/wezterm/issues/2442>
    fn pop_tmux_title_state(&mut self) {
        if let Some(title) = self.accumulating_title.take() {
            log::debug!("ST never received for pending tmux title escape sequence: {title:?}");
        }
    }

    pub fn perform(&mut self, action: Action) {
        debug!("perform {:?}", action);
        if self.suppress_initial_title_change {
            if let Action::OperatingSystemCommand(osc) = &action {
                if let OperatingSystemCommand::SetIconNameAndWindowTitle(_) = **osc {
                    debug!("suppressed {:?}", osc);
                    self.suppress_initial_title_change = false;
                    return;
                }
            }
        }
        match action {
            Action::Print(c) => self.print(c),
            Action::PrintString(s) => {
                for c in s.chars() {
                    self.print(c)
                }
            }
            Action::Control(code) => self.control(code),
            Action::DeviceControl(ctrl) => self.device_control(ctrl),
            Action::OperatingSystemCommand(osc) => self.osc_dispatch(*osc),
            Action::Esc(esc) => self.esc_dispatch(esc),
            Action::CSI(csi) => self.csi_dispatch(csi),
            #[cfg(feature = "images")]
            Action::Sixel(sixel) => self.sixel(&sixel),
            #[cfg(not(feature = "images"))]
            Action::Sixel(_) => {}
            Action::XtGetTcap(names) => self.xt_get_tcap(names),
            #[cfg(not(feature = "images"))]
            Action::KittyImage(_) => {}
            #[cfg(feature = "images")]
            Action::KittyImage(img) => {
                self.flush_print();
                if let Err(err) = self.kitty_img(*img) {
                    log::error!("kitty_img: {:#}", err);
                }
            }
        }
    }

    fn device_control(&mut self, ctrl: DeviceControlMode) {
        self.pop_tmux_title_state();
        match &ctrl {
            DeviceControlMode::ShortDeviceControl(s) => {
                match (s.byte, s.intermediates.as_slice()) {
                    (b'q', &[b'$']) => {
                        // DECRQSS - Request Status String
                        // https://vt100.net/docs/vt510-rm/DECRQSS.html
                        // The response is described here:
                        // https://vt100.net/docs/vt510-rm/DECRPSS.html
                        // but note that *that* text has the validity value
                        // inverted; there's a note about this in the xterm
                        // ctlseqs docs.
                        match *s.data.as_slice() {
                            [b'"', b'p'] => {
                                // DECSCL - select conformance level
                                write!(self.writer, "{}1$r65;1\"p{}", DCS, ST).ok();
                                self.writer.flush().ok();
                            }
                            [b'r'] => {
                                // DECSTBM - top and bottom margins
                                let margins = self.top_and_bottom_margins.clone();
                                write!(
                                    self.writer,
                                    "{}1$r{};{}r{}",
                                    DCS,
                                    margins.start + 1,
                                    margins.end,
                                    ST
                                )
                                .ok();
                                self.writer.flush().ok();
                            }
                            [b's'] => {
                                // DECSLRM - left and right margins
                                let margins = self.left_and_right_margins.clone();
                                write!(
                                    self.writer,
                                    "{}1$r{};{}s{}",
                                    DCS,
                                    margins.start + 1,
                                    margins.end,
                                    ST
                                )
                                .ok();
                                self.writer.flush().ok();
                            }
                            _ => {
                                if self.config.log_unknown_escape_sequences() {
                                    log::warn!("unhandled DECRQSS {:?}", s);
                                }
                                // Reply that the request is invalid
                                write!(self.writer, "{}0$r{}", DCS, ST).ok();
                                self.writer.flush().ok();
                            }
                        }
                    }
                    _ => {
                        if self.config.log_unknown_escape_sequences() {
                            log::warn!("unhandled {:?}", s);
                        }
                    }
                }
            }
            _ => match self.device_control_handler.as_mut() {
                Some(handler) => handler.handle_device_control(ctrl),
                None => {
                    if self.config.log_unknown_escape_sequences() {
                        log::warn!("unhandled {:?}", ctrl);
                    }
                }
            },
        }
    }

    /// Draw a character to the screen
    /// Prints `text` as `print` would each of its characters.
    pub fn print_str(&mut self, text: &str) {
        if let Some(title) = self.accumulating_title.as_mut() {
            title.push_str(text);
        } else {
            self.print.push_str(text);
        }
    }

    fn print(&mut self, c: char) {
        // We buffer up the chars to increase the chances of correctly grouping graphemes into cells
        if let Some(title) = self.accumulating_title.as_mut() {
            title.push(c);
        } else {
            self.print.push(c);
        }
    }

    fn control(&mut self, control: ControlCode) {
        let seqno = self.seqno;
        self.pop_tmux_title_state();
        self.flush_print();
        match control {
            ControlCode::LineFeed | ControlCode::VerticalTab | ControlCode::FormFeed => {
                if self.left_and_right_margins.contains(&self.cursor.x) {
                    self.new_line(false);
                } else {
                    // Do move down, but don't trigger a scroll when we're
                    // outside of the left/right margins
                    let old_y = self.cursor.y;
                    let y = if old_y == self.top_and_bottom_margins.end - 1 {
                        old_y
                    } else {
                        (old_y + 1).min(self.screen().physical_rows as i64 - 1)
                    };
                    self.screen_mut().dirty_line(old_y, seqno);
                    self.screen_mut().dirty_line(y, seqno);
                    self.cursor.y = y;
                    self.wrap_next = false;
                }
                if self.newline_mode {
                    self.cursor.x = 0;
                    self.clear_semantic_attribute_due_to_movement();
                }
            }
            ControlCode::CarriageReturn => {
                if self.cursor.x >= self.left_and_right_margins.start {
                    self.cursor.x = self.left_and_right_margins.start;
                } else {
                    self.cursor.x = 0;
                }
                let y = self.cursor.y;
                self.wrap_next = false;
                self.clear_semantic_attribute_due_to_movement();
                self.screen_mut().dirty_line(y, seqno);
            }

            ControlCode::Backspace => {
                if self.reverse_wraparound_mode
                    && self.dec_auto_wrap
                    && self.cursor.x == self.left_and_right_margins.start
                    && self.cursor.y == self.top_and_bottom_margins.start
                {
                    // Backspace off the top-left wraps around to the bottom right
                    let x_pos = Position::Absolute(self.left_and_right_margins.end as i64 - 1);
                    let y_pos = Position::Absolute(self.top_and_bottom_margins.end - 1);
                    self.set_cursor_pos(&x_pos, &y_pos);
                } else if self.reverse_wraparound_mode
                    && self.dec_auto_wrap
                    && self.cursor.x <= self.left_and_right_margins.start
                {
                    // Backspace off the left wraps around to the prior line on the right
                    let x_pos = Position::Absolute(self.left_and_right_margins.end as i64 - 1);
                    let y_pos = Position::Relative(-1);
                    self.set_cursor_pos(&x_pos, &y_pos);
                } else if self.reverse_wraparound_mode
                    && self.dec_auto_wrap
                    && self.cursor.x == self.left_and_right_margins.end - 1
                    && self.wrap_next
                {
                    // If the cursor is in the last column and a character was
                    // just output and reverse-wraparound is on then backspace
                    // by 1 cancels the pending wrap.
                    self.wrap_next = false;
                } else if self.cursor.x == self.left_and_right_margins.start {
                    // Respect the left margin and don't BS outside it
                } else {
                    self.set_cursor_pos(&Position::Relative(-1), &Position::Relative(0));
                }
            }
            ControlCode::HorizontalTab => self.c0_horizontal_tab(),
            ControlCode::HTS => self.c1_hts(),
            ControlCode::IND => self.c1_index(),
            ControlCode::NEL => self.c1_nel(),
            ControlCode::Bell => {
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::Bell);
                } else {
                    log::info!("Ding! (this is the bell)");
                }
            }
            ControlCode::RI => self.c1_reverse_index(),

            // wezterm only supports UTF-8, so does not support the
            // DEC National Replacement Character Sets.  However, it does
            // support the DEC Special Graphics character set used by
            // numerous ncurses applications.  DEC Special Graphics can be
            // selected by ASCII Shift Out (0x0E, ^N) or by setting G0
            // via ESC ( 0 .
            ControlCode::ShiftIn => {
                self.shift_out = false;
            }
            ControlCode::ShiftOut => {
                self.shift_out = true;
            }

            ControlCode::Enquiry => {
                let response = self.config.enq_answerback();
                if !response.is_empty() {
                    write!(self.writer, "{}", response).ok();
                    self.writer.flush().ok();
                }
            }

            ControlCode::Null => {}

            _ => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("unhandled ControlCode {:?}", control);
                }
            }
        }
    }

    fn csi_dispatch(&mut self, csi: CSI) {
        self.pop_tmux_title_state();
        self.flush_print();
        match csi {
            CSI::Sgr(sgr) => self.state.perform_csi_sgr(sgr),
            CSI::Cursor(wezterm_escape_parser::csi::Cursor::Left(n)) => {
                // We treat CUB (Cursor::Left) the same as Backspace as
                // that is what xterm does.
                // <https://github.com/wezterm/wezterm/issues/1273>
                for _ in 0..n {
                    self.control(ControlCode::Backspace);
                }
            }
            CSI::Cursor(cursor) => self.state.perform_csi_cursor(cursor),
            CSI::Edit(edit) => self.state.perform_csi_edit(edit),
            CSI::Mode(mode) => self.state.perform_csi_mode(mode),
            CSI::Device(dev) => self.state.perform_device(*dev),
            CSI::Mouse(mouse) => error!("mouse report sent by app? {:?}", mouse),
            CSI::Window(window) => self.state.perform_csi_window(*window),
            CSI::SelectCharacterPath(CharacterPath::ImplementationDefault, _) => {
                self.state.bidi_hint.take();
            }
            CSI::SelectCharacterPath(CharacterPath::LeftToRightOrTopToBottom, _) => {
                self.state
                    .bidi_hint
                    .replace(ParagraphDirectionHint::LeftToRight);
            }
            CSI::SelectCharacterPath(CharacterPath::RightToLeftOrBottomToTop, _) => {
                self.state
                    .bidi_hint
                    .replace(ParagraphDirectionHint::RightToLeft);
            }
            CSI::Keyboard(Keyboard::SetKittyState { flags, mode }) => {
                if self.config.enable_kitty_keyboard() {
                    let current_flags = match self.screen().keyboard_stack.last() {
                        Some(KeyboardEncoding::Kitty(flags)) => *flags,
                        _ => KittyKeyboardFlags::NONE,
                    };
                    let flags = match mode {
                        KittyKeyboardMode::AssignAll => flags,
                        KittyKeyboardMode::SetSpecified => current_flags | flags,
                        KittyKeyboardMode::ClearSpecified => current_flags - flags,
                    };
                    self.screen_mut().keyboard_stack.pop();
                    self.screen_mut()
                        .keyboard_stack
                        .push(KeyboardEncoding::Kitty(flags));
                }
            }
            CSI::Keyboard(Keyboard::PushKittyState { flags, mode }) => {
                if self.config.enable_kitty_keyboard() {
                    let current_flags = match self.screen().keyboard_stack.last() {
                        Some(KeyboardEncoding::Kitty(flags)) => *flags,
                        _ => KittyKeyboardFlags::NONE,
                    };
                    let flags = match mode {
                        KittyKeyboardMode::AssignAll => flags,
                        KittyKeyboardMode::SetSpecified => current_flags | flags,
                        KittyKeyboardMode::ClearSpecified => current_flags - flags,
                    };
                    let screen = self.screen_mut();
                    screen.keyboard_stack.push(KeyboardEncoding::Kitty(flags));
                    if screen.keyboard_stack.len() > 128 {
                        screen.keyboard_stack.remove(0);
                    }
                }
            }
            CSI::Keyboard(Keyboard::PopKittyState(n)) => {
                for _ in 0..n {
                    self.screen_mut().keyboard_stack.pop();
                }
            }
            CSI::Keyboard(Keyboard::QueryKittySupport) => {
                if self.config.enable_kitty_keyboard() {
                    let flags = match self.screen().keyboard_stack.last() {
                        Some(KeyboardEncoding::Kitty(flags)) => *flags,
                        _ => KittyKeyboardFlags::NONE,
                    };
                    write!(self.writer, "\x1b[?{}u", flags.bits()).ok();
                    self.writer.flush().ok();
                }
            }
            CSI::Keyboard(Keyboard::ReportKittyState(_)) => {
                // This is a response to QueryKittySupport and it is invalid for us
                // to receive it. Just ignore it.
            }
            CSI::Unspecified(unspec) => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("unknown unspecified CSI: {:?}", format!("{}", unspec));
                }
            }
        };
    }

    fn esc_dispatch(&mut self, esc: Esc) {
        let seqno = self.seqno;
        self.flush_print();
        if esc != Esc::Code(EscCode::StringTerminator) {
            self.pop_tmux_title_state();
        }
        match esc {
            Esc::Code(EscCode::StringTerminator) => {
                // String Terminator (ST); for the most part has nothing to do here, as its purpose is
                // handled implicitly through a state transition in the vtparse state tables.
                if let Some(title) = self.accumulating_title.take() {
                    self.osc_dispatch(OperatingSystemCommand::SetIconNameAndWindowTitle(title));
                }
            }
            Esc::Code(EscCode::TmuxTitle) => {
                self.accumulating_title.replace(String::new());
            }
            Esc::Code(EscCode::DecApplicationKeyPad) => {
                debug!("DECKPAM on");
                self.application_keypad = true;
            }
            Esc::Code(EscCode::DecNormalKeyPad) => {
                debug!("DECKPAM off");
                self.application_keypad = false;
            }
            Esc::Code(EscCode::ReverseIndex) => self.c1_reverse_index(),
            Esc::Code(EscCode::Index) => self.c1_index(),
            Esc::Code(EscCode::NextLine) => self.c1_nel(),
            Esc::Code(EscCode::HorizontalTabSet) => self.c1_hts(),
            Esc::Code(EscCode::DecLineDrawingG0) => {
                self.g0_charset = CharSet::DecLineDrawing;
            }
            Esc::Code(EscCode::AsciiCharacterSetG0) => {
                self.g0_charset = CharSet::Ascii;
            }
            Esc::Code(EscCode::UkCharacterSetG0) => {
                self.g0_charset = CharSet::Uk;
            }
            Esc::Code(EscCode::DecLineDrawingG1) => {
                self.g1_charset = CharSet::DecLineDrawing;
            }
            Esc::Code(EscCode::AsciiCharacterSetG1) => {
                self.g1_charset = CharSet::Ascii;
            }
            Esc::Code(EscCode::UkCharacterSetG1) => {
                self.g1_charset = CharSet::Uk;
            }
            Esc::Code(EscCode::DecSaveCursorPosition) => self.dec_save_cursor(),
            Esc::Code(EscCode::DecRestoreCursorPosition) => self.dec_restore_cursor(),

            Esc::Code(EscCode::DecDoubleHeightTopHalfLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_double_height_top(seqno);
            }
            Esc::Code(EscCode::DecDoubleHeightBottomHalfLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_double_height_bottom(seqno);
            }
            Esc::Code(EscCode::DecDoubleWidthLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_double_width(seqno);
            }
            Esc::Code(EscCode::DecSingleWidthLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_single_width(seqno);
            }

            Esc::Code(EscCode::DecScreenAlignmentDisplay) => {
                // This one is just to make vttest happy;
                // its original purpose was for aligning the CRT.
                // https://vt100.net/docs/vt510-rm/DECALN.html

                let screen = self.screen_mut();
                let col_range = 0..screen.physical_cols;
                for y in 0..screen.physical_rows as VisibleRowIndex {
                    let line_idx = screen.phys_row(y);
                    let line = screen.line_mut(line_idx);
                    line.resize(col_range.end, seqno);
                    line.fill_range(
                        col_range.clone(),
                        &Cell::new('E', CellAttributes::default()),
                        seqno,
                    );
                }

                self.top_and_bottom_margins = 0..self.screen().physical_rows as VisibleRowIndex;
                self.left_and_right_margins = 0..self.screen().physical_cols;
                self.cursor = Default::default();
            }

            // RIS resets a device to its initial state, i.e. the state it has after it is switched
            // on. This may imply, if applicable: remove tabulation stops, remove qualified areas,
            // reset graphic rendition, erase all positions, move active position to first
            // character position of first line.
            Esc::Code(EscCode::FullReset) => {
                let seqno = self.seqno;
                self.pen = Default::default();
                self.cursor = Default::default();
                self.wrap_next = false;
                self.clear_semantic_attribute_on_newline = false;
                self.insert = false;
                self.dec_auto_wrap = true;
                self.reverse_wraparound_mode = false;
                self.reverse_video_mode = false;
                self.dec_origin_mode = false;
                self.use_private_color_registers_for_each_graphic = false;
                self.color_map = default_color_map();
                self.application_cursor_keys = false;
                self.sixel_display_mode = false;
                self.dec_ansi_mode = false;
                self.application_keypad = false;
                self.bracketed_paste = false;
                self.synchronized_output = false;
                self.focus_tracking = false;
                self.mouse_tracking = false;
                self.mouse_encoding = MouseEncoding::X10;
                self.keyboard_encoding = KeyboardEncoding::Xterm;
                self.modify_other_keys = None;
                self.sixel_scrolls_right = false;
                self.any_event_mouse = false;
                self.button_event_mouse = false;
                self.current_mouse_buttons.clear();
                self.cursor_visible = true;
                self.g0_charset = CharSet::Ascii;
                self.g1_charset = CharSet::Ascii;
                self.shift_out = false;
                self.newline_mode = false;
                self.tabs = TabStop::new(self.screen().physical_cols, 8);
                self.palette.take();
                self.top_and_bottom_margins = 0..self.screen().physical_rows as VisibleRowIndex;
                self.left_and_right_margins = 0..self.screen().physical_cols;
                self.left_and_right_margin_mode = false;
                self.bidi_enabled.take();
                self.bidi_hint.take();
                self.unicode_version = self.config.unicode_version();
                self.unicode_version_stack.clear();
                self.suppress_initial_title_change = false;
                self.accumulating_title.take();
                self.progress = Progress::default();

                self.screen.full_reset();
                self.screen.activate_alt_screen(seqno);
                self.erase_in_display(EraseInDisplay::EraseDisplay);
                self.screen.activate_primary_screen(seqno);
                self.erase_in_display(EraseInDisplay::EraseScrollback);
                self.erase_in_display(EraseInDisplay::EraseDisplay);
                self.palette_did_change();
            }

            _ => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("ESC: unhandled {:?}", esc);
                }
            }
        }
    }

    fn osc_dispatch(&mut self, osc: OperatingSystemCommand) {
        self.pop_tmux_title_state();
        self.flush_print();
        match osc {
            OperatingSystemCommand::SetIconNameSun(title)
            | OperatingSystemCommand::SetIconName(title) => {
                if title.is_empty() {
                    self.icon_title = None;
                } else {
                    self.icon_title = Some(title);
                }
                let title = self.icon_title.clone();
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::IconTitleChanged(title));
                }
            }
            OperatingSystemCommand::SetIconNameAndWindowTitle(title) => {
                self.icon_title.take();
                self.title = title.clone();
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::WindowTitleChanged(title.clone()));
                    handler.alert(Alert::IconTitleChanged(Some(title)));
                }
            }

            OperatingSystemCommand::SetWindowTitleSun(title)
            | OperatingSystemCommand::SetWindowTitle(title) => {
                self.title = title.clone();
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::WindowTitleChanged(title));
                }
            }
            OperatingSystemCommand::SetHyperlink(link) => {
                self.set_hyperlink(link);
            }
            OperatingSystemCommand::Unspecified(unspec) => {
                if self.config.log_unknown_escape_sequences() {
                    let mut output = String::new();
                    write!(&mut output, "Unhandled OSC ").ok();

                    for item in unspec {
                        write!(&mut output, " {}", String::from_utf8_lossy(&item)).ok();
                    }
                    log::warn!("{}", output);
                }
            }

            OperatingSystemCommand::ClearSelection(selection) => {
                let selection = selection_to_selection(selection);
                self.set_clipboard_contents(selection, None).ok();
            }
            OperatingSystemCommand::QuerySelection(_) => {}
            OperatingSystemCommand::SetSelection(selection, selection_data) => {
                let selection = selection_to_selection(selection);
                match self.set_clipboard_contents(selection, Some(selection_data)) {
                    Ok(_) => (),
                    Err(err) => error!("failed to set clipboard in response to OSC 52: {:#?}", err),
                }
            }
            OperatingSystemCommand::ITermProprietary(iterm) => match iterm {
                ITermProprietary::RequestCellSize => {
                    let screen = self.screen();
                    let height = screen.physical_rows;
                    let width = screen.physical_cols;

                    let scale = if screen.dpi == 0 {
                        1.0
                    } else {
                        // Since iTerm2 is a macOS specific piece
                        // of software, it uses the macOS default dpi
                        // if 72 for the basis of its scale, regardless
                        // of the host base dpi.
                        screen.dpi as f32 / 72.
                    };
                    let width = (self.pixel_width as f32 / width as f32) / scale;
                    let height = (self.pixel_height as f32 / height as f32) / scale;

                    let response = OperatingSystemCommand::ITermProprietary(
                        ITermProprietary::ReportCellSize {
                            width_pixels: NotNan::new(width).unwrap(),
                            height_pixels: NotNan::new(height).unwrap(),
                            scale: if screen.dpi == 0 {
                                None
                            } else {
                                Some(NotNan::new(scale).unwrap())
                            },
                        },
                    );
                    write!(self.writer, "{}", response).ok();
                    self.writer.flush().ok();
                }
                #[cfg(feature = "images")]
                ITermProprietary::File(image) => self.set_image(*image),
                #[cfg(not(feature = "images"))]
                ITermProprietary::File(_) => {}
                ITermProprietary::SetUserVar { name, value } => {
                    self.user_vars.insert(name.clone(), value.clone());
                    if let Some(handler) = self.alert_handler.as_mut() {
                        handler.alert(Alert::SetUserVar { name, value });
                    }
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Set(n)) => {
                    self.unicode_version.version = n;
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Push(label)) => {
                    let vers = self.unicode_version.clone();
                    self.unicode_version_stack
                        .push(UnicodeVersionStackEntry { vers, label });
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Pop(None)) => {
                    if let Some(entry) = self.unicode_version_stack.pop() {
                        self.unicode_version = entry.vers;
                    }
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Pop(Some(label))) => {
                    while let Some(entry) = self.unicode_version_stack.pop() {
                        self.unicode_version = entry.vers;
                        if entry.label.as_deref() == Some(&label) {
                            break;
                        }
                    }
                }
                _ => {
                    if self.config.log_unknown_escape_sequences() {
                        log::warn!("unhandled iterm2: {:?}", iterm);
                    }
                }
            },

            OperatingSystemCommand::FinalTermSemanticPrompt(FinalTermSemanticPrompt::FreshLine) => {
                self.fresh_line();
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::FreshLineAndStartPrompt { .. },
            ) => {
                self.fresh_line();
                self.pen.set_semantic_type(SemanticType::Prompt);
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::StartPrompt(_),
            ) => {
                self.pen.set_semantic_type(SemanticType::Prompt);
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfCommandWithFreshLine { .. },
            ) => {
                self.fresh_line();
                self.pen.set_semantic_type(SemanticType::Prompt);
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfPromptAndStartOfInputUntilNextMarker,
            ) => {
                self.pen.set_semantic_type(SemanticType::Input);
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfPromptAndStartOfInputUntilEndOfLine,
            ) => {
                self.pen.set_semantic_type(SemanticType::Input);
                self.clear_semantic_attribute_on_newline = true;
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfInputAndStartOfOutput { .. },
            ) => {
                self.pen.set_semantic_type(SemanticType::Output);
            }

            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::CommandStatus { .. },
            ) => {}

            OperatingSystemCommand::SystemNotification(message) => {
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::ToastNotification {
                        title: None,
                        body: message,
                        focus: true,
                    });
                } else {
                    log::info!("Application sends SystemNotification: {}", message);
                }
            }
            OperatingSystemCommand::RxvtExtension(params) => {
                if let Some("notify") = params.first().map(String::as_str) {
                    let title = params.get(1);
                    let body = params.get(2);
                    let (title, body) = match (title.cloned(), body.cloned()) {
                        (Some(title), None) => (None, title),
                        (Some(title), Some(body)) => (Some(title), body),
                        _ => {
                            log::warn!("malformed rxvt notify escape: {:?}", params);
                            return;
                        }
                    };
                    if let Some(handler) = self.alert_handler.as_mut() {
                        handler.alert(Alert::ToastNotification {
                            title,
                            body,
                            focus: true,
                        });
                    }
                }
            }
            OperatingSystemCommand::CurrentWorkingDirectory(url) => {
                // The OSC 7 location, as the program sent it (normally a file:// URL).
                self.current_dir = Some(url);
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::CurrentWorkingDirectoryChanged);
                }
            }
            OperatingSystemCommand::ChangeColorNumber(specs) => {
                log::trace!("ChangeColorNumber: {:?}", specs);
                for pair in specs {
                    match pair.color {
                        ColorOrQuery::Query => {
                            let response =
                                OperatingSystemCommand::ChangeColorNumber(vec![ChangeColorPair {
                                    palette_index: pair.palette_index,
                                    color: ColorOrQuery::Color(
                                        self.palette().colors.0[pair.palette_index as usize],
                                    ),
                                }]);
                            write!(self.writer, "{}", response).ok();
                            self.writer.flush().ok();
                        }
                        ColorOrQuery::Color(c) => {
                            self.palette_mut().colors.0[pair.palette_index as usize] = c;
                        }
                    }
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }

            OperatingSystemCommand::ResetColors(colors) => {
                log::trace!("ResetColors: {:?}", colors);
                if colors.is_empty() {
                    // Reset all colors
                    self.palette.take();
                } else {
                    // Reset individual colors
                    if self.palette.is_none() {
                        // Already at the defaults
                    } else {
                        let base = self.config.color_palette();
                        for c in colors {
                            let c = c as usize;
                            self.palette_mut().colors.0[c] = base.colors.0[c];
                        }
                    }
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }

            OperatingSystemCommand::ChangeDynamicColors(first_color, colors) => {
                log::trace!("ChangeDynamicColors: {:?} {:?}", first_color, colors);
                use wezterm_escape_parser::osc::DynamicColorNumber;
                for (idx, color) in (first_color as u8..).zip(colors) {
                    let which_color: Option<DynamicColorNumber> = FromPrimitive::from_u8(idx);
                    log::trace!("ChangeDynamicColors item: {:?}", which_color);
                    if let Some(which_color) = which_color {
                        macro_rules! set_or_query {
                            ($name:ident) => {
                                match color {
                                    ColorOrQuery::Query => {
                                        let response = OperatingSystemCommand::ChangeDynamicColors(
                                            which_color,
                                            vec![ColorOrQuery::Color(self.palette().$name.into())],
                                        );
                                        log::trace!("Color Query response {:?}", response);
                                        write!(self.writer, "{}", response).ok();
                                        self.writer.flush().ok();
                                    }
                                    ColorOrQuery::Color(c) => self.palette_mut().$name = c.into(),
                                }
                            };
                        }
                        match which_color {
                            DynamicColorNumber::TextForegroundColor => set_or_query!(foreground),
                            DynamicColorNumber::TextBackgroundColor => set_or_query!(background),
                            DynamicColorNumber::TextCursorColor => {
                                if let ColorOrQuery::Color(c) = color {
                                    // We set the border to the background color; we don't
                                    // have an escape that sets that independently, and this
                                    // way just looks better.
                                    self.palette_mut().cursor_border = c;
                                }
                                set_or_query!(cursor_bg)
                            }
                            DynamicColorNumber::HighlightForegroundColor => {
                                set_or_query!(selection_fg)
                            }
                            DynamicColorNumber::HighlightBackgroundColor => {
                                set_or_query!(selection_bg)
                            }
                            DynamicColorNumber::MouseForegroundColor
                            | DynamicColorNumber::MouseBackgroundColor
                            | DynamicColorNumber::TektronixForegroundColor
                            | DynamicColorNumber::TektronixBackgroundColor
                            | DynamicColorNumber::TektronixCursorColor => {}
                        }
                    }
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }

            OperatingSystemCommand::ResetDynamicColor(color) => {
                log::trace!("ResetDynamicColor: {:?}", color);
                use wezterm_escape_parser::osc::DynamicColorNumber;
                let which_color: Option<DynamicColorNumber> = FromPrimitive::from_u8(color as u8);
                if let Some(which_color) = which_color {
                    macro_rules! reset {
                        ($name:ident) => {
                            if self.palette.is_none() {
                                // Already at the defaults
                            } else {
                                let base = self.config.color_palette();
                                self.palette_mut().$name = base.$name;
                            }
                        };
                    }
                    match which_color {
                        DynamicColorNumber::TextForegroundColor => reset!(foreground),
                        DynamicColorNumber::TextBackgroundColor => reset!(background),
                        DynamicColorNumber::TextCursorColor => {
                            reset!(cursor_bg);
                            // Since we set the border to the bg, we consider it reset
                            // by resetting the bg too!
                            reset!(cursor_border);
                        }
                        DynamicColorNumber::HighlightForegroundColor => reset!(selection_fg),
                        DynamicColorNumber::HighlightBackgroundColor => reset!(selection_bg),
                        DynamicColorNumber::MouseForegroundColor
                        | DynamicColorNumber::MouseBackgroundColor
                        | DynamicColorNumber::TektronixForegroundColor
                        | DynamicColorNumber::TektronixBackgroundColor
                        | DynamicColorNumber::TektronixCursorColor => {}
                    }
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }
            OperatingSystemCommand::ConEmuProgress(prog) => {
                use wezterm_escape_parser::osc::Progress as TProg;
                let prog = match prog {
                    TProg::None => Progress::None,
                    TProg::SetPercentage(p) => Progress::Percentage(p),
                    TProg::SetError(p) => Progress::Error(p),
                    TProg::SetIndeterminate => Progress::Indeterminate,
                    TProg::Paused => Progress::None,
                };
                if prog != self.progress {
                    self.progress = prog.clone();
                    if let Some(handler) = self.alert_handler.as_mut() {
                        handler.alert(Alert::Progress(prog));
                    }
                }
            }
        }
    }
}

fn selection_to_selection(sel: Selection) -> ClipboardSelection {
    match sel {
        Selection::CLIPBOARD => ClipboardSelection::Clipboard,
        Selection::PRIMARY => ClipboardSelection::PrimarySelection,
        // xterm will use a configurable selection in the NONE case
        Selection::NONE => ClipboardSelection::Clipboard,
        // otherwise we just use clipboard.  Could potentially
        // also use the same fallback configuration as NONE,
        // if/when we add it
        _ => ClipboardSelection::Clipboard,
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn characters_standing_alone_are_a_grapheme_each_beside_one_another() {
        let neighbors: Vec<char> = STANDING_ALONE
            .iter()
            .flat_map(|&(first, last)| [first, (first + last) / 2, last])
            .filter_map(char::from_u32)
            .collect();
        for c in STANDING_ALONE
            .iter()
            .flat_map(|&(first, last)| first..=last)
            .filter_map(char::from_u32)
        {
            assert!(stands_alone(c), "{:?}", c);
            let version = wezterm_cell::LATEST_UNICODE_VERSION;
            assert_eq!(
                text_char_column_width(c, &version),
                char_column_width(c, Some(&version)),
                "{:?}",
                c
            );
            for &n in &neighbors {
                for pair in [format!("{c}{n}"), format!("{n}{c}")] {
                    assert_eq!(Graphemes::new(&pair).count(), 2, "{:?}", pair);
                }
            }
        }
        for c in [
            '\u{301}', '\u{483}', '\u{200d}', '\u{fe0f}', '\u{3099}', '\u{1100}', '😀',
        ] {
            assert!(!stands_alone(c), "{:?}", c);
        }
    }

    #[test]
    fn a_grapheme_ends_where_the_segmenter_ends_it() {
        let text = "héllo e\u{301} 日本語 Ελληνικά текст й\u{308} 한국어 \u{1100}\u{1161} 😀\u{fe0f}a 👍🏽 🇯🇵🇫🇷 x";
        let mut ours = Vec::new();
        let mut rest = text;
        let mut ahead = None;
        while !rest.is_empty() {
            let (g, single) = first_grapheme(rest, &mut ahead);
            assert_eq!(
                single.map(String::from).as_deref(),
                Some(g).filter(|g| g.chars().count() == 1)
            );
            ours.push(g);
            rest = &rest[g.len()..];
        }
        assert_eq!(ours, Graphemes::new(text).collect::<Vec<_>>());
    }
}
