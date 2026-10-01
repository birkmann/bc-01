//! Pure, DOM-free logic. Everything here is unit-tested natively (`cargo test -p bc-ui`);
//! the ports of the legacy vitest suites live next to the code they test.
pub mod cache_core;
pub mod format;
pub mod fuzzy;
pub mod grid_place;
pub mod paging;
pub mod range_select;
pub mod realtime_conn;
pub mod selection;
pub mod shortcuts;
pub mod timeline;
