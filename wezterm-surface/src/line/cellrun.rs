use alloc::vec::Vec;

/// Graphemes placed one after another along a row, described by the text
/// they cover. Each grapheme is one character and one column wide unless it
/// is listed: most text has no listed graphemes at all, so building a run
/// and placing it cost no work per grapheme.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CellRun {
    /// Bytes of text the run covers.
    len: usize,
    /// Columns the run takes.
    width: usize,
    /// The column, counted from the run's start, and the width of each
    /// grapheme wider than one column.
    wide: Vec<(usize, usize)>,
    /// The byte offset, from the run's start, and the length of each
    /// grapheme of more than one character.
    joined: Vec<(usize, usize)>,
    /// The width of the last grapheme.
    last: usize,
}

impl CellRun {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Empties the run, keeping its buffers.
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
        self.width = 0;
        self.wide.clear();
        self.joined.clear();
        self.last = 0;
    }

    /// Bytes of text the run covers.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Columns the run takes.
    #[inline]
    pub fn width(&self) -> usize {
        self.width
    }

    /// The width of the last grapheme, or 0 for an empty run.
    #[inline]
    pub fn last_width(&self) -> usize {
        self.last
    }

    /// The column, from the run's start, and width of each grapheme wider
    /// than one column.
    #[inline]
    pub fn wide(&self) -> &[(usize, usize)] {
        &self.wide
    }

    /// Adds `n` graphemes of one byte and one column each, such as ASCII.
    #[inline]
    pub fn push_narrow(&mut self, n: usize) {
        if n > 0 {
            self.len += n;
            self.width += n;
            self.last = 1;
        }
    }

    /// Adds one grapheme of `len` bytes taking `width` columns; `single`
    /// when it is one character.
    #[inline]
    pub fn push(&mut self, len: usize, width: usize, single: bool) {
        if !single {
            self.joined.push((self.len, len));
        }
        if width != 1 {
            self.wide.push((self.width, width));
        }
        self.len += len;
        self.width += width;
        self.last = width;
    }

    /// Each grapheme of `text`, the text the run covers, with its width.
    pub fn graphemes<'t>(&'t self, text: &'t str) -> impl Iterator<Item = (&'t str, usize)> + 't {
        let mut joined = self.joined.iter().peekable();
        let mut wide = self.wide.iter().peekable();
        let (mut at, mut column) = (0, 0);
        core::iter::from_fn(move || {
            let rest = text.get(at..).filter(|rest| !rest.is_empty())?;
            let len = match joined.next_if(|(offset, _)| *offset == at) {
                Some(&(_, len)) => len,
                None => rest.chars().next().map_or(1, char::len_utf8),
            };
            let width = match wide.next_if(|(col, _)| *col == column) {
                Some(&(_, width)) => width,
                None => 1,
            };
            let grapheme = rest.get(..len)?;
            at += len;
            column += width;
            Some((grapheme, width))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::CellRun;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn a_run_gives_back_each_grapheme_with_its_width() {
        let text = "ab日e\u{301}x";
        let mut run = CellRun::default();
        run.push_narrow(2);
        run.push("日".len(), 2, true);
        run.push("e\u{301}".len(), 1, false);
        run.push(1, 1, true);
        assert_eq!(run.len(), text.len());
        assert_eq!(run.width(), 6);
        assert_eq!(run.last_width(), 1);
        assert_eq!(run.wide(), &[(2, 2)]);
        let graphemes: Vec<_> = run.graphemes(text).collect();
        assert_eq!(
            graphemes,
            vec![("a", 1), ("b", 1), ("日", 2), ("e\u{301}", 1), ("x", 1)]
        );
        run.clear();
        assert!(run.is_empty());
        assert_eq!(run.graphemes("").count(), 0);
    }
}
