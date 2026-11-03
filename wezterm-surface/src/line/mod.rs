mod cellref;
mod clusterline;
mod cold;
// line::line holds the Line type itself, as in upstream wezterm.
#[allow(clippy::module_inception)]
mod line;
mod linebits;
mod storage;
mod test;
mod vecstorage;

pub use cellref::CellRef;
pub use cold::inflations as cold_inflations;
pub use line::{DoubleClickRange, Line};
