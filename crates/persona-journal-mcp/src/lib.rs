//! MCP service exposing persona-journal write / read / kind tools.

use std::path::PathBuf;

use persona_journal::{FilterMode, Journal};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ServerHandler,
};
use serde::{Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Clone)]
pub struct JournalService {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    default_root: PathBuf,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SayParams {
    /// Persona id (matches persona-pack id).
    pub persona: String,
    /// Kind name (e.g. "emo"). Must be registered for the persona.
    pub kind: String,
    /// Entry body. First non-empty line (`# ...` ok) becomes the summary.
    pub text: String,
    /// Optional tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional override of journal root.
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryLatestParams {
    /// Persona id (matches persona-pack id).
    pub persona: String,
    /// Kind selector. One of:
    /// - single kind name: `"state"`
    /// - comma-separated list: `"state,memory,emo"` (whitespace around tokens is trimmed)
    /// - `"all"` — every kind registered for the persona
    pub kind: String,
    /// Max rows per kind. Defaults to 10.
    pub count: Option<usize>,
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntryReadParams {
    pub persona: String,
    /// Entry uname (e.g. `"emo/2024-08_000012"`). Used as the lookup key.
    pub id: String,
    /// Optional version. Defaults to current.
    pub version: Option<u32>,
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KindRegisterParams {
    pub persona: String,
    /// Full TOML body of the kind config (§9.5).
    pub config_toml: String,
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KindListParams {
    pub persona: String,
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectionRebuildParams {
    pub persona: String,
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReloadKindsParams {
    pub persona: String,
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryByRetrievalParams {
    /// Persona id (matches persona-pack id).
    pub persona: String,
    /// Kind name (e.g. "emo").
    pub kind: String,
    /// Max number of entries to return. Defaults to 10.
    pub n: Option<usize>,
    /// Reference time for decay calculation (RFC 3339). Defaults to now (UTC).
    pub now: Option<String>,
    /// Optional override of journal root.
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FilterParams {
    /// Persona id (matches persona-pack id).
    pub persona: String,
    /// Kind name (e.g. "emo").
    pub kind: String,
    /// Filter mode. Use `{"type":"visible","threshold":0.5}` etc.
    pub mode: FilterModeInput,
    /// Reference time for decay calculation (RFC 3339). Defaults to now (UTC).
    pub now: Option<String>,
    /// Optional override of journal root.
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PinParams {
    /// Persona id (matches persona-pack id).
    pub persona: String,
    /// Entry uname (e.g. `"emo/2024-08_000012"`).
    pub entry_id: String,
    /// Retrieval strength to pin to. Defaults to 1.0 (neutral) when omitted.
    pub strength: Option<f64>,
    /// Optional override of journal root.
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnpinParams {
    /// Persona id (matches persona-pack id).
    pub persona: String,
    /// Entry uname (e.g. `"emo/2024-08_000012"`).
    pub entry_id: String,
    /// Optional override of journal root.
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BoostKindParams {
    /// Persona id (matches persona-pack id).
    pub persona: String,
    /// Kind name (e.g. "emo").
    pub kind: String,
    /// Boost factor (must be > 0.0 and not NaN).
    pub factor: f64,
    /// Optional override of journal root.
    pub root: Option<String>,
}

/// Filter mode for `journal_filter`. Use `{"type":"visible","threshold":0.5}` etc.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FilterModeInput {
    /// Include entries with retrieval score >= threshold.
    Visible { threshold: f64 },
    /// Include entries with retrieval score < threshold.
    Archive { threshold: f64 },
    /// Top-k entries above threshold.
    Partial { threshold: f64, top_k: usize },
    /// All entries, score DESC.
    Full {},
}

#[derive(Debug, Serialize)]
struct SayResult {
    /// Entry uname in `{kind}/{ym}_{seq:06}` format.
    id: String,
}

#[derive(Debug, Serialize)]
struct EntryRowOut {
    /// Entry uname in `{kind}/{ym}_{seq:06}` format.
    id: String,
    kind: String,
    created_at: String,
    updated_at: String,
    current_version: u32,
    tags: Vec<String>,
    summary: Option<String>,
    retrieval_strength: f64,
}

impl From<persona_journal::EntryRow> for EntryRowOut {
    fn from(r: persona_journal::EntryRow) -> Self {
        Self {
            id: r.id,
            kind: r.kind,
            created_at: r.created_at,
            updated_at: r.updated_at,
            current_version: r.current_version,
            tags: r.tags,
            summary: r.summary,
            retrieval_strength: r.retrieval_strength,
        }
    }
}

#[tool_router]
impl JournalService {
    pub fn new(default_root: PathBuf) -> Self {
        Self {
            tool_router: Self::tool_router(),
            default_root,
        }
    }

    fn journal(&self, override_root: Option<String>) -> Journal {
        let root = match override_root {
            Some(p) => PathBuf::from(p),
            None => self.default_root.clone(),
        };
        Journal::open(root)
    }

    /// Append a new entry. Returns `{ "id": "<kind>/<ym>_<seq:06>" }` (e.g. `"emo/2024-08_000012"`).
    /// Auto-registers the `emo` preset kind on first use if no kinds exist.
    ///
    /// Mode-aware: dispatches to entries or named_index writer based on the
    /// registered kind's mode. `tags` are persisted for entries mode only
    /// (named_index does not currently persist tags).
    #[tool(name = "journal_say", annotations(open_world_hint = false))]
    async fn say(&self, Parameters(p): Parameters<SayParams>) -> Result<String, String> {
        let j = self.journal(p.root);
        j.ensure_default_kinds(&p.persona)
            .map_err(|e| e.to_string())?;
        let id = j
            .say_any(&p.persona, &p.kind, &p.text, p.tags)
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&SayResult { id }).map_err(|e| e.to_string())
    }

    /// Query latest entries (DESC by created_at), grouped by kind.
    ///
    /// The `kind` parameter accepts three forms:
    /// - single kind: `"state"` — returns `{"state": [<row>, ...]}`
    /// - comma-separated list: `"state,memory,emo"` — returns one key per kind
    /// - `"all"` — returns one key per kind registered for the persona
    ///
    /// Always returns a JSON **object** keyed by kind name (uniform shape across
    /// single / multi / all). Whitespace around comma-separated tokens is trimmed.
    #[tool(name = "journal_query_latest", annotations(open_world_hint = false))]
    async fn query_latest(
        &self,
        Parameters(p): Parameters<QueryLatestParams>,
    ) -> Result<String, String> {
        let j = self.journal(p.root);
        let kinds = resolve_kinds(&j, &p.persona, &p.kind)?;
        let n = p.count.unwrap_or(10);
        let mut out: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        for k in &kinds {
            let rows = j
                .query_latest(&p.persona, k, n)
                .map_err(|e| e.to_string())?;
            let rows_out: Vec<EntryRowOut> = rows.into_iter().map(EntryRowOut::from).collect();
            out.insert(
                k.clone(),
                serde_json::to_value(rows_out).map_err(|e| e.to_string())?,
            );
        }
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// Read an entry body. Optional version selects a historical snapshot.
    #[tool(name = "journal_entry_read", annotations(open_world_hint = false))]
    async fn entry_read(
        &self,
        Parameters(p): Parameters<EntryReadParams>,
    ) -> Result<String, String> {
        let j = self.journal(p.root);
        j.entry_read(&p.persona, &p.id, p.version)
            .map_err(|e| e.to_string())
    }

    /// Register (or replace) a kind config for a persona. Body is a TOML doc (§9.5).
    #[tool(name = "journal_kind_register", annotations(open_world_hint = false))]
    async fn kind_register(
        &self,
        Parameters(p): Parameters<KindRegisterParams>,
    ) -> Result<String, String> {
        let j = self.journal(p.root);
        let cfg = j
            .register_kind_from_toml(&p.persona, &p.config_toml)
            .map_err(|e| e.to_string())?;
        Ok(format!("{{\"kind\":\"{}\"}}", cfg.kind))
    }

    /// List registered kinds for a persona.
    #[tool(name = "journal_kind_list", annotations(open_world_hint = false))]
    async fn kind_list(&self, Parameters(p): Parameters<KindListParams>) -> Result<String, String> {
        let j = self.journal(p.root);
        let kinds = j.kind_list(&p.persona).map_err(|e| e.to_string())?;
        serde_json::to_string(&kinds).map_err(|e| e.to_string())
    }

    /// Rebuild FS projection (entry .md + `_index.md`) from DB SoT for a persona.
    #[tool(
        name = "journal_projection_rebuild",
        annotations(open_world_hint = false)
    )]
    async fn projection_rebuild(
        &self,
        Parameters(p): Parameters<ProjectionRebuildParams>,
    ) -> Result<String, String> {
        let j = self.journal(p.root);
        j.projection_rebuild(&p.persona)
            .map_err(|e| e.to_string())?;
        Ok("{\"ok\":true}".to_string())
    }

    /// Re-scan `<root>/<persona>/.journal.toml` and insert any new kinds (insert-if-absent).
    /// Existing kinds are not overwritten.  Returns `{"reloaded": N}` where N is the
    /// number of kinds newly inserted.
    #[tool(name = "journal_reload_kinds", annotations(open_world_hint = false))]
    async fn reload_kinds(
        &self,
        Parameters(p): Parameters<ReloadKindsParams>,
    ) -> Result<String, String> {
        let j = self.journal(p.root);
        let n = j.reload_kinds(&p.persona).map_err(|e| e.to_string())?;
        Ok(format!("{{\"reloaded\":{n}}}"))
    }

    /// Query entries ranked by retrieval strength (decay-weighted). Returns up to `n` rows
    /// ordered by score DESC. `now` defaults to current UTC time when omitted.
    #[tool(
        name = "journal_query_by_retrieval",
        annotations(open_world_hint = false)
    )]
    async fn query_by_retrieval(
        &self,
        Parameters(p): Parameters<QueryByRetrievalParams>,
    ) -> Result<String, String> {
        let j = self.journal(p.root);
        let now = parse_now_or_default(p.now)?;
        let rows = j
            .query_by_retrieval(&p.persona, &p.kind, p.n.unwrap_or(10), now)
            .map_err(|e| e.to_string())?;
        let out: Vec<EntryRowOut> = rows.into_iter().map(EntryRowOut::from).collect();
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// Filter entries by retrieval strength using one of four modes (Visible / Archive /
    /// Partial / Full). `now` defaults to current UTC time when omitted.
    #[tool(name = "journal_filter", annotations(open_world_hint = false))]
    async fn filter(&self, Parameters(p): Parameters<FilterParams>) -> Result<String, String> {
        let j = self.journal(p.root);
        let now = parse_now_or_default(p.now)?;
        let mode = filter_mode_input_to_core(p.mode);
        let rows = j
            .filter(&p.persona, &p.kind, mode, now)
            .map_err(|e| e.to_string())?;
        let out: Vec<EntryRowOut> = rows.into_iter().map(EntryRowOut::from).collect();
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// Pin an entry by setting its `retrieval_strength`. Defaults to 1.0 when
    /// `strength` is omitted. Returns `{"ok":true}` on success.
    #[tool(name = "journal_pin", annotations(open_world_hint = false))]
    async fn pin(&self, Parameters(p): Parameters<PinParams>) -> Result<String, String> {
        let j = self.journal(p.root);
        j.pin(&p.persona, &p.entry_id, p.strength)
            .map_err(|e| e.to_string())?;
        Ok("{\"ok\":true}".to_string())
    }

    /// Unpin an entry by resetting its `retrieval_strength` to 1.0 (neutral).
    /// Returns `{"ok":true}` on success.
    #[tool(name = "journal_unpin", annotations(open_world_hint = false))]
    async fn unpin(&self, Parameters(p): Parameters<UnpinParams>) -> Result<String, String> {
        let j = self.journal(p.root);
        j.unpin(&p.persona, &p.entry_id)
            .map_err(|e| e.to_string())?;
        Ok("{\"ok\":true}".to_string())
    }

    /// Set a kind-wide retrieval boost factor (must be > 0.0 and not NaN).
    /// Returns `{"ok":true}` on success.
    #[tool(name = "journal_boost_kind", annotations(open_world_hint = false))]
    async fn boost_kind(
        &self,
        Parameters(p): Parameters<BoostKindParams>,
    ) -> Result<String, String> {
        let j = self.journal(p.root);
        j.boost_kind(&p.persona, &p.kind, p.factor)
            .map_err(|e| e.to_string())?;
        Ok("{\"ok\":true}".to_string())
    }
}

/// Resolve the `kind` parameter accepted by `journal_query_latest` into a list
/// of concrete kind names.
///
/// - `"all"` — every kind registered for the persona (via `kind_list`)
/// - comma-separated (`"a,b,c"`) — delegated to [`parse_kind_list`]
/// - single non-empty token — single-element vec
/// - empty / whitespace-only — error
fn resolve_kinds(j: &Journal, persona: &str, kind: &str) -> Result<Vec<String>, String> {
    let trimmed = kind.trim();
    if trimmed.is_empty() {
        return Err("kind parameter is empty".to_string());
    }
    if trimmed == "all" {
        let configs = j.kind_list(persona).map_err(|e| e.to_string())?;
        let kinds: Vec<String> = configs.into_iter().map(|c| c.kind).collect();
        if kinds.is_empty() {
            return Err(format!("no kinds registered for persona '{persona}'"));
        }
        return Ok(kinds);
    }
    parse_kind_list(trimmed)
}

/// Pure parser for the non-`"all"` form: single token or comma-separated list.
/// Whitespace around tokens is trimmed; empty tokens are dropped. Returns an
/// error if every token is empty (e.g. input was `",,,"`).
fn parse_kind_list(input: &str) -> Result<Vec<String>, String> {
    if !input.contains(',') {
        return Ok(vec![input.to_string()]);
    }
    let kinds: Vec<String> = input
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if kinds.is_empty() {
        Err("kind parameter has no non-empty tokens after trimming".to_string())
    } else {
        Ok(kinds)
    }
}

fn parse_now_or_default(now: Option<String>) -> Result<OffsetDateTime, String> {
    match now {
        None => Ok(OffsetDateTime::now_utc()),
        Some(s) => {
            OffsetDateTime::parse(&s, &Rfc3339).map_err(|e| format!("invalid 'now' (RFC3339): {e}"))
        }
    }
}

fn filter_mode_input_to_core(input: FilterModeInput) -> FilterMode {
    match input {
        FilterModeInput::Visible { threshold } => FilterMode::Visible { threshold },
        FilterModeInput::Archive { threshold } => FilterMode::Archive { threshold },
        FilterModeInput::Partial { threshold, top_k } => FilterMode::Partial { threshold, top_k },
        FilterModeInput::Full {} => FilterMode::Full,
    }
}

#[tool_handler]
impl ServerHandler for JournalService {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "persona-journal — local-first diary. Tools: journal_say / \
             journal_query_latest / journal_entry_read / journal_kind_register / \
             journal_kind_list / journal_projection_rebuild / journal_reload_kinds / \
             journal_query_by_retrieval / journal_filter / journal_pin / \
             journal_unpin / journal_boost_kind."
                .to_string(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kind_list_single_token() {
        assert_eq!(parse_kind_list("state").unwrap(), vec!["state".to_string()]);
    }

    #[test]
    fn parse_kind_list_comma_separated() {
        assert_eq!(
            parse_kind_list("state,memory,emo").unwrap(),
            vec!["state".to_string(), "memory".to_string(), "emo".to_string()]
        );
    }

    #[test]
    fn parse_kind_list_trims_whitespace() {
        assert_eq!(
            parse_kind_list("state, memory ,  emo").unwrap(),
            vec!["state".to_string(), "memory".to_string(), "emo".to_string()]
        );
    }

    #[test]
    fn parse_kind_list_drops_empty_tokens() {
        assert_eq!(
            parse_kind_list("state,,memory").unwrap(),
            vec!["state".to_string(), "memory".to_string()]
        );
    }

    #[test]
    fn parse_kind_list_only_commas_errors() {
        assert!(parse_kind_list(",,,").is_err());
    }

    fn open_tmp_journal() -> (tempfile::TempDir, Journal) {
        let tmp = tempfile::tempdir().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        (tmp, j)
    }

    #[test]
    fn resolve_kinds_single_passes_through() {
        let (_tmp, j) = open_tmp_journal();
        let r = resolve_kinds(&j, "anyone", "state").unwrap();
        assert_eq!(r, vec!["state".to_string()]);
    }

    #[test]
    fn resolve_kinds_comma_sep_passes_through() {
        let (_tmp, j) = open_tmp_journal();
        let r = resolve_kinds(&j, "anyone", "state, memory, emo").unwrap();
        assert_eq!(
            r,
            vec!["state".to_string(), "memory".to_string(), "emo".to_string()]
        );
    }

    #[test]
    fn resolve_kinds_empty_errors() {
        let (_tmp, j) = open_tmp_journal();
        assert!(resolve_kinds(&j, "anyone", "").is_err());
        assert!(resolve_kinds(&j, "anyone", "   ").is_err());
    }

    #[test]
    fn resolve_kinds_all_returns_registered_kinds() {
        let (_tmp, j) = open_tmp_journal();
        let persona = "shi";
        let toml_for = |kind: &str| {
            format!(
                "kind = \"{kind}\"\nmode = \"entries\"\npath_template = \"{{persona}}/{kind}/{{persona}}_{kind}_{{yyyy}}-{{mm}}_{{seq:05}}.md\"\n"
            )
        };
        j.register_kind_from_toml(persona, &toml_for("alpha"))
            .unwrap();
        j.register_kind_from_toml(persona, &toml_for("beta"))
            .unwrap();
        let mut kinds = resolve_kinds(&j, persona, "all").unwrap();
        kinds.sort();
        assert_eq!(kinds, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn resolve_kinds_all_errors_when_no_kinds_registered() {
        let (_tmp, j) = open_tmp_journal();
        assert!(resolve_kinds(&j, "ghost", "all").is_err());
    }
}
