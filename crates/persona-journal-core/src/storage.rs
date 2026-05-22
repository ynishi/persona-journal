//! Pure storage path helpers (no I/O).

use std::path::{Path, PathBuf};

/// Returns the `seq_in_kind` string: `"{year_month}_{seq:06}"`.
///
/// `year_month` must already be in `"YYYY-MM"` format.
/// kind lowercase enforcement is the caller's responsibility.
pub fn seq_in_kind_str(year_month: &str, seq: u32) -> String {
    format!("{}_{:06}", year_month, seq)
}

/// Returns the uname: `"{kind}/{seq_in_kind}"`.
///
/// Both arguments are used verbatim; callers must ensure `kind` is lowercase
/// and `seq_in_kind` is a valid `seq_in_kind_str` output.
pub fn uname(kind: &str, seq_in_kind: &str) -> String {
    format!("{}/{}", kind, seq_in_kind)
}

/// versioning OFF: `<root>/<persona>/<kind>/<seq_in_kind>.md`
pub fn flat_path(root: &Path, persona: &str, kind: &str, seq_in_kind: &str) -> PathBuf {
    root.join(persona)
        .join(kind)
        .join(format!("{}.md", seq_in_kind))
}

/// versioning ON: `<root>/<persona>/<kind>/<seq_in_kind>/<seq_in_kind>_vN.md`
pub fn versioned_path(
    root: &Path,
    persona: &str,
    kind: &str,
    seq_in_kind: &str,
    version: u32,
) -> PathBuf {
    root.join(persona)
        .join(kind)
        .join(seq_in_kind)
        .join(format!("{}_v{}.md", seq_in_kind, version))
}

pub fn extract_first_line(body: &str) -> Option<String> {
    body.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().trim_start_matches('#').trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn seq_in_kind_str_zero_pads_to_6() {
        assert_eq!(seq_in_kind_str("2024-08", 1), "2024-08_000001");
        assert_eq!(seq_in_kind_str("2024-08", 999999), "2024-08_999999");
    }

    #[test]
    fn uname_format() {
        assert_eq!(uname("emo", "2024-08_000001"), "emo/2024-08_000001");
    }

    #[test]
    fn flat_path_layout() {
        let root = PathBuf::from("/data");
        assert_eq!(
            flat_path(&root, "alice", "emo", "2024-08_000001"),
            PathBuf::from("/data/alice/emo/2024-08_000001.md")
        );
    }

    #[test]
    fn versioned_path_layout() {
        let root = PathBuf::from("/data");
        assert_eq!(
            versioned_path(&root, "alice", "emo", "2024-08_000001", 3),
            PathBuf::from("/data/alice/emo/2024-08_000001/2024-08_000001_v3.md")
        );
    }

    #[test]
    fn extract_first_line_trims_header() {
        assert_eq!(
            extract_first_line("# Hello\n\nworld"),
            Some("Hello".to_string())
        );
    }

    #[test]
    fn extract_first_line_empty_body() {
        assert_eq!(extract_first_line("   \n\n"), None);
    }
}
