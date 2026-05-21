//! MCP service exposing persona-journal write / read / kind tools.

use std::path::PathBuf;

use persona_journal::Journal;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ServerHandler,
};
use serde::{Deserialize, Serialize};

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
    pub persona: String,
    pub kind: String,
    /// Max rows. Defaults to 10.
    pub count: Option<usize>,
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntryReadParams {
    pub persona: String,
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

#[derive(Debug, Serialize)]
struct SayResult {
    id: String,
}

#[derive(Debug, Serialize)]
struct EntryRowOut {
    id: String,
    kind: String,
    created_at: String,
    updated_at: String,
    current_version: u32,
    tags: Vec<String>,
    summary: Option<String>,
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

    /// Append a new entry. Returns `{ "id": "<YYYY-MM_NNNNN>" }`.
    /// Auto-registers the `emo` preset kind on first use if no kinds exist.
    #[tool(name = "journal_say", annotations(open_world_hint = false))]
    async fn say(&self, Parameters(p): Parameters<SayParams>) -> Result<String, String> {
        let j = self.journal(p.root);
        j.ensure_default_kinds(&p.persona)
            .map_err(|e| e.to_string())?;
        let id = j
            .say(&p.persona, &p.kind, &p.text, p.tags)
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&SayResult { id }).map_err(|e| e.to_string())
    }

    /// Query latest entries of a kind (DESC by created_at).
    #[tool(name = "journal_query_latest", annotations(open_world_hint = false))]
    async fn query_latest(
        &self,
        Parameters(p): Parameters<QueryLatestParams>,
    ) -> Result<String, String> {
        let j = self.journal(p.root);
        let rows = j
            .query_latest(&p.persona, &p.kind, p.count.unwrap_or(10))
            .map_err(|e| e.to_string())?;
        let out: Vec<EntryRowOut> = rows.into_iter().map(EntryRowOut::from).collect();
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
}

#[tool_handler]
impl ServerHandler for JournalService {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "persona-journal — local-first diary. Tools: journal_say / \
             journal_query_latest / journal_entry_read / journal_kind_register / \
             journal_kind_list / journal_projection_rebuild / journal_reload_kinds."
                .to_string(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}
