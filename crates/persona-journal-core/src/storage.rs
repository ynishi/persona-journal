//! Pure storage path helpers (no I/O).

use std::path::{Path, PathBuf};

pub fn entry_id(year: i32, month: u32, seq: u32) -> String {
    format!("{:04}-{:02}_{:05}", year, month, seq)
}

/// versioning OFF: `<persona>/<kind>/<persona>_<kind>_<entryId>.md`
pub fn flat_path(root: &Path, persona: &str, kind: &str, entry_id: &str) -> PathBuf {
    root.join(persona)
        .join(kind)
        .join(format!("{}_{}_{}.md", persona, kind, entry_id))
}

/// versioning ON: `<persona>/<kind>/<persona>_<kind>_<entryId>/<persona>_<kind>_<entryId>_vN.md`
pub fn versioned_path(
    root: &Path,
    persona: &str,
    kind: &str,
    entry_id: &str,
    version: u32,
) -> PathBuf {
    root.join(persona)
        .join(kind)
        .join(format!("{}_{}_{}", persona, kind, entry_id))
        .join(format!("{}_{}_{}_v{}.md", persona, kind, entry_id, version))
}

pub fn extract_first_line(body: &str) -> Option<String> {
    body.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().trim_start_matches('#').trim().to_string())
        .filter(|s| !s.is_empty())
}
