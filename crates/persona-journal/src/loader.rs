//! `.journal.toml` loader — thin shim re-exporting Core parse functions.
//!
//! Pure parsing logic lives in `persona_journal_core::loader`.
//! This module re-exports the public API for backward compatibility and
//! retains the test-only load counter used by integration tests in journal.rs.

pub use persona_journal_core::loader::{parse_journal_toml, parse_kind_toml};

// ── test-only load counter ────────────────────────────────────────────────────
//
// Per-(canonical_root, persona) counter so concurrent tests in different temp
// dirs don't interfere with each other's assertions.
// `pub(crate) use` comes before the mod declaration to satisfy
// clippy::items_after_test_module lint.

#[cfg(test)]
pub(crate) use self::load_counter::{get as load_counter_get, increment as load_counter_inc};

#[cfg(test)]
mod load_counter {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    type Key = (PathBuf, String);
    static MAP: OnceLock<Mutex<HashMap<Key, usize>>> = OnceLock::new();

    fn map() -> &'static Mutex<HashMap<Key, usize>> {
        MAP.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(crate) fn increment(root: &Path, persona: &str) {
        let key = (root.to_path_buf(), persona.to_string());
        let mut m = map().lock().unwrap();
        *m.entry(key).or_insert(0) += 1;
    }

    pub(crate) fn get(root: &Path, persona: &str) -> usize {
        let key = (root.to_path_buf(), persona.to_string());
        let m = map().lock().unwrap();
        *m.get(&key).unwrap_or(&0)
    }
}
