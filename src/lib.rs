//! A tiny, agent-friendly RSS/Atom reader. The binary in `main.rs` is the public
//! interface; these modules are its internals.

pub mod db;
pub mod error;
pub mod feed;
pub mod fetch;
pub mod opml;
pub mod text;
pub mod time;
