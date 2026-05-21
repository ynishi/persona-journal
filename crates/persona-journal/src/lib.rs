//! persona-journal — local-first, SQLite-backed, versioned diary for a persona.
//!
//! See `DESIGN.md` at the repo root. The DB (`_journal.db` per persona under
//! `<root>/<persona>/`) is the single source of truth for both content and meta.
//! Filesystem `.md` files are derived projections regenerated from the DB;
//! call `Journal::projection_rebuild` to restore FS consistency from the DB SoT.

pub mod db;
pub mod error;
pub mod journal;
pub mod loader;
pub mod projection;
pub mod schema;
pub mod storage;

pub use error::{Error, Result};
pub use journal::{EntryRow, FilterMode, Journal};
pub use schema::{KindConfig, KindMode, NamedSource};
// NOTE: CoreError is re-exported for callers that need to inspect Core-layer errors.
// K-85: pub use re-export and explicit path coexist here for backward compat (not avoidable).
pub use persona_journal_core::CoreError;
