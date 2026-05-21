use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KindMode {
    Entries,
    NamedIndex,
}

impl KindMode {
    pub fn as_str(self) -> &'static str {
        match self {
            KindMode::Entries => "entries",
            KindMode::NamedIndex => "named_index",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "entries" => Some(KindMode::Entries),
            "named_index" => Some(KindMode::NamedIndex),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedSource {
    Hand,
    Query,
}

impl NamedSource {
    pub fn as_str(self) -> &'static str {
        match self {
            NamedSource::Hand => "hand",
            NamedSource::Query => "query",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hand" => Some(NamedSource::Hand),
            "query" => Some(NamedSource::Query),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecayConfig {
    pub half_life_days: f64,
    pub weight: f64,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            half_life_days: 30.0,
            weight: 1.0,
        }
    }
}

impl PartialEq for DecayConfig {
    fn eq(&self, other: &Self) -> bool {
        self.half_life_days == other.half_life_days && self.weight == other.weight
    }
}

/// Kind configuration (spec §9.5). `config_toml` stores the raw TOML received by
/// `kind_register` verbatim; the journal interprets only fixed fields (Not-fat passthrough).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KindConfig {
    pub kind: String,
    pub mode: KindMode,
    /// Only applies to `named_index`. `None` for `entries`.
    pub source: Option<NamedSource>,
    pub path_template: String,
    pub versioning: bool,
    pub indexed: bool,
    pub decay: DecayConfig,
    /// Kind-wide retrieval boost factor (default `1.0`).
    ///
    /// This is a static multiplier that is **independent of `decay.weight`**.
    /// - `decay.weight` determines the shape of the time-decay curve
    ///   (half-life × weight), and is time-dependent.
    /// - `boost_factor` is a time-independent multiplier applied to all entries
    ///   of this kind in the retrieval score.
    ///
    /// Both combine multiplicatively in ST3's retrieval formula:
    /// `retrieval_strength × boost_factor × decay(t)`.
    /// (This field is storage-only in ST2; formula integration is in ST3.)
    pub boost_factor: f64,
    pub tags: Vec<String>,
    pub body_template: Option<String>,
    /// Full raw TOML received by `kind_register` (for opaque passthrough).
    pub config_toml: String,
}

impl KindConfig {
    /// Default preset for `emo` (spec §9).
    pub fn preset_emo() -> Self {
        let base = Self {
            kind: "emo".to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig::default(),
            boost_factor: 1.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        let toml = base.to_config_toml();
        Self {
            config_toml: toml,
            ..base
        }
    }

    /// Default preset for `archive` (spec §9.5 / ST5).
    ///
    /// - `mode = NamedIndex`, `source = Query`
    /// - `path_template = "{persona}/{persona}_archive_index.md"`
    /// - `decay = { half_life_days = 365.0, weight = 0.5 }`
    /// - `versioning = false`, `indexed = true`, `boost_factor = 1.0`
    pub fn preset_archive() -> Self {
        let base = Self {
            kind: "archive".to_string(),
            mode: KindMode::NamedIndex,
            source: Some(NamedSource::Query),
            path_template: "{persona}/{persona}_archive_index.md".to_string(),
            versioning: false,
            indexed: true,
            decay: DecayConfig {
                half_life_days: 365.0,
                weight: 0.5,
            },
            boost_factor: 1.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        let toml = base.to_config_toml();
        Self {
            config_toml: toml,
            ..base
        }
    }

    /// Serialize this `KindConfig` to a canonical single-kind TOML string.
    ///
    /// Output format matches what `parse_kind_toml` accepts.
    /// Decay is omitted when it equals the default (half_life_days=30.0, weight=1.0).
    /// Source and body_template are omitted when None.
    pub fn to_config_toml(&self) -> String {
        let tags_toml: Vec<String> = self.tags.iter().map(|t| format!("\"{t}\"")).collect();
        let tags_str = format!("[{}]", tags_toml.join(", "));
        let mut parts = vec![
            format!("kind = \"{}\"", self.kind),
            format!("mode = \"{}\"", self.mode.as_str()),
        ];
        if let Some(s) = &self.source {
            parts.push(format!("source = \"{}\"", s.as_str()));
        }
        parts.push(format!(
            "path_template = \"{}\"",
            self.path_template
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        ));
        parts.push(format!("versioning = {}", self.versioning));
        parts.push(format!("indexed = {}", self.indexed));
        parts.push(format!("tags = {tags_str}"));
        let decay_half = self.decay.half_life_days;
        let decay_weight = self.decay.weight;
        if decay_half != 30.0 || decay_weight != 1.0 {
            parts.push(format!(
                "decay = {{ half_life_days = {decay_half}, weight = {decay_weight} }}"
            ));
        }
        // Crux C3: omit boost_factor when it equals the default (1.0); emit only when != 1.0.
        if self.boost_factor != 1.0 {
            parts.push(format!("boost_factor = {}", self.boost_factor));
        }
        if let Some(bt) = &self.body_template {
            parts.push(format!(
                "body_template = \"{}\"",
                bt.replace('\\', "\\\\").replace('"', "\\\"")
            ));
        }
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::parse_kind_toml;

    /// T1: happy path — preset_emo round-trips through to_config_toml → parse_kind_toml.
    ///
    /// Verifies Crux #3: KindConfig::to_config_toml output must round-trip through
    /// parse_kind_toml and produce a structurally equal KindConfig.
    #[test]
    fn kind_config_round_trip_preset_emo() {
        let cfg = KindConfig::preset_emo();
        let toml = cfg.to_config_toml();

        // parse_kind_toml must succeed on the generated TOML.
        // Safety: preset_emo produces valid TOML — parse failure would be a bug.
        let parsed = parse_kind_toml(&toml).expect("preset_emo round-trip parse must succeed");

        assert_eq!(parsed.kind, cfg.kind);
        assert_eq!(parsed.mode, cfg.mode);
        assert_eq!(parsed.source, cfg.source);
        assert_eq!(parsed.path_template, cfg.path_template);
        assert_eq!(parsed.versioning, cfg.versioning);
        assert_eq!(parsed.indexed, cfg.indexed);
        assert_eq!(parsed.decay, cfg.decay);
        assert_eq!(parsed.tags, cfg.tags);
        assert_eq!(parsed.body_template, cfg.body_template);

        // Idempotency: re-serializing parsed gives the same TOML string.
        assert_eq!(parsed.to_config_toml(), toml);
    }

    /// T_preset_archive_round_trip: preset_archive round-trips through to_config_toml → parse_kind_toml.
    ///
    /// Verifies:
    /// - decay (365.0, 0.5) appears in TOML (non-default → must be emitted)
    /// - boost_factor=1.0 does NOT appear in TOML (default → must be omitted)
    /// - parse round-trip succeeds
    /// - all fields equal the expected values
    /// - serialization is idempotent
    #[test]
    fn kind_config_round_trip_preset_archive() {
        let cfg = KindConfig::preset_archive();
        let toml = cfg.to_config_toml();

        // decay {365.0, 0.5} is non-default → must appear in TOML output.
        assert!(
            toml.contains("decay"),
            "decay {{365.0, 0.5}} must appear in preset_archive TOML: {toml}"
        );
        // boost_factor=1.0 is the default → must NOT appear in TOML output.
        assert!(
            !toml.contains("boost_factor"),
            "boost_factor=1.0 (default) must NOT appear in preset_archive TOML: {toml}"
        );

        // Safety: preset_archive produces valid TOML — parse failure would be a bug.
        let parsed = parse_kind_toml(&toml).expect("preset_archive round-trip parse must succeed");

        assert_eq!(parsed.kind, cfg.kind);
        assert_eq!(parsed.kind, "archive");
        assert_eq!(parsed.mode, cfg.mode);
        assert_eq!(parsed.source, cfg.source);
        assert_eq!(parsed.path_template, cfg.path_template);
        assert_eq!(parsed.versioning, cfg.versioning);
        assert_eq!(parsed.indexed, cfg.indexed);
        assert_eq!(parsed.decay, cfg.decay);
        assert_eq!(parsed.decay.half_life_days, 365.0);
        assert_eq!(parsed.decay.weight, 0.5);
        assert_eq!(parsed.tags, cfg.tags);
        assert_eq!(parsed.body_template, cfg.body_template);
        assert_eq!(parsed.boost_factor, 1.0);

        // Idempotency: re-serializing parsed gives the same TOML string.
        assert_eq!(parsed.to_config_toml(), toml);
    }

    /// T2: boundary — KindConfig with non-default decay is included in TOML output.
    #[test]
    fn kind_config_round_trip_non_default_decay() {
        let cfg = KindConfig {
            kind: "custom".to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "p/{kind}/{seq:05}.md".to_string(),
            versioning: false,
            indexed: false,
            decay: DecayConfig {
                half_life_days: 7.0,
                weight: 2.0,
            },
            boost_factor: 1.0,
            tags: vec!["a".to_string(), "b".to_string()],
            body_template: None,
            config_toml: String::new(),
        };
        let toml = cfg.to_config_toml();
        // Non-default decay must appear in the output.
        assert!(
            toml.contains("decay"),
            "expected decay line in TOML: {toml}"
        );

        let parsed = parse_kind_toml(&toml).expect("non-default decay round-trip must succeed");
        assert_eq!(parsed.decay, cfg.decay);
        assert_eq!(parsed.tags, cfg.tags);
        assert_eq!(parsed.to_config_toml(), toml);
    }

    /// T3: boundary — to_config_toml with default decay omits the decay line.
    #[test]
    fn kind_config_default_decay_omitted() {
        let cfg = KindConfig {
            kind: "simple".to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "p/{kind}/{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig::default(),
            boost_factor: 1.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        let toml = cfg.to_config_toml();
        // The literal string "decay = " must not appear when decay is default.
        assert!(
            !toml.contains("decay = "),
            "default decay must be omitted from TOML: {toml}"
        );
        // Crux C3: boost_factor must be omitted when it equals 1.0 (the default).
        assert!(
            !toml.contains("boost_factor"),
            "default boost_factor (1.0) must be omitted from TOML: {toml}"
        );
    }

    /// T9 (crux C3): boost_factor round-trip — non-default value is emitted and parsed back;
    /// default value (1.0) is omitted from TOML output.
    #[test]
    fn kind_config_round_trip_with_boost_factor() {
        // Non-default boost_factor is emitted and parses back correctly.
        let cfg = KindConfig {
            kind: "boosted".to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "p/{kind}/{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig::default(),
            boost_factor: 2.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        let toml = cfg.to_config_toml();
        // boost_factor = 2.0 must appear in TOML.
        assert!(
            toml.contains("boost_factor"),
            "boost_factor != 1.0 must appear in TOML: {toml}"
        );
        // Safety: valid TOML produced by to_config_toml; parse failure is a bug.
        let parsed = parse_kind_toml(&toml).expect("round-trip parse must succeed");
        assert_eq!(
            parsed.boost_factor, 2.0,
            "parsed boost_factor must equal 2.0, got {}",
            parsed.boost_factor
        );

        // Default boost_factor (1.0) is omitted from TOML output.
        let cfg_default = KindConfig {
            boost_factor: 1.0,
            ..cfg.clone()
        };
        let toml_default = cfg_default.to_config_toml();
        assert!(
            !toml_default.contains("boost_factor"),
            "boost_factor == 1.0 must be omitted from TOML: {toml_default}"
        );
        // Parsing TOML without boost_factor key must restore 1.0.
        let parsed_default =
            parse_kind_toml(&toml_default).expect("default round-trip parse must succeed");
        assert_eq!(
            parsed_default.boost_factor, 1.0,
            "absent boost_factor key must restore 1.0, got {}",
            parsed_default.boost_factor
        );
    }
}
