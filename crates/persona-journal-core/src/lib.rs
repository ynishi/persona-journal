//! persona-journal-core — pure-parse types shared across persona-journal crates.
//!
//! This crate contains no I/O and no rusqlite dependency.
//! It provides the canonical TOML→`KindConfig` parsing logic and schema types.

pub mod error;
pub mod loader;
pub mod schema;
pub mod storage;

pub use error::{CoreError, Result};
