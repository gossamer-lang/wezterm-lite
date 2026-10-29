mod cellref;
mod clusterline;
// line::line holds the Line type itself, as in upstream wezterm.
#[allow(clippy::module_inception)]
mod line;
mod linebits;
mod storage;
mod test;
mod vecstorage;

pub use cellref::CellRef;
pub use line::{DoubleClickRange, Line};
