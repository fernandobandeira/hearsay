//! narrator — self-hosted TTS book reader, the Rust server.
//!
//! The python implementation at `~/git/narrator` is the reference; this crate
//! re-implements its HTTP contract, its cache layout, its chunking (bug for bug)
//! and its vault writes, so the two are interchangeable in front of the same
//! `work/` directory and the same Obsidian vault.

pub mod api;
pub mod book;
pub mod cache;
pub mod chapters;
pub mod config;
pub mod err;
pub mod events;
pub mod plancache;
pub mod render;
pub mod state;
pub mod stt;
pub mod text;
pub mod tts;
pub mod vault;
pub mod watch;
