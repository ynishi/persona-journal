//! File layer (Content). 1 entry = 1 file (versioning OFF) or 1 dir per entry (versioning ON).
//!
//! Pure path helpers are re-exported from `persona_journal_core::storage`.
//! I/O helpers (`write_file`, `read_file`) remain here.

pub use persona_journal_core::storage::{
    extract_first_line, flat_path, seq_in_kind_str, uname, versioned_path,
};

use std::path::Path;

use crate::error::Result;

pub fn write_file(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)?;
    Ok(())
}

// Kept for future projection repair / debug tooling. entry_read now reads from DB (versions.body).
#[allow(dead_code)]
pub fn read_file(path: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(path)?)
}
