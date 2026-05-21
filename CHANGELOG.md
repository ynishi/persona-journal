# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [0.1.0] - 2026-05-21

### Added

- **`persona-journal-core` crate** — new workspace member (`crates/persona-journal-core`) that
  holds all pure-parse / I/O-free logic: `schema` (`KindConfig` / `KindMode` / `NamedSource` /
  `DecayConfig`), `loader` (`parse_kind_toml` / `parse_journal_toml`), `storage` pure helpers
  (`entry_id` / `flat_path` / `versioned_path` / `extract_first_line`), and `error`
  (`CoreError` enum + `Result<T>` type alias). `rusqlite` is intentionally excluded; the crate
  has no direct or transitive dependency on it.
- **`pub fn Journal::register_kind_from_toml(&self, persona: &str, toml_src: &str) -> Result<KindConfig>`**
  — unified entry-point for TOML→kind registration. Parses the TOML string via
  `persona_journal_core::loader::parse_kind_toml`, delegates to `self.kind_register`, and
  returns the registered `KindConfig`. The `journal_kind_register` MCP handler now routes
  exclusively through this method; zero crate-external direct callers of
  `loader::parse_kind_toml` remain.
- **`KindConfig::to_config_toml(&self) -> String`** — canonical TOML serializer for
  `KindConfig` (defined in `persona-journal-core`). Replaces the hand-assembled string
  construction that was in `loader.rs`. Output deterministically round-trips through
  `parse_kind_toml`: `parse_kind_toml(&cfg.to_config_toml())` produces a structurally
  equal `KindConfig`, and a second `to_config_toml()` call on the parsed value returns
  an identical string (idempotent).
- **Round-trip test `kind_config_round_trip_preset_emo`** in `persona-journal-core` — verifies
  `KindConfig::preset_emo()` → `to_config_toml()` → `parse_kind_toml()` field-for-field
  equality and idempotency of the serializer output.
- **`.journal.toml` loader** — `<root>/<persona>/.journal.toml` is scanned on first
  `open_db` per persona (and on explicit `reload_kinds`). Uses `[[kinds]]`
  array-of-tables format. Insert-if-absent semantics: existing kinds registered via
  `journal_kind_register` are never overwritten. Silent skip if file is absent.
- **`pub fn Journal::reload_kinds(&self, persona: &str) -> Result<usize>`** — explicit
  reload API. Clears the per-persona "already loaded" flag and re-scans
  `.journal.toml`. Returns the number of kinds newly inserted. Existing kinds are
  preserved (insert-if-absent). Returns `Ok(0)` if `.journal.toml` is absent.
- **MCP tool `journal_reload_kinds`** — `{ persona, root? }` → `{"reloaded": N}`.
  Annotations: `open_world_hint = false` (same as all other tools). `idempotent_hint`
  not set (disk content may change between calls).
- **`pub fn loader::parse_kind_toml(src: &str) -> Result<KindConfig>`** — moved from
  `persona-journal-mcp` crate; now the single source of truth for kind TOML parsing.
- **`pub fn loader::parse_journal_toml(src: &str) -> Result<Vec<KindConfig>>`** — new
  function that parses a full `.journal.toml` (`[[kinds]]` array-of-tables).
- `versions.body TEXT NOT NULL` column (DB-side body storage).
- `Db::version_body` / `Db::list_all_versions` methods.

### Changed

- **Internal: `Db::insert_entry_in_tx` / `Db::add_version_in_tx` private helpers extracted** —
  duplicate SQL literals previously inlined in both `insert_entry` / `add_version` and
  `say_atomic` are consolidated into two private helpers. Both helpers receive `&Transaction`
  as a parameter and never open or close a transaction themselves, preserving the single-Tx
  atomicity guarantee of `say_atomic`. No change to the `say_atomic` public signature.
- **`Db::add_version` signature: `body: &str` parameter added** — the previous signature
  omitted `body`, causing a `NOT NULL` constraint violation on the `versions.body` column for
  any direct caller. With 0 external callers this was a dead/broken API; the parameter is
  added to make the function callable. `say_atomic` behaviour and the MCP surface are
  unchanged.
- **Internal: `ensure_loaded` / `reload_kinds` unified** — both methods now delegate to a
  private helper `Journal::load_journal_toml_kinds(persona, force_reload: bool)`. The
  observable behaviour (insert-if-absent semantics, `Ok(0)` on absent file, LOADED-flag
  semantics, Mutex lock ordering) is unchanged.
- **`loader::parse_kind_toml` no longer called directly by `persona-journal-mcp`** — the MCP
  `kind_register` handler uses `Journal::register_kind_from_toml` exclusively. The function
  remains publicly accessible via `persona_journal::loader::parse_kind_toml` for backward
  compatibility; MCP-internal direct callers are reduced to zero.
- **`parse_journal_toml` internal serialization** — `config_toml` field is now constructed by
  `cfg.to_config_toml()` rather than a hand-assembled format string (no observable difference
  in stored value).
- **`KindConfig::preset_emo` rebuilt via `to_config_toml`** — `config_toml` literal is now
  derived from the canonical serializer at construction time, ensuring consistency with any
  kind registered through other code paths.
- **`parse_kind_toml` moved to `persona-journal` crate** (`crates/persona-journal/src/loader.rs`).
  Return type changed from `anyhow::Result<KindConfig>` to `persona_journal::Result<KindConfig>`.
  The `journal_kind_register` MCP handler uses `loader::parse_kind_toml` via the public API.
- **MCP tool `journal_index_rebuild` renamed to `journal_projection_rebuild`**.
- **CLI subcommand `index-rebuild` renamed to `projection-rebuild`**.
- **`Journal::index_rebuild` renamed to `Journal::projection_rebuild`** — now regenerates
  entry `.md` and `_index.md` from DB SoT.
- **DB is now the SoT for entry body** — FS `.md` files are a read projection.
- **Doc: `# Concurrency` section added to `kind_list` and `kind_get`** — Both methods
  were missing the `# Concurrency` rustdoc section that all other public `Journal`
  methods carry. The section text follows the same pattern as `kind_register`: acquires
  `Mutex<HashMap>` and `Mutex<Db>` sequentially, neither guard crosses an `.await` point.
- **Doc: `Journal::open` canonicalization note** — Added a `# Notes` section explaining
  that callers are encouraged to pass a pre-canonicalized path and that `db_cache`
  canonicalizes internally with a raw-path fallback.
- **Workspace `rustfmt` applied** — `cargo fmt --workspace` applied to five files with
  pre-existing formatting violations (`projection.rs`, `schema.rs`, `storage.rs`,
  `persona-journal-mcp/src/lib.rs`, `persona-journal-mcp/src/main.rs`). No logic changes.

### Fixed

- **`db_cache` path canonicalization** — `db_cache::get_or_open` now resolves `root`
  via `std::fs::canonicalize` before constructing the `(PathBuf, String)` cache key and
  the `db_path`. If canonicalization fails (e.g. the root directory does not yet exist),
  the raw path is used as a fallback, preserving the previous behaviour. This ensures
  symlink variants and relative-path variants of the same root directory map to a single
  cache entry rather than creating duplicate `Arc<Mutex<Db>>` instances.
- **`KindConfig.tags` DB persistence** — `kinds` table now has a `tags TEXT NOT NULL DEFAULT '[]'`
  column. `upsert_kind` serializes `KindConfig.tags` to JSON and persists it; `get_kind`
  deserializes and returns the stored value. The previous hard-coded `tags: vec![]` fallback in
  `db.rs` is removed, so registered tags survive across process restarts.
- **`say` DB-first write with best-effort FS projection** — `Db::say_atomic` now stores the
  entry body in `versions.body` and commits the DB transaction before touching the filesystem.
  After the Tx commits, `Journal::say` writes the FS `.md` projection as a best-effort step:
  a write failure is logged via `tracing::warn` and does not roll back the DB — the entry is
  safely recorded in SQLite. Use `journal_projection_rebuild` to repair stale `.md` files.

### Performance

- **Per-persona Db connection cache** — `Journal::open_db` now returns a cached
  `Arc<Mutex<Db>>` for each `(root, persona)` pair via a process-global
  `OnceLock<Mutex<HashMap<(PathBuf, String), Arc<Mutex<Db>>>>>`. In long-running MCP
  processes (e.g. Claude Desktop), repeated tool calls for the same persona reuse the
  existing SQLite connection instead of opening a new one per call. The public signature
  of `Journal::open` is unchanged.

### Tests

- **E2E MCP stdio JSON-RPC tests for `persona-journal-mcp`** —
  `crates/persona-journal-mcp/tests/e2e_mcp.rs` (new file) spawns the real binary via
  `std::process::Command` and drives it over stdio JSON-RPC. Covers: `tools/list` (all 7
  tools advertised), `journal_kind_register` tags round-trip, `journal_kind_list`,
  `.journal.toml` auto-load, and `journal_reload_kinds`. Wire layer verification ensures
  serialization/deserialization fidelity that unit tests cannot catch.

### Cleanup

- **Removed dead-code shim** — `_ensure_error_used` in `db.rs` (the `#[allow(dead_code)]`
  function that existed only to satisfy an early lint workaround) is deleted. `Error` is
  already referenced throughout the codebase via `Result<T, Error>`.
