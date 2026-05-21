//! Index / projection. SoT is SQLite; md files are derived snapshots.

use std::fmt::Write;
use std::path::Path;

use crate::db::Db;
use crate::error::Result;
use crate::storage::write_file;

const ROOT_INDEX_LATEST_N: usize = 10;

/// Write RootIndex (spec §6.1) to `<root>/<persona>/_index.md`.
/// Collects kinds where `indexed = true` and lists the latest N entries per kind.
pub fn render_root_index(db: &Db, persona: &str, now_iso: &str) -> Result<String> {
    let kinds = db.list_indexed_kinds()?;
    let mut out = String::new();
    let _ = writeln!(out, "# {} journal", persona);
    let _ = writeln!(out);
    let _ = writeln!(out, "_Last derived: {}_", now_iso);
    let _ = writeln!(out);
    for k in &kinds {
        let rows = db.query_latest(&k.kind, ROOT_INDEX_LATEST_N)?;
        if rows.is_empty() {
            continue;
        }
        let _ = writeln!(out, "## {} (latest {})", k.kind, rows.len());
        let _ = writeln!(out);
        for r in rows {
            let tag_part = if r.tags.is_empty() {
                String::new()
            } else {
                let joined: Vec<String> = r.tags.iter().map(|t| format!("`{}`", t)).collect();
                format!(" {}", joined.join(" "))
            };
            let summary = r
                .first_line_cache
                .as_deref()
                .map(|s| format!(" — {}", s))
                .unwrap_or_default();
            let _ = writeln!(out, "- {}{}{}", r.id, tag_part, summary);
        }
        let _ = writeln!(out);
    }
    Ok(out)
}

pub fn write_root_index(root: &Path, persona: &str, content: &str) -> Result<()> {
    let path = root.join(persona).join("_index.md");
    write_file(&path, content)
}
