use crate::line::clusterline::ClusteredLine;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use wezterm_cell::CellAttributes;

/// Counts block inflations, so a compaction looks for inflated blocks only
/// after a read inflated one.
static INFLATIONS: AtomicU64 = AtomicU64::new(0);

/// How many cold blocks reads have inflated so far.
pub fn inflations() -> u64 {
    INFLATIONS.load(Ordering::Relaxed)
}

/// Rows of history stored compressed together. They inflate as a block the
/// first time any of them is read.
pub(crate) struct ColdBlock {
    data: Box<[u8]>,
    attrs: Box<[CellAttributes]>,
    rows: usize,
    /// The heap memory of the compressed rows and their attributes, which
    /// never change once the block is made.
    stored: usize,
    inflated: OnceLock<Box<[ClusteredLine]>>,
}

impl ColdBlock {
    pub(crate) fn new(lines: &[&ClusteredLine]) -> Self {
        let mut raw = Vec::with_capacity(lines.iter().map(|line| line.encoded_size()).sum());
        let mut attrs = Vec::new();
        for line in lines {
            line.encode(&mut raw, &mut attrs);
        }
        let data = lz4_flex::compress_prepend_size(&raw).into_boxed_slice();
        let stored = data.len()
            + attrs
                .iter()
                .map(|a| core::mem::size_of::<CellAttributes>() + a.heap_bytes())
                .sum::<usize>()
            + core::mem::size_of::<Self>();
        Self {
            data,
            attrs: attrs.into_boxed_slice(),
            rows: lines.len(),
            stored,
            inflated: OnceLock::new(),
        }
    }

    /// Whether a read has inflated the block.
    pub(crate) fn inflated(&self) -> bool {
        self.inflated.get().is_some()
    }

    /// Heap memory the block holds for each of its rows: the compressed rows
    /// and their attributes. The rows a read inflates are left out, as the
    /// next compaction frees them, so a row's count never changes.
    pub(crate) fn heap_bytes_per_row(&self) -> usize {
        self.stored / self.rows.max(1)
    }

    pub(crate) fn lines(&self) -> &[ClusteredLine] {
        self.inflated.get_or_init(|| {
            INFLATIONS.fetch_add(1, Ordering::Relaxed);
            let raw = lz4_flex::decompress_size_prepended(&self.data).unwrap_or_default();
            let mut input = raw.as_slice();
            (0..self.rows)
                .map(|_| ClusteredLine::decode(&mut input, &self.attrs))
                .collect()
        })
    }
}

/// One row of a cold block.
#[derive(Clone)]
pub(crate) struct ColdRow {
    pub(crate) block: Arc<ColdBlock>,
    pub(crate) row: usize,
}

impl ColdRow {
    pub(crate) fn line(&self) -> &ClusteredLine {
        &self.block.lines()[self.row]
    }
}

impl core::fmt::Debug for ColdRow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.line().fmt(f)
    }
}

impl PartialEq for ColdRow {
    fn eq(&self, other: &Self) -> bool {
        self.line() == other.line()
    }
}
