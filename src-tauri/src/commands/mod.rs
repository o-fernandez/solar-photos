//! The Tauri command handlers, split by domain. `lib.rs` keeps the shared
//! state, setup, background orchestration and the thumb protocol.

pub mod curation;
pub mod geo;
pub mod library;
pub mod people;
