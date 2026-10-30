//! Platform-independent heart of Nothing: the index, the query engine, the snapshot
//! format and the UI <-> service wire protocol. No Win32 in here, so it is unit-testable
//! and the UI binary links only the protocol bits it uses (LTO drops the rest).

pub mod fold;
pub mod index;
pub mod persist;
pub mod pool;
pub mod proto;
pub mod search;

pub use index::{flag, Index, Sort, Volume, NONE};
