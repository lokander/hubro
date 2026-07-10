//! Library crate backing the `dataview` binary. Exists so integration tests
//! (and eventually other tooling) can import app code; `main.rs` remains the
//! binary entry point.

pub mod db;
pub mod util;
