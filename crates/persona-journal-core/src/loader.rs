//! `.journal.toml` loader — parse kind configs from a persona's TOML file.
//!
//! This module owns the canonical TOML → `KindConfig` parsing logic shared by
//! both the loader (`Journal::ensure_loaded`) and the MCP `journal_kind_register`
//! handler.

use serde::Deserialize;

use crate::error::{CoreError, Result};
use crate::schema::{DecayConfig, KindConfig, KindMode, NamedSource};

// ── private raw deserialization structs ──────────────────────────────────────

#[derive(Deserialize)]
struct Raw {
    kind: String,
    mode: String,
    source: Option<String>,
    path_template: String,
    #[serde(default = "default_true")]
    versioning: bool,
    #[serde(default = "default_true")]
    indexed: bool,
    #[serde(default)]
    decay: RawDecay,
    /// Absent when the key is not present in TOML; `unwrap_or(1.0)` restores default.
    boost_factor: Option<f64>,
    #[serde(default)]
    tags: Vec<String>,
    body_template: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawDecay {
    half_life_days: Option<f64>,
    weight: Option<f64>,
}

fn default_true() -> bool {
    true
}

/// Wrapper for the `[[kinds]]` array-of-tables format.
#[derive(Deserialize)]
struct RawJournal {
    #[serde(default)]
    kinds: Vec<Raw>,
}

// ── public API ────────────────────────────────────────────────────────────────

/// Parse the contents of a single-kind TOML string and return a `KindConfig`.
///
/// Recognized fields: `kind`, `mode`, `source`, `path_template`,
/// `versioning` (default `true`), `indexed` (default `true`),
/// `decay`, `tags` (default `[]`), `body_template`.
///
/// # Errors
/// - `CoreError::TomlDe` — TOML deserialization failure (unknown field,
///   type mismatch, etc.).
/// - `CoreError::Invalid` — `mode` string is not a recognized `KindMode`
///   variant, or `source` string is not a recognized `NamedSource` variant.
///
/// # Concurrency
/// Pure function with no shared state.  `Send + Sync`.  Safe to call
/// concurrently from any number of threads.
///
/// # Cancel Safety
/// Does not use `.await`.  Cancel-safe by construction.
pub fn parse_kind_toml(src: &str) -> Result<KindConfig> {
    let r: Raw = toml::from_str(src)?;
    raw_to_kind_config(r, src)
}

/// Parse the contents of a `.journal.toml` file and return all defined kinds.
///
/// The TOML format uses `[[kinds]]` array-of-tables; each entry has the same
/// fields as the single-kind TOML accepted by `parse_kind_toml`.
/// Returns `Ok(vec![])` for a valid TOML that defines zero kinds.
///
/// # Errors
/// - `CoreError::TomlDe` — TOML deserialization failure.
/// - `CoreError::Invalid` — `mode` or `source` field contains an unrecognized
///   value.
///
/// # Concurrency
/// Pure function with no shared state.  `Send + Sync`.  Safe to call
/// concurrently from any number of threads.
///
/// # Cancel Safety
/// Does not use `.await`.  Cancel-safe by construction.
pub fn parse_journal_toml(src: &str) -> Result<Vec<KindConfig>> {
    let journal: RawJournal = toml::from_str(src)?;
    journal
        .kinds
        .into_iter()
        .map(|r| {
            // For each entry in the array-of-tables we reconstruct a per-entry
            // TOML string to store in `config_toml` (opaque passthrough).
            // We use toml::to_string on the Raw's fields directly — but Raw
            // doesn't implement Serialize. Instead we build a minimal TOML
            // representation from the KindConfig after construction.
            // The `config_toml` field will be a serialized representation of
            // the kind entry (not the full file), which is consistent with the
            // single-kind `parse_kind_toml` contract.
            let cfg = raw_to_kind_config(r, "")?;
            let config_toml = cfg.to_config_toml();
            Ok(KindConfig { config_toml, ..cfg })
        })
        .collect()
}

// ── private helpers ───────────────────────────────────────────────────────────

fn raw_to_kind_config(r: Raw, src: &str) -> Result<KindConfig> {
    let mode = KindMode::parse(&r.mode)
        .ok_or_else(|| CoreError::Invalid(format!("invalid mode: {}", r.mode)))?;
    let source = match r.source.as_deref() {
        None => None,
        Some(s) => Some(
            NamedSource::parse(s)
                .ok_or_else(|| CoreError::Invalid(format!("invalid source: {s}")))?,
        ),
    };
    Ok(KindConfig {
        kind: r.kind,
        mode,
        source,
        path_template: r.path_template,
        versioning: r.versioning,
        indexed: r.indexed,
        decay: DecayConfig {
            half_life_days: r.decay.half_life_days.unwrap_or(30.0),
            weight: r.decay.weight.unwrap_or(1.0),
        },
        // Crux C3: absent boost_factor key restores the default (1.0).
        boost_factor: r.boost_factor.unwrap_or(1.0),
        tags: r.tags,
        body_template: r.body_template,
        config_toml: src.to_string(),
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const EMO_TOML: &str = r#"
kind = "emo"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []
"#;

    #[test]
    fn parse_kind_toml_entries_mode() {
        let cfg = parse_kind_toml(EMO_TOML.trim()).unwrap();
        assert_eq!(cfg.kind, "emo");
        assert!(matches!(cfg.mode, KindMode::Entries));
        assert!(cfg.versioning);
        assert!(cfg.indexed);
        assert!(cfg.tags.is_empty());
    }

    #[test]
    fn parse_kind_toml_named_index_mode() {
        let toml = r#"
kind = "non_rem"
mode = "named_index"
source = "hand"
path_template = "{persona}/{persona}_{kind}_index.md"
versioning = false
indexed = false
tags = []
"#;
        let cfg = parse_kind_toml(toml.trim()).unwrap();
        assert_eq!(cfg.kind, "non_rem");
        assert!(matches!(cfg.mode, KindMode::NamedIndex));
        assert!(matches!(cfg.source, Some(NamedSource::Hand)));
        assert!(!cfg.versioning);
        assert!(!cfg.indexed);
    }

    #[test]
    fn parse_kind_toml_invalid_mode_returns_error() {
        let toml = r#"
kind = "x"
mode = "unknown_mode"
path_template = "foo"
"#;
        let err = parse_kind_toml(toml.trim()).unwrap_err();
        assert!(
            matches!(err, CoreError::Invalid(_)),
            "expected Invalid, got: {err:?}"
        );
    }

    #[test]
    fn parse_journal_toml_parses_multiple_kinds() {
        let src = r#"
[[kinds]]
kind = "emo"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []

[[kinds]]
kind = "non_rem"
mode = "named_index"
source = "hand"
path_template = "{persona}/{persona}_{kind}_index.md"
versioning = false
indexed = false
tags = []
"#;
        let kinds = parse_journal_toml(src).unwrap();
        assert_eq!(kinds.len(), 2);
        assert_eq!(kinds[0].kind, "emo");
        assert_eq!(kinds[1].kind, "non_rem");
        assert!(matches!(kinds[1].source, Some(NamedSource::Hand)));
    }

    #[test]
    fn parse_journal_toml_empty_returns_empty_vec() {
        let kinds = parse_journal_toml("").unwrap();
        assert!(kinds.is_empty());
    }

    #[test]
    fn parse_journal_toml_zero_kinds_section_ok() {
        // Valid TOML with no [[kinds]] entries.
        let src = "# just a comment\n";
        let kinds = parse_journal_toml(src).unwrap();
        assert!(kinds.is_empty());
    }
}
