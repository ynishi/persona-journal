//! High-level API. Open a `Journal` at `<root>` and call `say` / `query_latest` / ...
//!
//! DB is per-persona: `<root>/<persona>/_journal.db`. Opened on first access per
//! `(root, persona)` pair and cached process-globally in `db_cache::CACHE`.
//!
//! A second process-global cache `loaded_cache::LOADED` tracks which
//! `(canonical_root, persona)` pairs have already had their `.journal.toml`
//! loaded, so the loader runs at most once per pair per process lifetime.

use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::db::{Db, EntryMetaRow};
use crate::error::{Error, Result};
use crate::projection::{render_root_index, write_root_index};
use crate::schema::{KindConfig, KindMode};
use crate::storage::{entry_id, extract_first_line, flat_path, versioned_path, write_file};

pub(crate) mod db_cache {
    use super::*;
    use std::collections::HashMap;
    use std::sync::OnceLock;

    type Key = (PathBuf, String);
    type Value = Arc<Mutex<Db>>;

    static CACHE: OnceLock<Mutex<HashMap<Key, Value>>> = OnceLock::new();

    fn cache() -> &'static Mutex<HashMap<Key, Value>> {
        CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(crate) fn get_or_open(root: &Path, persona: &str) -> Result<Value> {
        // Fallback to raw path: canonicalize fails when root does not yet exist; Db::open will create it.
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let key = (canonical_root.clone(), persona.to_string());
        let mut map = cache()
            .lock()
            .map_err(|e| Error::Invalid(format!("cache lock poisoned: {e}")))?;
        if let Some(v) = map.get(&key) {
            return Ok(Arc::clone(v));
        }
        let db_path = canonical_root.join(persona).join("_journal.db");
        let db = Db::open(&db_path)?;
        let arc = Arc::new(Mutex::new(db));
        map.insert(key, Arc::clone(&arc));
        Ok(arc)
    }
}

/// Process-global cache tracking which `(canonical_root, persona)` pairs have
/// already had their `.journal.toml` loaded.  The loader runs at most once per
/// pair per process lifetime (unless explicitly reset by `reload_kinds`).
pub(crate) mod loaded_cache {
    use super::*;
    use std::collections::HashSet;
    use std::sync::OnceLock;

    type Key = (PathBuf, String);

    static LOADED: OnceLock<Mutex<HashSet<Key>>> = OnceLock::new();

    fn loaded() -> &'static Mutex<HashSet<Key>> {
        LOADED.get_or_init(|| Mutex::new(HashSet::new()))
    }

    /// Insert `(canonical_root, persona)` into the set.
    /// Returns `true` if this is a newly inserted entry (not-yet-loaded),
    /// `false` if it was already present (already loaded).
    pub(crate) fn mark(canonical_root: PathBuf, persona: &str) -> Result<bool> {
        let key = (canonical_root, persona.to_string());
        let mut set = loaded()
            .lock()
            .map_err(|e| Error::Invalid(format!("loaded cache lock poisoned: {e}")))?;
        Ok(set.insert(key))
    }

    /// Returns `true` if `(canonical_root, persona)` is already in the set.
    pub(crate) fn contains(canonical_root: &Path, persona: &str) -> Result<bool> {
        let key = (canonical_root.to_path_buf(), persona.to_string());
        let set = loaded()
            .lock()
            .map_err(|e| Error::Invalid(format!("loaded cache lock poisoned: {e}")))?;
        Ok(set.contains(&key))
    }

    /// Remove `(canonical_root, persona)` from the set (used by `reload_kinds`).
    pub(crate) fn remove(canonical_root: &Path, persona: &str) -> Result<()> {
        let key = (canonical_root.to_path_buf(), persona.to_string());
        let mut set = loaded()
            .lock()
            .map_err(|e| Error::Invalid(format!("loaded cache lock poisoned: {e}")))?;
        set.remove(&key);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct EntryRow {
    pub id: String,
    pub kind: String,
    pub created_at: String,
    pub updated_at: String,
    pub current_version: u32,
    pub tags: Vec<String>,
    pub summary: Option<String>,
    pub retrieval_strength: f64,
}

impl From<EntryMetaRow> for EntryRow {
    fn from(r: EntryMetaRow) -> Self {
        Self {
            id: r.id,
            kind: r.kind,
            created_at: r.created_at,
            updated_at: r.updated_at,
            current_version: r.current_version,
            tags: r.tags,
            summary: r.first_line_cache,
            retrieval_strength: r.retrieval_strength,
        }
    }
}

/// Retrieval-strength filter mode for [`Journal::filter`].
///
/// Defines how entries are selected based on their computed score
/// (retrieval_strength × boost_factor × decay_weight × exp(-ln2 × age / half_life)).
///
/// Invariants:
/// - `Visible(t) ⊕ Archive(t) = Full` (disjoint partition)
/// - `Partial(t, k) ⊇ Visible(t)` (Partial always includes every Visible entry)
/// - `Full == query_by_retrieval(n=usize::MAX)` (same ordered set)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterMode {
    /// Entries with score >= threshold (above the visibility line).
    Visible { threshold: f64 },
    /// Union of Visible and top-k entries by score DESC.
    /// Always contains at least every Visible entry.
    Partial { threshold: f64, top_k: usize },
    /// Entries with score < threshold (below the visibility line).
    Archive { threshold: f64 },
    /// All entries ordered by score DESC (equivalent to query_by_retrieval(n=total)).
    Full,
}

pub struct Journal {
    root: PathBuf,
}

impl Journal {
    /// Open a `Journal` rooted at `root`.
    ///
    /// This call is cheap: no file I/O is performed. DB connections are opened
    /// lazily on the first call per persona and cached process-globally in
    /// `db_cache::CACHE`.
    ///
    /// # Notes
    /// Callers are encouraged to pass a canonicalized `PathBuf` (e.g. via
    /// `std::fs::canonicalize`) when the root is known to exist, so that symlink
    /// or relative-path variants resolve to the same DB cache entry. The cache
    /// also canonicalizes internally with a raw-path fallback.
    ///
    /// # Concurrency
    /// `Journal` is `Send`. Multiple `Journal` instances backed by the same
    /// `root` safely share the same underlying `Arc<Mutex<Db>>` entries via the
    /// global cache. Callers running in a `tokio` multi-thread runtime must not
    /// hold any `MutexGuard` (obtained from `open_db`) across an `.await` point,
    /// as this can deadlock the runtime.
    pub fn open(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn open_db(&self, persona: &str) -> Result<Arc<Mutex<Db>>> {
        let arc = db_cache::get_or_open(&self.root, persona)?;
        self.ensure_loaded(persona)?;
        Ok(arc)
    }

    /// Shared implementation for `ensure_loaded` and `reload_kinds`.
    ///
    /// `force_reload = false` → ensure-loaded semantics: skip if already in `LOADED`,
    /// increment the test-only load counter, do not track inserted count.
    /// `force_reload = true` → reload semantics: clear `LOADED` entry first, skip the
    /// test counter, return the number of newly inserted kinds.
    ///
    /// # Concurrency
    /// - `LOADED` Mutex is acquired, inspected/mutated, and released **before**
    ///   any file I/O or `Mutex<Db>` acquisition.  The two mutexes are never
    ///   held simultaneously; deadlock is structurally impossible.
    /// - Under a first-open race (`force_reload = false`), only the caller that
    ///   wins the `mark` insert proceeds to I/O; the other returns `Ok(0)`.
    /// - No `MutexGuard` is held across an `.await` point; safe to call from
    ///   an async context.
    ///
    /// # Errors
    /// Returns `Err` for I/O failure, TOML parse failure, lock poisoning, or DB
    /// errors.  `.journal.toml` absence is treated as `Ok(0)` (silent skip).
    fn load_journal_toml_kinds(&self, persona: &str, force_reload: bool) -> Result<usize> {
        let canonical_root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.to_path_buf());

        // force_reload path: remove the loaded flag so the slow path runs again.
        if force_reload {
            loaded_cache::remove(&canonical_root, persona)?;
        }

        // ensure_loaded path: fast-path return when already loaded.
        if !force_reload && loaded_cache::contains(&canonical_root, persona)? {
            return Ok(0);
        }

        // Mark as loaded before I/O (serializes concurrent first-open races).
        let is_first = loaded_cache::mark(canonical_root.clone(), persona)?;

        // ensure_loaded path: another thread beat us; skip I/O.
        if !force_reload && !is_first {
            return Ok(0);
        }

        // Test-only load counter: incremented only on the ensure_loaded path.
        #[cfg(test)]
        if !force_reload {
            crate::loader::load_counter_inc(&canonical_root, persona);
        }

        let toml_path = canonical_root.join(persona).join(".journal.toml");
        let src = match std::fs::read_to_string(&toml_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(0);
            }
            Err(e) => {
                tracing::warn!(
                    ?e,
                    ?toml_path,
                    "load_journal_toml_kinds: failed to read .journal.toml"
                );
                return Err(Error::Io(e));
            }
        };

        let kinds = crate::loader::parse_journal_toml(&src).map_err(|e| {
            tracing::warn!(
                ?e,
                ?toml_path,
                "load_journal_toml_kinds: failed to parse .journal.toml"
            );
            e
        })?;

        // Use db_cache::get_or_open directly — NOT self.open_db() — to avoid recursion.
        let db_arc = db_cache::get_or_open(&self.root, persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;

        let mut inserted = 0usize;
        for cfg in kinds {
            if db.get_kind(&cfg.kind)?.is_some() {
                tracing::info!(
                    kind = %cfg.kind,
                    "load_journal_toml_kinds: kind already registered, skipping to preserve existing config"
                );
                continue;
            }
            db.upsert_kind(&cfg)?;
            inserted += 1;
        }

        Ok(inserted)
    }

    /// Trigger the `.journal.toml` loader for `persona` if it has not yet run
    /// in this process.
    ///
    /// Fast-path: acquires `LOADED: Mutex<HashSet>` for a single `contains`
    /// check, then releases it immediately.  If the persona is already in the
    /// set, returns `Ok(())` without performing any I/O.
    ///
    /// Slow-path (first call per persona): inserts the persona into `LOADED`
    /// before releasing the mutex, then reads and parses
    /// `<canonical_root>/<persona>/.journal.toml`, then opens `Mutex<Db>` and
    /// runs insert-if-absent upserts for each kind.
    ///
    /// # Concurrency
    /// - `LOADED` Mutex is **never held while performing I/O or while holding
    ///   `Mutex<Db>`**.  The two locks are acquired sequentially, not
    ///   concurrently; deadlock is structurally impossible.
    /// - Under a first-open race, only the thread that successfully inserts the
    ///   persona into `LOADED` proceeds to I/O; the others return immediately on
    ///   the next `contains` check (after the inserting thread releases the
    ///   mutex).  `.journal.toml` is therefore read at most once per
    ///   `(canonical_root, persona)` pair per process lifetime.
    /// - Called internally by `open_db`.  Callers of `open_db` must **not** hold
    ///   any other lock when `open_db` is invoked, to preserve lock ordering.
    ///
    /// # Panics
    /// Does not panic.  Lock and I/O failures are returned as `Err`.
    fn ensure_loaded(&self, persona: &str) -> Result<()> {
        let _ = self.load_journal_toml_kinds(persona, false)?;
        Ok(())
    }

    /// Re-scan `<root>/<persona>/.journal.toml` and insert any new kinds.
    ///
    /// Clears the per-persona "already loaded" flag from the process-global
    /// `LOADED` cache, then runs the same insert-if-absent loader logic as the
    /// initial `ensure_loaded` call.  Existing kinds (registered via
    /// `kind_register` or a prior `ensure_loaded`) are **not** overwritten.
    ///
    /// Returns the number of kinds newly inserted in this call.
    /// Returns `Ok(0)` if `.journal.toml` is absent (silent skip).
    ///
    /// # Concurrency
    /// - Acquires `LOADED: Mutex<HashSet>` briefly to remove the cached entry,
    ///   then releases it before performing any I/O or acquiring `Mutex<Db>`.
    /// - Lock acquisition order is `LOADED` → release → `Mutex<Db>`.
    ///   This matches the order used by `ensure_loaded`; no lock is ever held
    ///   simultaneously with another, so deadlock is structurally impossible.
    /// - Concurrent calls for the **same persona** are serialized by the
    ///   sequential acquire of `LOADED` Mutex.  Concurrent calls for
    ///   **different personas** do not contend on the inner `Mutex<Db>`.
    /// - No `MutexGuard` is held across an `.await` point; safe to call from
    ///   an async context without blocking the runtime.
    /// - If `LOADED` Mutex is poisoned (a thread panicked while holding it),
    ///   returns `Err(Error::Invalid("loaded cache lock poisoned: ..."))`.
    ///
    /// # Panics
    /// Does not panic.  All lock and I/O failures are returned as `Err`.
    pub fn reload_kinds(&self, persona: &str) -> Result<usize> {
        let inserted = self.load_journal_toml_kinds(persona, true)?;
        tracing::info!(persona, inserted, "reload_kinds: complete");
        Ok(inserted)
    }

    /// Register (or replace) a kind config for the persona.
    ///
    /// # Concurrency
    /// Acquires `Mutex<HashMap>` and `Mutex<Db>` sequentially; neither guard
    /// crosses an `.await` point. Concurrent callers for the same persona
    /// serialize at `Mutex<Db>`. Returns `Err` if any mutex is poisoned.
    pub fn kind_register(&self, persona: &str, config: &KindConfig) -> Result<()> {
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        db.upsert_kind(config)
    }

    /// Parse `toml_src` as a single-kind TOML document, register the resulting
    /// `KindConfig` for `persona`, and return the parsed config.
    ///
    /// This is the canonical entry point for the MCP `journal_kind_register`
    /// handler and any other external callers that have raw TOML input.  It
    /// unifies the TOML→kind path that was previously split across the MCP
    /// handler and `kind_register` (Crux #2).
    ///
    /// # Arguments
    /// - `persona` — persona id (matches persona-pack id).
    /// - `toml_src` — full single-kind TOML document (§9.5).
    ///
    /// # Returns
    /// The parsed `KindConfig` on success; callers may extract the `kind` field
    /// to build a JSON response.
    ///
    /// # Errors
    /// - TOML deserialization failure → propagated from `parse_kind_toml`.
    /// - `kind_register` failure (DB or lock error) → propagated as `Error`.
    ///
    /// Both error kinds are returned via `?` through `From<CoreError> for Error`.
    pub fn register_kind_from_toml(&self, persona: &str, toml_src: &str) -> Result<KindConfig> {
        let cfg = persona_journal_core::loader::parse_kind_toml(toml_src)?;
        self.kind_register(persona, &cfg)?;
        Ok(cfg)
    }

    /// Lists registered kinds for the persona.
    ///
    /// # Concurrency
    /// Acquires `Mutex<HashMap>` and `Mutex<Db>` sequentially; neither guard
    /// crosses an `.await` point. Concurrent callers for the same persona
    /// serialize at `Mutex<Db>`. Returns `Err` if any mutex is poisoned.
    pub fn kind_list(&self, persona: &str) -> Result<Vec<KindConfig>> {
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        db.list_kinds()
    }

    /// Looks up a kind by name for the persona.
    ///
    /// # Concurrency
    /// Acquires `Mutex<HashMap>` and `Mutex<Db>` sequentially; neither guard
    /// crosses an `.await` point. Concurrent callers for the same persona
    /// serialize at `Mutex<Db>`. Returns `Err` if any mutex is poisoned.
    pub fn kind_get(&self, persona: &str, kind: &str) -> Result<Option<KindConfig>> {
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        db.get_kind(kind)
    }

    /// Ensure the `emo` preset is registered if no kinds exist yet.
    ///
    /// # Concurrency
    /// Not atomic with respect to concurrent callers: two threads may both
    /// observe an empty kinds list and both attempt to upsert the preset.
    /// The second upsert is idempotent (`upsert_kind` uses INSERT OR REPLACE).
    /// No guard is held across an `.await` point.
    pub fn ensure_default_kinds(&self, persona: &str) -> Result<()> {
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        if db.list_kinds()?.is_empty() {
            db.upsert_kind(&KindConfig::preset_emo())?;
        }
        Ok(())
    }

    /// Append a new entry. Returns the entry id.
    ///
    /// # Concurrency
    /// This method acquires two locks sequentially: the outer `Mutex<HashMap>`
    /// (released before returning `Arc<Mutex<Db>>`), then the inner
    /// `Mutex<Db>` (released before this function returns). Neither guard is
    /// held across an `.await` point. Concurrent calls for the **same persona**
    /// serialize at the inner `Mutex<Db>`. Concurrent calls for **different
    /// personas** do not contend on the inner lock.
    ///
    /// If the inner `Mutex<Db>` is poisoned (a thread panicked while holding
    /// it), this function returns `Err(Error::Invalid("db lock poisoned: ..."))`.
    /// The process must be restarted to recover from a poisoned mutex.
    ///
    /// # Panics
    /// Does not panic. All lock failures are propagated as `Err`.
    pub fn say(&self, persona: &str, kind: &str, text: &str, tags: Vec<String>) -> Result<String> {
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        let kind_cfg = db
            .get_kind(kind)?
            .ok_or_else(|| Error::UnknownKind(kind.to_string()))?;
        if !matches!(kind_cfg.mode, KindMode::Entries) {
            return Err(Error::Invalid(
                "say() supports entries mode only (MVP)".to_string(),
            ));
        }

        let now = OffsetDateTime::now_utc();
        let now_iso = now
            .format(&Rfc3339)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let ym = format!("{:04}-{:02}", now.year(), u8::from(now.month()));
        let seq = db.next_seq(kind, &ym)?;
        let id = entry_id(now.year(), u8::from(now.month()) as u32, seq);

        let first_line = extract_first_line(text);

        let path = if kind_cfg.versioning {
            versioned_path(&self.root, persona, kind, &id, 1)
        } else {
            flat_path(&self.root, persona, kind, &id)
        };
        let rel = path
            .strip_prefix(&self.root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());
        db.say_atomic(
            &id,
            kind,
            &now_iso,
            first_line.as_deref(),
            &tags,
            1,
            &rel,
            text,
        )?;

        // --- Tx done. Projection regen is best-effort. ---
        // FS .md files are projections from the DB SoT. Failures here do not
        // roll back the committed entry; run `projection_rebuild` to restore
        // FS consistency from the DB.
        self.project_entry_file_best_effort(&path, text);
        if kind_cfg.indexed {
            self.project_index_best_effort(&db, persona, &now_iso);
        }

        Ok(id)
    }

    /// Single implementation of "render and write `_index.md` for one persona".
    ///
    /// Both `project_index_best_effort` (called from `say`) and `projection_rebuild`
    /// delegate here.  This is the canonical projection write path for `_index.md`
    /// (Crux 2: single helper).
    fn project_index_inner(&self, db: &Db, persona: &str, now_iso: &str) -> Result<()> {
        let body = render_root_index(db, persona, now_iso)?;
        write_root_index(&self.root, persona, &body)
    }

    /// Best-effort wrapper called from `say()`.  Logs a warning on error and
    /// returns `()` — FS projection failures do not roll back the committed entry.
    fn project_index_best_effort(&self, db: &Db, persona: &str, now_iso: &str) {
        if let Err(e) = self.project_index_inner(db, persona, now_iso) {
            tracing::warn!(?e, "index projection failed; entry is safe in DB");
        }
    }

    /// Best-effort entry-file wrapper called from `say()`.  Logs a warning on
    /// error and returns `()`.
    fn project_entry_file_best_effort(&self, path: &Path, text: &str) {
        if let Err(e) = write_file(path, text) {
            tracing::warn!(?e, ?path, "fs projection write failed; entry is safe in DB");
        }
    }

    /// Return the latest `n` entries for `persona` / `kind`, newest first.
    ///
    /// # Concurrency
    /// Acquires `Mutex<HashMap>` briefly to look up the cached `Arc<Mutex<Db>>`,
    /// then acquires `Mutex<Db>` for the duration of the SQLite query.
    /// No guard is held across an `.await` point. Safe to call concurrently
    /// from multiple threads or tokio tasks for the same persona; reads
    /// serialize at the inner `Mutex<Db>`.
    ///
    /// Returns `Err(Error::Invalid(...))` if either mutex is poisoned.
    pub fn query_latest(&self, persona: &str, kind: &str, n: usize) -> Result<Vec<EntryRow>> {
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        let rows = db.query_latest(kind, n)?;
        Ok(rows.into_iter().map(EntryRow::from).collect())
    }

    /// Return up to `n` entries for `persona` / `kind`, ordered by Ebbinghaus decay score
    /// (highest score first).
    ///
    /// # Decay formula
    ///
    /// ```text
    /// score = weight * exp(-ln(2) * age_days / half_life_days)
    /// ```
    ///
    /// `half_life_days` is the number of days after which the score halves.
    /// Entries with a higher score appear earlier in the result.
    ///
    /// # Arguments
    /// - `persona`: persona name
    /// - `kind`: kind name
    /// - `n`: maximum number of rows to return
    /// - `now`: reference timestamp for age computation; pass a fixed value in tests
    ///   to get deterministic score ordering
    ///
    /// # Concurrency
    /// Acquires `Mutex<HashMap>` briefly to look up the cached `Arc<Mutex<Db>>`,
    /// then acquires `Mutex<Db>` for the duration of the SQLite query.
    /// No guard is held across an `.await` point.
    ///
    /// # Errors
    /// Returns `Err(Error::Invalid(...))` if the `now` timestamp cannot be formatted
    /// as RFC 3339, or if either mutex is poisoned.
    /// Returns `Err(Error::Sqlite(...))` on DB error.
    pub fn query_by_retrieval(
        &self,
        persona: &str,
        kind: &str,
        n: usize,
        now: OffsetDateTime,
    ) -> Result<Vec<EntryRow>> {
        let now_iso = now
            .format(&Rfc3339)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        let rows = db.query_by_retrieval(kind, n, &now_iso)?;
        Ok(rows.into_iter().map(EntryRow::from).collect())
    }

    /// Filter entries by retrieval strength using one of four modes.
    ///
    /// Score is computed by the DB using the 4-factor formula from `query_by_retrieval`
    /// (retrieval_strength × boost_factor × decay_weight × exp(-ln2 × age / half_life)).
    /// No score reimplementation is performed in Rust.
    ///
    /// # Arguments
    /// - `persona`: persona name
    /// - `kind`: kind name
    /// - `mode`: filter mode (Visible / Partial / Archive / Full)
    /// - `now`: reference time for decay calculation
    ///
    /// # Errors
    /// Returns `Err(Error::Invalid(...))` for NaN or negative threshold, or on DB/lock failure.
    pub fn filter(
        &self,
        persona: &str,
        kind: &str,
        mode: FilterMode,
        now: OffsetDateTime,
    ) -> Result<Vec<EntryRow>> {
        // 1. Validate threshold (NaN check must come before < 0.0 because NaN < 0.0 is false)
        match mode {
            FilterMode::Visible { threshold } | FilterMode::Archive { threshold } => {
                if threshold.is_nan() {
                    return Err(Error::Invalid("filter threshold is NaN".into()));
                }
                if threshold < 0.0 {
                    return Err(Error::Invalid(format!(
                        "filter threshold must be >= 0.0, got {threshold}"
                    )));
                }
            }
            FilterMode::Partial { threshold, .. } => {
                if threshold.is_nan() {
                    return Err(Error::Invalid("filter threshold is NaN".into()));
                }
                if threshold < 0.0 {
                    return Err(Error::Invalid(format!(
                        "filter threshold must be >= 0.0, got {threshold}"
                    )));
                }
            }
            FilterMode::Full => {}
        }
        // 2. Format now as RFC3339, open DB
        let now_iso = now
            .format(&Rfc3339)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        // 3. Fetch all entries with scores (tags already enriched inside db layer)
        let scored = db.query_by_retrieval_with_scores(kind, usize::MAX, &now_iso)?;
        // 4. Partition by mode (crux constraints enforced here)
        let rows: Vec<EntryMetaRow> = match mode {
            // Full: all entries in score DESC order
            FilterMode::Full => scored.into_iter().map(|(r, _)| r).collect(),
            // Visible: score >= threshold (crux: ">=" not ">")
            FilterMode::Visible { threshold } => scored
                .into_iter()
                .filter(|(_, s)| *s >= threshold)
                .map(|(r, _)| r)
                .collect(),
            // Archive: score < threshold (crux: "<" not "<=")
            FilterMode::Archive { threshold } => scored
                .into_iter()
                .filter(|(_, s)| *s < threshold)
                .map(|(r, _)| r)
                .collect(),
            // Partial: UNION of {score >= threshold} and {top_k by score DESC} (crux: OR not AND)
            FilterMode::Partial { threshold, top_k } => {
                let mut keep_ids: HashSet<String> = scored
                    .iter()
                    .filter(|(_, s)| *s >= threshold)
                    .map(|(r, _)| r.id.clone())
                    .collect();
                for (r, _) in scored.iter().take(top_k) {
                    keep_ids.insert(r.id.clone());
                }
                // Preserve score DESC order from the original sorted vec
                scored
                    .into_iter()
                    .filter(|(r, _)| keep_ids.contains(&r.id))
                    .map(|(r, _)| r)
                    .collect()
            }
        };
        // 5. Convert EntryMetaRow -> EntryRow (tags carried from db layer, no tags_for call here)
        Ok(rows.into_iter().map(EntryRow::from).collect())
    }

    /// Render the archive index for `persona` as a Markdown 5-column table.
    ///
    /// Delegates to `Journal::filter(persona, "archive", FilterMode::Archive { threshold: 0.01 }, now)`
    /// — no score logic is inlined here (Crux 1: filter wrap 依存境界).
    ///
    /// Output contract (Crux 2: Markdown table 出力契約):
    /// - Header: `# {persona}_archive_index`
    /// - Table columns (fixed order): `created_at | kind | entry_id | retrieval_strength | body (head 80 chars)`
    /// - Rows ordered by `created_at` DESC
    /// - When 0 archive entries exist, only the header and separator rows are emitted
    ///
    /// Returns `Err(Error::UnknownKind("archive"))` if the archive kind is not registered
    /// for `persona` (Crux 3: 未登録 kind の reject).
    ///
    /// # Errors
    /// - `Error::UnknownKind("archive")` — archive kind not registered
    /// - `Error::Invalid(...)` — DB lock poisoned, RFC3339 format failure
    /// - `Error::Sqlite(...)` — underlying DB error
    pub fn archive_index_render(&self, persona: &str) -> Result<String> {
        // Crux 3: reject if archive kind is not registered (filter itself returns empty Vec
        // for unknown kinds — must check explicitly before calling filter).
        let _archive_cfg = match self.kind_get(persona, "archive")? {
            Some(cfg) => cfg,
            None => {
                tracing::warn!(persona, "archive_index_render: archive kind not registered");
                return Err(Error::UnknownKind("archive".to_string()));
            }
        };

        let now = OffsetDateTime::now_utc();

        // Crux 1: delegate to filter with FilterMode::Archive { threshold: 0.01 }.
        // No score comparison logic is inlined here.
        let mut rows = self.filter(
            persona,
            "archive",
            FilterMode::Archive { threshold: 0.01 },
            now,
        )?;

        // Crux 2: sort created_at DESC (RFC3339 is lexicographically ordered).
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        // Crux 2: build 5-column Markdown table.
        let mut out = String::new();
        let _ = writeln!(out, "# {persona}_archive_index");
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "| created_at | kind | entry_id | retrieval_strength | body (head 80 chars) |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|");
        for row in &rows {
            let body_head: String = row
                .summary
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} |",
                row.created_at, row.kind, row.id, row.retrieval_strength, body_head
            );
        }
        Ok(out)
    }

    /// Read the content of entry `id` at the given `version` (defaults to
    /// current version).
    ///
    /// Body is read directly from `versions.body` in the DB (DB is SoT).
    /// No filesystem I/O is performed.
    ///
    /// # Arguments
    /// - `persona`: persona name
    /// - `id`: entry identifier
    /// - `version`: specific version to read; uses `current_version` if `None`
    ///
    /// # Returns
    /// The body string stored for that version.
    ///
    /// # Errors
    /// Returns `Error::EntryNotFound` if the entry or version does not exist.
    /// Returns `Err` on DB or lock failure.
    pub fn entry_read(&self, persona: &str, id: &str, version: Option<u32>) -> Result<String> {
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        let meta = db
            .get_entry(id)?
            .ok_or_else(|| Error::EntryNotFound(id.to_string()))?;
        let v = version.unwrap_or(meta.current_version);
        db.version_body(id, v)?
            .ok_or_else(|| Error::EntryNotFound(format!("{}@v{}", id, v)))
    }

    /// Set the `retrieval_strength` for the given entry.
    ///
    /// # Arguments
    /// - `persona`: persona id (matches persona-pack id)
    /// - `entry_id`: entry identifier (text id, e.g. `"2024-01_00001"`)
    /// - `value`: new retrieval strength; must be in `[0.0, 1.0]` and not NaN
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// Returns `Err(Error::Invalid(...))` if `value` is NaN or outside `[0.0, 1.0]`.
    /// Returns `Err(Error::EntryNotFound(...))` if the entry does not exist.
    /// Returns `Err` on DB or lock failure.
    pub fn set_retrieval_strength(&self, persona: &str, entry_id: &str, value: f64) -> Result<()> {
        if value.is_nan() {
            return Err(Error::Invalid(
                "retrieval_strength must not be NaN".to_string(),
            ));
        }
        if !(0.0..=1.0).contains(&value) {
            return Err(Error::Invalid(format!(
                "retrieval_strength out of range [0.0, 1.0]: {value}"
            )));
        }
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        db.set_retrieval_strength(entry_id, value)
    }

    /// Get the `retrieval_strength` for the given entry.
    ///
    /// # Arguments
    /// - `persona`: persona id (matches persona-pack id)
    /// - `entry_id`: entry identifier (text id, e.g. `"2024-01_00001"`)
    ///
    /// # Returns
    /// `Ok(value)` with the current retrieval strength.
    ///
    /// # Errors
    /// Returns `Err(Error::EntryNotFound(...))` if the entry does not exist.
    /// Returns `Err` on DB or lock failure.
    pub fn get_retrieval_strength(&self, persona: &str, entry_id: &str) -> Result<f64> {
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        db.get_retrieval_strength(entry_id)
    }

    /// Pin an entry by setting its `retrieval_strength` to a specific value.
    ///
    /// Delegates to `set_retrieval_strength` with `strength.unwrap_or(1.0)`.
    ///
    /// # Arguments
    /// - `persona`: persona id (matches persona-pack id)
    /// - `entry_id`: entry identifier (text id, e.g. `"2024-01_00001"`)
    /// - `strength`: target retrieval strength; if `None`, defaults to `1.0`
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// Returns `Err(Error::Invalid(...))` if `strength` is NaN or outside `[0.0, 1.0]`.
    /// Returns `Err(Error::EntryNotFound(...))` if the entry does not exist.
    /// Returns `Err` on DB or lock failure.
    ///
    /// # Concurrency
    /// Acquires `Mutex<Db>` for the duration of the DB write. No guard crosses an `.await`
    /// point. Cancel-safe by construction.
    pub fn pin(&self, persona: &str, entry_id: &str, strength: Option<f64>) -> Result<()> {
        self.set_retrieval_strength(persona, entry_id, strength.unwrap_or(1.0))
    }

    /// Unpin an entry by resetting its `retrieval_strength` to `1.0` (neutral).
    ///
    /// Always writes `1.0` regardless of the current value.
    /// The schema enforces `NOT NULL`, so `NULL` is never written.
    ///
    /// # Arguments
    /// - `persona`: persona id (matches persona-pack id)
    /// - `entry_id`: entry identifier (text id, e.g. `"2024-01_00001"`)
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// Returns `Err(Error::EntryNotFound(...))` if the entry does not exist.
    /// Returns `Err` on DB or lock failure.
    ///
    /// # Concurrency
    /// Acquires `Mutex<Db>` for the duration of the DB write. No guard crosses an `.await`
    /// point. Cancel-safe by construction.
    pub fn unpin(&self, persona: &str, entry_id: &str) -> Result<()> {
        // Crux C1: always write 1.0 (neutral reset). NULL is schema-impossible (NOT NULL).
        self.set_retrieval_strength(persona, entry_id, 1.0)
    }

    /// Set a kind-wide retrieval boost factor.
    ///
    /// `boost_factor` は `decay.weight` とは独立した static 倍率です。
    /// - `decay.weight` は時間減衰曲線 (half-life × weight) の形状を決定するパラメータで、時間依存
    /// - `boost_factor` は kind 全 entry の retrieval スコアに対する乗数で、時間非依存
    ///
    /// 両者は乗算的に組み合わさり、ST3 の retrieval 計算式で
    /// `retrieval_strength × boost_factor × decay(t)` として作用します
    /// (本 ST2 scope では storage のみ、計算式統合は ST3)。
    ///
    /// # Arguments
    /// - `persona`: persona id (matches persona-pack id)
    /// - `kind`: kind name (must already be registered)
    /// - `factor`: new boost factor; must be `> 0.0` and not NaN
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// Returns `Err(Error::Invalid(...))` if `factor` is NaN or `<= 0.0`.
    /// Returns `Err(Error::UnknownKind(...))` if the kind is not registered.
    /// Returns `Err` on DB or lock failure.
    ///
    /// # Concurrency
    /// Acquires `Mutex<Db>` for the duration of the DB write. No guard crosses an `.await`
    /// point. Cancel-safe by construction.
    pub fn boost_kind(&self, persona: &str, kind: &str, factor: f64) -> Result<()> {
        // Crux C2: reject NaN (IEEE 754: NaN <= 0.0 is false, so must check separately)
        // and reject zero / negative values; both would corrupt ST3 retrieval scores.
        if factor.is_nan() || factor <= 0.0 {
            return Err(Error::Invalid(format!(
                "boost_factor must be > 0.0 and not NaN, got {factor}"
            )));
        }
        let db_arc = self.open_db(persona)?;
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        db.set_boost_factor(kind, factor)
    }

    /// Rebuild all FS projections for `persona` from the DB SoT.
    ///
    /// This is a **strict-mode** repair tool: any `write_file` failure returns
    /// `Err` immediately (unlike `say`, which uses best-effort warn). Use this
    /// to restore FS consistency after a partial failure or manual edit.
    ///
    /// # What is regenerated
    /// 1. Every entry `.md` file in `versions` (all versions, all kinds). Path
    ///    is determined by `KindConfig.versioning`: versioned path for `true`,
    ///    flat path for `false`.
    /// 2. `_index.md` for each `indexed = true` kind.
    ///
    /// # Concurrency
    /// Acquires `Mutex<Db>` to read all versions + kind configs, releases it
    /// before performing any filesystem writes. No guard crosses an `.await`
    /// point. Concurrent filesystem writes from two callers both produce identical
    /// content (DB is SoT) so the final on-disk state is deterministic regardless
    /// of write order.
    ///
    /// # Errors
    /// Returns `Err` if any DB operation or filesystem write fails.
    pub fn projection_rebuild(&self, persona: &str) -> Result<()> {
        let db_arc = self.open_db(persona)?;
        let now = OffsetDateTime::now_utc();
        let now_iso = now
            .format(&Rfc3339)
            .map_err(|e| Error::Invalid(e.to_string()))?;

        // Collect all version rows while holding the lock.
        let version_rows = {
            let db = db_arc
                .lock()
                .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
            let rows = db.list_all_versions()?;
            // Build per-row (path, body) pairs while we still hold the lock so
            // we can call get_kind inside the guard.
            let mut pairs: Vec<(std::path::PathBuf, String)> = Vec::with_capacity(rows.len());
            for row in rows {
                let kind_cfg = db
                    .get_kind(&row.kind)?
                    .ok_or_else(|| Error::Invalid(format!("unknown kind: {}", row.kind)))?;
                let path = if kind_cfg.versioning {
                    versioned_path(&self.root, persona, &row.kind, &row.entry_id, row.version)
                } else {
                    flat_path(&self.root, persona, &row.kind, &row.entry_id)
                };
                pairs.push((path, row.body));
            }
            pairs
            // MutexGuard dropped here
        };

        // Write entry projections — strict: propagate Err.
        for (path, body) in version_rows {
            write_file(&path, &body)?;
        }

        // Write _index.md via the shared helper — strict: propagate Err.
        // Re-acquire the guard briefly (short read-only access for render).
        let db = db_arc
            .lock()
            .map_err(|e| Error::Invalid(format!("db lock poisoned: {e}")))?;
        self.project_index_inner(&db, persona, &now_iso)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Compile-time assertion: Db is !Sync (rusqlite::Connection is !Sync),
    // confirming that Arc<Mutex<Db>> is the correct wrapper (not bare Arc<Db>).
    static_assertions::assert_not_impl_any!(crate::db::Db: Sync);

    #[test]
    fn say_and_query_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let id1 = j
            .say("shi", "emo", "# 雨の日\n窓を見ていた", vec!["静か".into()])
            .unwrap();
        let id2 = j.say("shi", "emo", "# 朝の声", vec![]).unwrap();
        assert_ne!(id1, id2);
        let rows = j.query_latest("shi", "emo", 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, id2);
        let body = j.entry_read("shi", &id1, None).unwrap();
        assert!(body.contains("雨の日"));
        // DB is SoT: entry_read works without FS projection
        let body2 = j.entry_read("shi", &id2, None).unwrap();
        assert!(body2.contains("朝の声"));
        let idx = std::fs::read_to_string(tmp.path().join("shi/_index.md")).unwrap();
        assert!(idx.contains("# shi journal"));
        assert!(idx.contains(&id1));
    }

    #[test]
    fn kind_tags_roundtrip() {
        use crate::schema::KindConfig;

        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        let mut cfg = KindConfig::preset_emo();
        cfg.kind = "test_kind".to_string();
        cfg.tags = vec!["alpha".into(), "beta".into(), "gamma".into()];
        j.kind_register("shi", &cfg).unwrap();

        let got = j.kind_get("shi", "test_kind").unwrap().unwrap();
        assert_eq!(got.tags, vec!["alpha", "beta", "gamma"]);

        let list = j.kind_list("shi").unwrap();
        let found = list.iter().find(|k| k.kind == "test_kind").unwrap();
        assert_eq!(found.tags, vec!["alpha", "beta", "gamma"]);
    }

    #[cfg(unix)]
    #[test]
    fn say_succeeds_when_fs_projection_fails() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let emo_dir = tmp.path().join("shi/emo");
        std::fs::create_dir_all(&emo_dir).unwrap();
        let mut perms = std::fs::metadata(&emo_dir).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&emo_dir, perms.clone()).unwrap();

        let id = j.say("shi", "emo", "# test body", vec![]).unwrap();

        perms.set_mode(0o755);
        std::fs::set_permissions(&emo_dir, perms).unwrap();

        // DB is healthy: body is readable via DB SELECT even though FS projection failed
        let body = j.entry_read("shi", &id, None).unwrap();
        assert!(body.contains("test body"));
    }

    #[test]
    fn entry_read_reads_from_db_not_fs() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let id = j.say("shi", "emo", "# from db", vec![]).unwrap();
        // Remove FS projection — entry_read must still succeed via DB
        let emo_dir = tmp.path().join("shi/emo");
        std::fs::remove_dir_all(&emo_dir).unwrap();
        let body = j.entry_read("shi", &id, None).unwrap();
        assert!(body.contains("from db"));
    }

    #[test]
    fn projection_rebuild_regenerates_fs_from_db() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let id = j.say("shi", "emo", "# rebuild me", vec![]).unwrap();
        let persona_dir = tmp.path().join("shi");
        std::fs::remove_dir_all(persona_dir.join("emo")).unwrap();
        let _ = std::fs::remove_file(persona_dir.join("_index.md"));
        j.projection_rebuild("shi").unwrap();
        // FS restored: _index.md contains the entry id
        let idx = std::fs::read_to_string(persona_dir.join("_index.md")).unwrap();
        assert!(idx.contains(&id));
        // entry .md (versioned path) also restored — at least one file in emo dir
        let entries = std::fs::read_dir(persona_dir.join("emo")).unwrap();
        assert!(entries.count() > 0);
    }

    /// Crux 2 evidence: both `say` and `projection_rebuild` produce an `_index.md`
    /// that contains the entry id, confirming that both paths share the same
    /// `project_index_inner` helper.
    #[test]
    fn say_and_rebuild_produce_equivalent_index() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let id = j.say("shi", "emo", "# equivalence test", vec![]).unwrap();
        let persona_dir = tmp.path().join("shi");
        let after_say = std::fs::read_to_string(persona_dir.join("_index.md")).unwrap();

        // Remove _index.md and rebuild via projection_rebuild.
        std::fs::remove_file(persona_dir.join("_index.md")).unwrap();
        j.projection_rebuild("shi").unwrap();
        let after_rebuild = std::fs::read_to_string(persona_dir.join("_index.md")).unwrap();

        // Both paths produce an _index.md that contains the entry id.
        assert!(
            after_say.contains(&id),
            "after_say _index.md missing entry id: {id}"
        );
        assert!(
            after_rebuild.contains(&id),
            "after_rebuild _index.md missing entry id: {id}"
        );
    }

    // ── concurrency tests ────────────────────────────────────────────────────

    #[test]
    fn test_db_cache_concurrent_same_persona() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let persona = "shi";
        let n = 4;
        let barrier = Arc::new(Barrier::new(n));

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let root = root.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    db_cache::get_or_open(&root, persona)
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let arcs: Vec<_> = results.into_iter().map(|r| r.unwrap()).collect();
        let first_ptr = Arc::as_ptr(&arcs[0]);
        for arc in &arcs[1..] {
            assert_eq!(
                Arc::as_ptr(arc),
                first_ptr,
                "all threads must share same Arc"
            );
        }
    }

    #[test]
    fn test_db_cache_concurrent_different_personas() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let n = 8usize;
        let barrier = Arc::new(Barrier::new(n));

        let handles: Vec<_> = (0..n)
            .map(|i| {
                let root = root.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let persona = format!("p{i}");
                    db_cache::get_or_open(&root, &persona)
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap().unwrap();
        }
    }

    #[test]
    fn test_mutex_db_no_guard_across_sync_boundary() {
        use std::sync::Arc;
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let j = Arc::new(Journal::open(tmp.path().to_path_buf()));
        j.ensure_default_kinds("shi").unwrap();

        let j2 = Arc::clone(&j);
        let h1 = thread::spawn(move || {
            for i in 0..100 {
                j2.say("shi", "emo", &format!("thread1 entry {i}"), vec![])
                    .unwrap();
            }
        });
        let h2 = thread::spawn(move || {
            for i in 0..100 {
                j.say("shi", "emo", &format!("thread2 entry {i}"), vec![])
                    .unwrap();
            }
        });
        h1.join().unwrap();
        h2.join().unwrap();

        let j_verify = Journal::open(tmp.path().to_path_buf());
        let rows = j_verify.query_latest("shi", "emo", 200).unwrap();
        assert_eq!(rows.len(), 200);
    }

    #[test]
    fn test_oncelock_init_runs_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, OnceLock};
        use std::thread;

        static CELL: OnceLock<usize> = OnceLock::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let n = 8;

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let counter = Arc::clone(&counter);
                thread::spawn(move || {
                    CELL.get_or_init(|| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        42
                    });
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_arc_clone_shared_ownership() {
        use crate::db::Db;
        use std::sync::{Arc, Mutex};
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let db = Db::open(&tmp.path().join("test.db")).unwrap();
        let arc = Arc::new(Mutex::new(db));
        let n = 4;

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let arc = Arc::clone(&arc);
                thread::spawn(move || {
                    let _guard = arc.lock().unwrap();
                    // guard drop at end of scope
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(Arc::strong_count(&arc), 1);
    }

    #[test]
    fn test_hashmap_no_cross_root_contamination() {
        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();

        let arc_a = db_cache::get_or_open(tmp_a.path(), "shi").unwrap();
        let arc_b = db_cache::get_or_open(tmp_b.path(), "shi").unwrap();

        assert!(
            !std::sync::Arc::ptr_eq(&arc_a, &arc_b),
            "different roots must produce different Arc instances"
        );
    }

    // ── loader functional tests ──────────────────────────────────────────────

    const JOURNAL_TOML_3KINDS: &str = r#"
[[kinds]]
kind = "loader_test_a"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []

[[kinds]]
kind = "loader_test_b"
mode = "named_index"
source = "hand"
path_template = "{persona}/{persona}_loader_test_b_index.md"
versioning = false
indexed = false
tags = ["x"]

[[kinds]]
kind = "loader_test_c"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = false
indexed = true
tags = []
"#;

    #[test]
    fn test_loader_installs_kinds_from_journal_toml() {
        let tmp = TempDir::new().unwrap();
        let persona_dir = tmp.path().join("alice");
        std::fs::create_dir_all(&persona_dir).unwrap();
        std::fs::write(
            persona_dir.join(".journal.toml"),
            JOURNAL_TOML_3KINDS.trim(),
        )
        .unwrap();

        let j = Journal::open(tmp.path().to_path_buf());
        // Trigger open_db → ensure_loaded by calling kind_list.
        let kinds = j.kind_list("alice").unwrap();
        let names: Vec<&str> = kinds.iter().map(|k| k.kind.as_str()).collect();
        assert!(names.contains(&"loader_test_a"), "a missing: {names:?}");
        assert!(names.contains(&"loader_test_b"), "b missing: {names:?}");
        assert!(names.contains(&"loader_test_c"), "c missing: {names:?}");
    }

    #[test]
    fn test_loader_skips_mcp_registered_kind() {
        // Crux #2: loader must NOT overwrite a kind registered via kind_register.
        let tmp = TempDir::new().unwrap();
        let persona = "crux2_persona";
        let persona_dir = tmp.path().join(persona);
        std::fs::create_dir_all(&persona_dir).unwrap();

        // First register "emo" with tags=["mcp_tag"] via kind_register.
        let j = Journal::open(tmp.path().to_path_buf());
        let mut mcp_cfg = KindConfig::preset_emo();
        mcp_cfg.tags = vec!["mcp_tag".to_string()];
        j.kind_register(persona, &mcp_cfg).unwrap();

        // Now write .journal.toml that tries to set emo tags=["toml_tag"].
        let toml = r#"
[[kinds]]
kind = "emo"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = ["toml_tag"]
"#;
        std::fs::write(persona_dir.join(".journal.toml"), toml.trim()).unwrap();

        // Open a fresh Journal (new instance, same root — new loaded_cache entry).
        let j2 = Journal::open(tmp.path().to_path_buf());
        // Trigger ensure_loaded.
        let _ = j2.kind_list(persona).unwrap();

        // The MCP-registered tags must be preserved — loader must not overwrite.
        let got = j2.kind_get(persona, "emo").unwrap().unwrap();
        assert_eq!(
            got.tags,
            vec!["mcp_tag"],
            "loader should not have overwritten MCP-registered emo tags"
        );
    }

    #[test]
    fn test_loader_silent_skip_when_file_missing() {
        // Crux #1 / silent skip: no .journal.toml → no error, ensure_default_kinds runs.
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        // No .journal.toml exists; kind_list should succeed and be empty before ensure_default_kinds.
        // We just verify no error is raised.
        j.ensure_default_kinds("absent_persona").unwrap();
        let kinds = j.kind_list("absent_persona").unwrap();
        // ensure_default_kinds should have inserted the emo preset.
        assert!(
            kinds.iter().any(|k| k.kind == "emo"),
            "emo preset missing after ensure_default_kinds: {kinds:?}"
        );
    }

    #[test]
    fn test_loader_runs_once_per_persona() {
        let tmp = TempDir::new().unwrap();
        let persona = "once_persona";
        let persona_dir = tmp.path().join(persona);
        std::fs::create_dir_all(&persona_dir).unwrap();
        std::fs::write(
            persona_dir.join(".journal.toml"),
            r#"
[[kinds]]
kind = "once_kind"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []
"#
            .trim(),
        )
        .unwrap();

        let canonical_root = tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| tmp.path().to_path_buf());
        let j = Journal::open(tmp.path().to_path_buf());
        // Call kind_list 100 times — ensure_loaded should fire IO only once.
        for _ in 0..100 {
            j.kind_list(persona).unwrap();
        }

        let count = crate::loader::load_counter_get(&canonical_root, persona);
        assert_eq!(
            count, 1,
            "LOAD_COUNTER should be exactly 1 for this (root, persona), got {count}"
        );
    }

    #[test]
    fn test_reload_kinds_re_reads_journal_toml() {
        // After initial load, modify .journal.toml, call reload_kinds, verify new kind appears
        // and existing kind is not overwritten (insert-if-absent still applies).
        let tmp = TempDir::new().unwrap();
        let persona = "reload_persona";
        let persona_dir = tmp.path().join(persona);
        std::fs::create_dir_all(&persona_dir).unwrap();

        let initial_toml = r#"
[[kinds]]
kind = "initial_kind"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = ["original"]
"#;
        std::fs::write(persona_dir.join(".journal.toml"), initial_toml.trim()).unwrap();

        let j = Journal::open(tmp.path().to_path_buf());
        // Trigger initial load.
        let _ = j.kind_list(persona).unwrap();

        // Overwrite .journal.toml with updated content: change initial_kind tags + add new_kind.
        let updated_toml = r#"
[[kinds]]
kind = "initial_kind"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = ["modified"]

[[kinds]]
kind = "new_kind"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []
"#;
        std::fs::write(persona_dir.join(".journal.toml"), updated_toml.trim()).unwrap();

        let inserted = j.reload_kinds(persona).unwrap();
        // "new_kind" should be inserted, "initial_kind" already exists → skipped.
        assert_eq!(inserted, 1, "expected 1 new kind inserted, got {inserted}");

        // initial_kind tags must still be "original" (not overwritten by reload).
        let got = j.kind_get(persona, "initial_kind").unwrap().unwrap();
        assert_eq!(
            got.tags,
            vec!["original"],
            "reload_kinds should not overwrite existing kind"
        );

        // new_kind should now be present.
        assert!(
            j.kind_get(persona, "new_kind").unwrap().is_some(),
            "new_kind should be present after reload"
        );
    }

    // ── loader concurrency tests ─────────────────────────────────────────────

    #[test]
    fn test_first_open_race_loads_once() {
        // 4 threads simultaneously call kind_list on a fresh persona — loader should run exactly once.
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let persona = "race_persona";
        let persona_dir = tmp.path().join(persona);
        std::fs::create_dir_all(&persona_dir).unwrap();
        std::fs::write(
            persona_dir.join(".journal.toml"),
            r#"
[[kinds]]
kind = "race_kind"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []
"#
            .trim(),
        )
        .unwrap();

        let canonical_root = tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| tmp.path().to_path_buf());
        let root = tmp.path().to_path_buf();
        let n = 4;
        let barrier = Arc::new(Barrier::new(n));

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let root = root.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let j = Journal::open(root);
                    j.kind_list(persona)
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap().unwrap();
        }

        // Only 1 fs IO should have occurred across all 4 threads.
        let count = crate::loader::load_counter_get(&canonical_root, persona);
        assert_eq!(
            count, 1,
            "concurrent first-open: LOAD_COUNTER should be 1 for this (root, persona), got {count}"
        );
    }

    #[test]
    fn test_reload_during_say() {
        // Thread A: 50 say() calls; Thread B: 5 reload_kinds() calls.
        // Expect no deadlock and all Ok results.
        use std::sync::Arc;
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let persona = "reload_say_persona";
        let persona_dir = tmp.path().join(persona);
        std::fs::create_dir_all(&persona_dir).unwrap();
        std::fs::write(
            persona_dir.join(".journal.toml"),
            r#"
[[kinds]]
kind = "emo"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []
"#
            .trim(),
        )
        .unwrap();

        let j = Arc::new(Journal::open(tmp.path().to_path_buf()));
        // Initial load.
        j.kind_list(persona).unwrap();

        let j_a = Arc::clone(&j);
        let j_b = Arc::clone(&j);
        let persona_a = persona.to_string();
        let persona_b = persona.to_string();

        let h_a = thread::spawn(move || {
            for i in 0..50 {
                j_a.say(&persona_a, "emo", &format!("entry {i}"), vec![])
                    .unwrap();
            }
        });
        let h_b = thread::spawn(move || {
            for _ in 0..5 {
                j_b.reload_kinds(&persona_b).unwrap();
            }
        });

        h_a.join().unwrap();
        h_b.join().unwrap();
    }

    #[test]
    fn test_lock_ordering_loaded_then_db() {
        // 2 threads: A calls ensure_loaded (via kind_list), B calls kind_register.
        // Both must complete within 5 seconds without deadlock.
        use std::sync::{Arc, Barrier};
        use std::thread;
        use std::time::Duration;

        let tmp = TempDir::new().unwrap();
        let persona = "lockorder_persona";
        let root = tmp.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));

        let root_a = root.clone();
        let barrier_a = Arc::clone(&barrier);
        let h_a = thread::spawn(move || {
            barrier_a.wait();
            let j = Journal::open(root_a);
            j.kind_list(persona)
        });

        let root_b = root.clone();
        let barrier_b = Arc::clone(&barrier);
        let h_b = thread::spawn(move || {
            barrier_b.wait();
            let j = Journal::open(root_b);
            let cfg = KindConfig::preset_emo();
            j.kind_register(persona, &cfg)
        });

        // 5-second timeout via channel trick.
        let (tx_a, rx_a) = std::sync::mpsc::channel();
        let (tx_b, rx_b) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx_a.send(h_a.join());
        });
        thread::spawn(move || {
            let _ = tx_b.send(h_b.join());
        });

        let res_a = rx_a
            .recv_timeout(Duration::from_secs(5))
            .expect("thread A deadlocked (>5s)");
        let res_b = rx_b
            .recv_timeout(Duration::from_secs(5))
            .expect("thread B deadlocked (>5s)");

        res_a.unwrap().unwrap();
        res_b.unwrap().unwrap();
    }

    #[test]
    fn test_mutex_poison_propagation() {
        // Verify that the `map_err` wrapper in loaded_cache converts a poisoned
        // Mutex error into Error::Invalid("loaded cache lock poisoned...").
        //
        // We use a *local* Mutex to demonstrate the error-propagation path
        // without touching the process-global LOADED OnceLock, which would
        // cascade failures into every other test running in the same process.
        use std::thread;

        let local: std::sync::Arc<std::sync::Mutex<()>> =
            std::sync::Arc::new(std::sync::Mutex::new(()));

        let local2 = std::sync::Arc::clone(&local);
        let h = thread::spawn(move || {
            let _guard = local2.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = h.join(); // Err(panic) — local is now poisoned.

        // Simulate the loaded_cache map_err path: lock().map_err(|e| Error::Invalid(...))
        let result: Result<()> = local
            .lock()
            .map_err(|e| Error::Invalid(format!("loaded cache lock poisoned: {e}")))
            .map(|_guard| ());

        match result {
            Err(Error::Invalid(msg)) => {
                assert!(
                    msg.contains("loaded cache lock poisoned"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Error::Invalid with poison message, got: {other:?}"),
        }
    }

    #[test]
    fn test_arc_mutex_db_send_sync() {
        // Compile-time assertion: Arc<Mutex<Db>> is Send + Sync.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<Mutex<crate::db::Db>>>();
    }

    #[test]
    fn test_1000_calls_fast_path() {
        let tmp = TempDir::new().unwrap();
        let persona = "fast_path_persona";
        let persona_dir = tmp.path().join(persona);
        std::fs::create_dir_all(&persona_dir).unwrap();
        std::fs::write(
            persona_dir.join(".journal.toml"),
            r#"
[[kinds]]
kind = "fast_kind"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []
"#
            .trim(),
        )
        .unwrap();

        let canonical_root = tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| tmp.path().to_path_buf());
        let j = Journal::open(tmp.path().to_path_buf());
        for _ in 0..1000 {
            j.kind_list(persona).unwrap();
        }

        let count = crate::loader::load_counter_get(&canonical_root, persona);
        assert_eq!(
            count, 1,
            "1000 kind_list calls: LOAD_COUNTER should be 1 for this (root, persona), got {count}"
        );
    }

    /// Crux evidence: `query_by_retrieval` orders by retrieval score, and the score ratio
    /// for a 1-day-old vs 30-day-old entry under half_life_days=1.0 is ~555:1.
    ///
    /// Verifies crux constraints:
    ///  - Decay formula drives ORDER BY (not insertion order / created_at DESC)
    ///  - Test asserts ordering and score ratio (not just row count)
    #[test]
    fn query_by_retrieval_decay_ordering_and_ratio() {
        use crate::schema::{DecayConfig, KindConfig, KindMode};
        use time::Duration;

        let tmp = TempDir::new().unwrap();
        let persona = "alice";
        let kind = "test_decay";

        let j = Journal::open(tmp.path().to_path_buf());

        // Register a kind with half_life_days=1.0 so the 1-day-old entry scores ~0.5
        // and the 30-day-old entry scores ~0.0009 (ratio ~555:1).
        let cfg = KindConfig {
            kind: kind.to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig {
                half_life_days: 1.0,
                weight: 1.0,
            },
            boost_factor: 1.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        j.kind_register(persona, &cfg).unwrap();

        // Create both entries (say records created_at = now internally)
        let id_new = j.say(persona, kind, "# new entry", vec![]).unwrap();
        let id_old = j.say(persona, kind, "# old entry", vec![]).unwrap();

        // Reference timestamp: use now_utc so the "new" entry age is ~0 days.
        let now = OffsetDateTime::now_utc();

        // Back-date id_old to 30 days ago via the existing DB connection.
        let ts_30d_ago = now - Duration::days(30);
        let ts_30d_str = ts_30d_ago
            .format(&Rfc3339)
            .expect("Rfc3339 format must not fail");

        // Access the DB via db_cache (same Arc<Mutex<Db>> that Journal uses).
        let db_arc = j.open_db(persona).unwrap();
        let db = db_arc.lock().unwrap();
        db.set_created_at_for_test(&id_old, &ts_30d_str).unwrap();
        drop(db);

        // query_by_retrieval must return [id_new, id_old] — decay score drives ORDER BY.
        let rows = j.query_by_retrieval(persona, kind, 10, now).unwrap();
        assert_eq!(rows.len(), 2, "expected 2 rows from query_by_retrieval");
        assert_eq!(
            rows[0].id, id_new,
            "1-day-old entry must rank first (higher decay score)"
        );
        assert_eq!(
            rows[1].id, id_old,
            "30-day-old entry must rank second (lower decay score)"
        );

        // Score ratio assertion: score_new / score_old must be >> 100.
        // Theoretical: score_new ≈ exp(-ln(2)*~0/1.0) ≈ 1.0,
        //              score_old ≈ exp(-ln(2)*30/1.0) ≈ 0.0009, ratio ≈ 1000+.
        let now_iso = now.format(&Rfc3339).unwrap();
        let db2 = db_arc.lock().unwrap();
        let scored = db2
            .query_by_retrieval_with_scores(kind, 10, &now_iso)
            .unwrap();
        assert_eq!(scored.len(), 2);
        let (_, score_new) = &scored[0];
        let (_, score_old) = &scored[1];
        assert!(
            score_new / score_old > 100.0,
            "expected score_new/score_old > 100, got {}",
            score_new / score_old
        );
        // Verify absolute values are in expected range
        assert!(
            *score_new > 0.1,
            "score_new should be > 0.1, got {score_new}"
        );
        assert!(
            *score_old < 0.01,
            "score_old should be < 0.01, got {score_old}"
        );
    }

    // ── query_by_retrieval 4-factor probe tests ──────────────────────────────

    /// Test #1: retrieval_strength influences ordering (higher strength → ranked first).
    #[test]
    fn query_by_retrieval_strength_influences_order() {
        use crate::schema::{DecayConfig, KindConfig, KindMode};

        let tmp = TempDir::new().unwrap();
        let persona = "alice";
        let kind = "test_strength_order";

        let j = Journal::open(tmp.path().to_path_buf());

        let cfg = KindConfig {
            kind: kind.to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig {
                half_life_days: 1.0,
                weight: 1.0,
            },
            boost_factor: 1.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        j.kind_register(persona, &cfg).unwrap();

        let id_low = j
            .say(persona, kind, "# low strength entry", vec![])
            .unwrap();
        let id_high = j
            .say(persona, kind, "# high strength entry", vec![])
            .unwrap();

        // Set same created_at so age is equal — only retrieval_strength differs.
        let now = OffsetDateTime::now_utc();
        let now_iso = now.format(&Rfc3339).expect("Rfc3339 format must not fail");
        {
            let db_arc = j.open_db(persona).unwrap();
            let db = db_arc.lock().unwrap();
            db.set_created_at_for_test(&id_low, &now_iso).unwrap();
            db.set_created_at_for_test(&id_high, &now_iso).unwrap();
        }

        j.set_retrieval_strength(persona, &id_low, 0.5).unwrap();
        j.set_retrieval_strength(persona, &id_high, 1.0).unwrap();

        let rows = j.query_by_retrieval(persona, kind, 10, now).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].id, id_high,
            "higher retrieval_strength must rank first"
        );
        assert_eq!(
            rows[1].id, id_low,
            "lower retrieval_strength must rank second"
        );
    }

    /// Test #2: boost_factor doubles score (Crux violation detection — boost_factor factor).
    ///
    /// Two personas (each their own DB) carry the same kind name but with different
    /// boost_factor values (2.0 vs 1.0). Score ratio must be ≈ 2.0.
    #[test]
    fn query_by_retrieval_boost_factor_doubles_score() {
        use crate::schema::{DecayConfig, KindConfig, KindMode};

        let tmp = TempDir::new().unwrap();
        let persona_high = "alice_high";
        let persona_low = "alice_low";
        let kind = "test_boost";

        let j = Journal::open(tmp.path().to_path_buf());

        let cfg_high = KindConfig {
            kind: kind.to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig {
                half_life_days: 1.0,
                weight: 1.0,
            },
            boost_factor: 2.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        let cfg_low = KindConfig {
            kind: kind.to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig {
                half_life_days: 1.0,
                weight: 1.0,
            },
            boost_factor: 1.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        // Each persona gets its own DB — no ID collision.
        j.kind_register(persona_high, &cfg_high).unwrap();
        j.kind_register(persona_low, &cfg_low).unwrap();

        let id_high = j
            .say(persona_high, kind, "# high boost entry", vec![])
            .unwrap();
        let id_low = j
            .say(persona_low, kind, "# low boost entry", vec![])
            .unwrap();

        // Equalise created_at so age decay is identical.
        let now = OffsetDateTime::now_utc();
        let now_iso = now.format(&Rfc3339).expect("Rfc3339 format must not fail");
        {
            let db_arc_high = j.open_db(persona_high).unwrap();
            let db = db_arc_high.lock().unwrap();
            db.set_created_at_for_test(&id_high, &now_iso).unwrap();
        }
        {
            let db_arc_low = j.open_db(persona_low).unwrap();
            let db = db_arc_low.lock().unwrap();
            db.set_created_at_for_test(&id_low, &now_iso).unwrap();
        }

        let scored_high = {
            let db_arc = j.open_db(persona_high).unwrap();
            let db = db_arc.lock().unwrap();
            db.query_by_retrieval_with_scores(kind, 10, &now_iso)
                .unwrap()
        };
        let scored_low = {
            let db_arc = j.open_db(persona_low).unwrap();
            let db = db_arc.lock().unwrap();
            db.query_by_retrieval_with_scores(kind, 10, &now_iso)
                .unwrap()
        };

        assert_eq!(scored_high.len(), 1);
        assert_eq!(scored_low.len(), 1);
        let (_, score_high) = &scored_high[0];
        let (_, score_low) = &scored_low[0];

        let ratio = score_high / score_low;
        assert!(
            (ratio - 2.0).abs() < 0.1,
            "expected score_high / score_low ≈ 2.0 (boost_factor doubles score), got {ratio}"
        );
    }

    /// Test #3: retrieval_strength=0.0 ranks last and scores exactly 0.0
    /// (Crux violation detection — retrieval_strength factor / Boundary).
    #[test]
    fn query_by_retrieval_zero_strength_ranks_last() {
        use crate::schema::{DecayConfig, KindConfig, KindMode};

        let tmp = TempDir::new().unwrap();
        let persona = "alice";
        let kind = "test_zero_strength";

        let j = Journal::open(tmp.path().to_path_buf());

        let cfg = KindConfig {
            kind: kind.to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig {
                half_life_days: 1.0,
                weight: 1.0,
            },
            boost_factor: 1.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        j.kind_register(persona, &cfg).unwrap();

        let id_zero = j
            .say(persona, kind, "# zero strength entry", vec![])
            .unwrap();
        let id_mid = j
            .say(persona, kind, "# mid strength entry", vec![])
            .unwrap();
        let id_full = j
            .say(persona, kind, "# full strength entry", vec![])
            .unwrap();

        // Equalise created_at so only retrieval_strength determines rank.
        let now = OffsetDateTime::now_utc();
        let now_iso = now.format(&Rfc3339).expect("Rfc3339 format must not fail");
        {
            let db_arc = j.open_db(persona).unwrap();
            let db = db_arc.lock().unwrap();
            db.set_created_at_for_test(&id_zero, &now_iso).unwrap();
            db.set_created_at_for_test(&id_mid, &now_iso).unwrap();
            db.set_created_at_for_test(&id_full, &now_iso).unwrap();
        }

        j.set_retrieval_strength(persona, &id_zero, 0.0).unwrap();
        j.set_retrieval_strength(persona, &id_mid, 0.5).unwrap();
        j.set_retrieval_strength(persona, &id_full, 1.0).unwrap();

        let rows = j.query_by_retrieval(persona, kind, 10, now).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, id_full, "strength=1.0 must rank first");
        assert_eq!(rows[1].id, id_mid, "strength=0.5 must rank second");
        assert_eq!(rows[2].id, id_zero, "strength=0.0 must rank last");

        // score for zero-strength entry must be exactly 0.0.
        let db_arc = j.open_db(persona).unwrap();
        let db = db_arc.lock().unwrap();
        let scored = db
            .query_by_retrieval_with_scores(kind, 10, &now_iso)
            .unwrap();
        let score_zero = scored
            .iter()
            .find(|(row, _)| row.id == id_zero)
            .map(|(_, s)| *s)
            .expect("id_zero must be in scored results");
        assert_eq!(
            score_zero, 0.0,
            "retrieval_strength=0.0 must produce score=0.0"
        );
    }

    /// Test #4: n=0 returns empty result (boundary).
    #[test]
    fn query_by_retrieval_n_zero_returns_empty() {
        use crate::schema::{DecayConfig, KindConfig, KindMode};

        let tmp = TempDir::new().unwrap();
        let persona = "alice";
        let kind = "test_n_zero";

        let j = Journal::open(tmp.path().to_path_buf());

        let cfg = KindConfig {
            kind: kind.to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig {
                half_life_days: 1.0,
                weight: 1.0,
            },
            boost_factor: 1.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        j.kind_register(persona, &cfg).unwrap();
        j.say(persona, kind, "# an entry", vec![]).unwrap();

        let now = OffsetDateTime::now_utc();
        let rows = j.query_by_retrieval(persona, kind, 0, now).unwrap();
        assert!(rows.is_empty(), "n=0 must return empty result");
    }

    /// Test #5: n larger than total entries returns all (boundary).
    #[test]
    fn query_by_retrieval_n_exceeds_total_returns_all() {
        use crate::schema::{DecayConfig, KindConfig, KindMode};

        let tmp = TempDir::new().unwrap();
        let persona = "alice";
        let kind = "test_n_exceeds";

        let j = Journal::open(tmp.path().to_path_buf());

        let cfg = KindConfig {
            kind: kind.to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig {
                half_life_days: 1.0,
                weight: 1.0,
            },
            boost_factor: 1.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        j.kind_register(persona, &cfg).unwrap();
        j.say(persona, kind, "# entry 1", vec![]).unwrap();
        j.say(persona, kind, "# entry 2", vec![]).unwrap();

        let now = OffsetDateTime::now_utc();
        let rows = j.query_by_retrieval(persona, kind, 100, now).unwrap();
        assert_eq!(rows.len(), 2, "n=100 with 2 entries must return all 2");
    }

    // ── retrieval_strength tests ─────────────────────────────────────────────

    /// T1 (migration happy path): new entries get retrieval_strength == 1.0 by default.
    #[test]
    fn retrieval_strength_default_is_1_0() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let id1 = j.say("shi", "emo", "# entry 1", vec![]).unwrap();
        let id2 = j.say("shi", "emo", "# entry 2", vec![]).unwrap();
        let id3 = j.say("shi", "emo", "# entry 3", vec![]).unwrap();
        let rows = j.query_latest("shi", "emo", 10).unwrap();
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert_eq!(
                row.retrieval_strength, 1.0,
                "entry {} must have retrieval_strength == 1.0, got {}",
                row.id, row.retrieval_strength
            );
        }
        // Confirm via get_retrieval_strength as well
        assert_eq!(j.get_retrieval_strength("shi", &id1).unwrap(), 1.0);
        assert_eq!(j.get_retrieval_strength("shi", &id2).unwrap(), 1.0);
        assert_eq!(j.get_retrieval_strength("shi", &id3).unwrap(), 1.0);
    }

    /// T1 (round-trip): set_retrieval_strength persists and get_retrieval_strength reads back.
    #[test]
    fn retrieval_strength_round_trip() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let id = j.say("shi", "emo", "# round trip entry", vec![]).unwrap();
        j.set_retrieval_strength("shi", &id, 0.5).unwrap();
        let got = j.get_retrieval_strength("shi", &id).unwrap();
        assert_eq!(got, 0.5, "expected 0.5, got {got}");
    }

    /// T2 (boundary): 0.0 and 1.0 are accepted; -0.1, 1.1, and NaN are rejected.
    #[test]
    fn retrieval_strength_boundary() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let id = j.say("shi", "emo", "# boundary entry", vec![]).unwrap();

        // Accept 0.0 (lower boundary)
        j.set_retrieval_strength("shi", &id, 0.0).unwrap();
        assert_eq!(j.get_retrieval_strength("shi", &id).unwrap(), 0.0);

        // Accept 1.0 (upper boundary)
        j.set_retrieval_strength("shi", &id, 1.0).unwrap();
        assert_eq!(j.get_retrieval_strength("shi", &id).unwrap(), 1.0);

        // T3 (error paths): reject values outside [0.0, 1.0]
        let neg = j.set_retrieval_strength("shi", &id, -0.1);
        match neg {
            Err(Error::Invalid(msg)) => assert!(
                msg.contains("out of range"),
                "expected 'out of range' in error, got: {msg}"
            ),
            other => panic!("expected Error::Invalid for -0.1, got: {other:?}"),
        }

        let over = j.set_retrieval_strength("shi", &id, 1.1);
        match over {
            Err(Error::Invalid(msg)) => assert!(
                msg.contains("out of range"),
                "expected 'out of range' in error, got: {msg}"
            ),
            other => panic!("expected Error::Invalid for 1.1, got: {other:?}"),
        }

        let nan = j.set_retrieval_strength("shi", &id, f64::NAN);
        match nan {
            Err(Error::Invalid(msg)) => assert!(
                msg.contains("NaN"),
                "expected 'NaN' in error message, got: {msg}"
            ),
            other => panic!("expected Error::Invalid for NaN, got: {other:?}"),
        }
    }

    /// T3 (error path): get/set on a non-existent entry returns EntryNotFound.
    #[test]
    fn retrieval_strength_entry_not_found() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();

        let get_result = j.get_retrieval_strength("shi", "2099-01_99999");
        assert!(
            matches!(get_result, Err(Error::EntryNotFound(_))),
            "expected EntryNotFound for get on missing entry, got: {get_result:?}"
        );

        let set_result = j.set_retrieval_strength("shi", "2099-01_99999", 0.5);
        assert!(
            matches!(set_result, Err(Error::EntryNotFound(_))),
            "expected EntryNotFound for set on missing entry, got: {set_result:?}"
        );
    }

    /// T1 (migration idempotency): opening the same DB path twice must succeed.
    #[test]
    fn migration_is_idempotent() {
        use crate::db::Db;
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test_idempotent.db");
        // First open: creates table and runs migration (column already present via DDL)
        let db1 = Db::open(&db_path).unwrap();
        drop(db1);
        // Second open: PRAGMA table_info finds column, skips ALTER TABLE
        let db2 = Db::open(&db_path);
        assert!(
            db2.is_ok(),
            "second Db::open on same path must succeed (idempotent migration)"
        );
    }

    /// Regression: legacy DB lacking `kinds.tags` column must be migrated on Db::open.
    /// Reproduces a failure observed on existing persona DBs where SCHEMA_SQL had
    /// `kinds.tags` but no ALTER migration existed for pre-existing DBs.
    #[test]
    fn migration_adds_kinds_tags_to_legacy_db() {
        use crate::db::Db;
        use rusqlite::Connection;
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("legacy_kinds.db");
        // Manually craft a legacy kinds table without the `tags` column.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE kinds (
                    name TEXT PRIMARY KEY,
                    mode TEXT NOT NULL,
                    source TEXT,
                    path_template TEXT NOT NULL,
                    versioning INTEGER NOT NULL DEFAULT 0,
                    indexed INTEGER NOT NULL DEFAULT 1,
                    decay_half_life REAL NOT NULL DEFAULT 30.0,
                    decay_weight REAL NOT NULL DEFAULT 1.0,
                    body_template TEXT,
                    config_toml TEXT NOT NULL DEFAULT ''
                )",
            )
            .unwrap();
        }
        // Db::open should run the idempotent migration and add `tags` (and `boost_factor`).
        let db = Db::open(&db_path).expect("Db::open must succeed on legacy DB");
        // Confirm the migration ran: PRAGMA should now list `tags`.
        let conn = Connection::open(&db_path).unwrap();
        let mut stmt = conn.prepare("PRAGMA table_info(kinds)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(|x| x.ok())
            .collect();
        assert!(
            cols.iter().any(|c| c == "tags"),
            "kinds.tags column must be present after migration, got cols: {cols:?}"
        );
        assert!(
            cols.iter().any(|c| c == "boost_factor"),
            "kinds.boost_factor column must also be present, got cols: {cols:?}"
        );
        drop(db);
    }

    // ── pin / unpin / boost_kind tests ──────────────────────────────────────────

    /// T1 (pin happy path): pin with Some(0.5) persists retrieval_strength == 0.5.
    #[test]
    fn pin_some_sets_strength() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let id = j.say("shi", "emo", "# pin some entry", vec![]).unwrap();
        j.pin("shi", &id, Some(0.5)).unwrap();
        let got = j.get_retrieval_strength("shi", &id).unwrap();
        assert_eq!(got, 0.5, "expected 0.5 after pin(Some(0.5)), got {got}");
    }

    /// T1 (pin happy path): pin with None defaults to retrieval_strength == 1.0.
    #[test]
    fn pin_none_sets_one() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let id = j.say("shi", "emo", "# pin none entry", vec![]).unwrap();
        // First set to something else to ensure the update actually fires.
        j.set_retrieval_strength("shi", &id, 0.3).unwrap();
        j.pin("shi", &id, None).unwrap();
        let got = j.get_retrieval_strength("shi", &id).unwrap();
        assert_eq!(got, 1.0, "expected 1.0 after pin(None), got {got}");
    }

    /// T1/Crux C1 (unpin): unpin always resets retrieval_strength to 1.0 regardless of prior value.
    #[test]
    fn unpin_resets_to_one() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let id = j.say("shi", "emo", "# unpin entry", vec![]).unwrap();
        // Pre-condition: set to non-default value.
        j.set_retrieval_strength("shi", &id, 0.3).unwrap();
        assert_eq!(j.get_retrieval_strength("shi", &id).unwrap(), 0.3);
        // Crux C1: unpin must write 1.0 (neutral), not NULL or any other sentinel.
        j.unpin("shi", &id).unwrap();
        let got = j.get_retrieval_strength("shi", &id).unwrap();
        assert_eq!(
            got, 1.0,
            "unpin must reset retrieval_strength to 1.0, got {got}"
        );
    }

    /// T2/Crux C2 (boundary): boost_kind rejects factor == 0.0 with Error::Invalid.
    #[test]
    fn boost_kind_zero_rejected() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let result = j.boost_kind("shi", "emo", 0.0);
        assert!(
            matches!(result, Err(Error::Invalid(_))),
            "expected Error::Invalid for factor 0.0, got: {result:?}"
        );
    }

    /// T2/Crux C2 (boundary): boost_kind rejects factor < 0.0 with Error::Invalid.
    #[test]
    fn boost_kind_negative_rejected() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let result = j.boost_kind("shi", "emo", -1.0);
        assert!(
            matches!(result, Err(Error::Invalid(_))),
            "expected Error::Invalid for factor -1.0, got: {result:?}"
        );
    }

    /// T2/Crux C2 (boundary): boost_kind rejects NaN with Error::Invalid.
    ///
    /// IEEE 754: NaN <= 0.0 is false, so NaN must be checked explicitly to prevent persistence.
    #[test]
    fn boost_kind_nan_rejected() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let result = j.boost_kind("shi", "emo", f64::NAN);
        assert!(
            matches!(result, Err(Error::Invalid(_))),
            "expected Error::Invalid for NaN factor, got: {result:?}"
        );
    }

    /// T1 (boost_kind happy path): boost_kind with a large factor is accepted and round-trips.
    #[test]
    fn boost_kind_accepts_large() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        j.boost_kind("shi", "emo", 100.0).unwrap();
        // Read back via kind_get to verify the round-trip.
        let kind = j.kind_get("shi", "emo").unwrap().expect("emo must exist");
        assert_eq!(
            kind.boost_factor, 100.0,
            "expected boost_factor == 100.0 after boost_kind, got {}",
            kind.boost_factor
        );
    }

    /// T3 (error path): boost_kind with an unregistered kind name returns Error::UnknownKind.
    #[test]
    fn boost_kind_unknown_kind_rejected() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        j.ensure_default_kinds("shi").unwrap();
        let result = j.boost_kind("shi", "nonexistent_kind", 2.0);
        assert!(
            matches!(result, Err(Error::UnknownKind(_))),
            "expected Error::UnknownKind for unregistered kind, got: {result:?}"
        );
    }

    // ── filter tests ───────────────────────────────────────────────────────────

    /// Helper: create a Journal with a single kind and N entries of known scores.
    ///
    /// Returns `(Journal, TempDir, [id; N], OffsetDateTime)`.
    /// All entries share the same `created_at` so decay is uniform;
    /// scores are determined solely by `retrieval_strength`.
    fn setup_filter_journal(
        n: usize,
        strengths: &[f64],
        tags_for_first: Vec<String>,
    ) -> (Journal, tempfile::TempDir, Vec<String>, OffsetDateTime) {
        use crate::schema::{DecayConfig, KindConfig, KindMode};
        assert_eq!(n, strengths.len());

        let tmp = TempDir::new().unwrap();
        let persona = "alice";
        let kind = "filter_test_kind";

        let j = Journal::open(tmp.path().to_path_buf());
        let cfg = KindConfig {
            kind: kind.to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig {
                half_life_days: 1.0,
                weight: 1.0,
            },
            boost_factor: 1.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        j.kind_register(persona, &cfg).unwrap();

        let now = OffsetDateTime::now_utc();
        let now_iso = now.format(&Rfc3339).expect("Rfc3339 must not fail");

        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let entry_tags = if i == 0 {
                tags_for_first.clone()
            } else {
                vec![]
            };
            let id = j
                .say(persona, kind, &format!("entry {i}"), entry_tags)
                .unwrap();
            ids.push(id);
        }

        // Equalise created_at so decay is uniform across entries
        {
            let db_arc = j.open_db(persona).unwrap();
            let db = db_arc.lock().unwrap();
            for id in &ids {
                db.set_created_at_for_test(id, &now_iso).unwrap();
            }
        }

        // Set retrieval_strength for score control
        for (id, &s) in ids.iter().zip(strengths.iter()) {
            j.set_retrieval_strength(persona, id, s).unwrap();
        }

        (j, tmp, ids, now)
    }

    /// filter::T1 (happy path): Visible(0.5) returns only entries with score >= 0.5.
    #[test]
    fn filter_visible_returns_above_threshold() {
        let (j, _tmp, ids, now) = setup_filter_journal(4, &[0.1, 0.5, 0.7, 1.0], vec![]);
        let persona = "alice";
        let kind = "filter_test_kind";

        let rows = j
            .filter(persona, kind, FilterMode::Visible { threshold: 0.5 }, now)
            .unwrap();
        let row_ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();

        // ids[1]=0.5, ids[2]=0.7, ids[3]=1.0 qualify; ids[0]=0.1 must not
        assert!(
            !row_ids.contains(&ids[0].as_str()),
            "score=0.1 must not appear in Visible(0.5)"
        );
        assert!(
            row_ids.contains(&ids[1].as_str()),
            "score=0.5 must appear in Visible(0.5)"
        );
        assert!(
            row_ids.contains(&ids[2].as_str()),
            "score=0.7 must appear in Visible(0.5)"
        );
        assert!(
            row_ids.contains(&ids[3].as_str()),
            "score=1.0 must appear in Visible(0.5)"
        );
    }

    /// filter::T2 (happy path): Archive(0.5) returns only entries with score < 0.5.
    #[test]
    fn filter_archive_returns_below_threshold() {
        let (j, _tmp, ids, now) = setup_filter_journal(4, &[0.1, 0.5, 0.7, 1.0], vec![]);
        let persona = "alice";
        let kind = "filter_test_kind";

        let rows = j
            .filter(persona, kind, FilterMode::Archive { threshold: 0.5 }, now)
            .unwrap();
        let row_ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();

        // Only ids[0]=0.1 qualifies (< 0.5); ids[1]=0.5 must NOT appear
        assert!(
            row_ids.contains(&ids[0].as_str()),
            "score=0.1 must appear in Archive(0.5)"
        );
        assert!(
            !row_ids.contains(&ids[1].as_str()),
            "score=0.5 must NOT appear in Archive(0.5)"
        );
        assert!(
            !row_ids.contains(&ids[2].as_str()),
            "score=0.7 must NOT appear in Archive(0.5)"
        );
        assert!(
            !row_ids.contains(&ids[3].as_str()),
            "score=1.0 must NOT appear in Archive(0.5)"
        );
    }

    /// filter::T3 (invariant): Visible(t) ∪ Archive(t) == Full (disjoint partition).
    #[test]
    fn filter_visible_union_archive_equals_full() {
        use std::collections::HashSet;
        let (j, _tmp, _ids, now) = setup_filter_journal(5, &[0.0, 0.3, 0.5, 0.75, 1.0], vec![]);
        let persona = "alice";
        let kind = "filter_test_kind";

        let visible = j
            .filter(persona, kind, FilterMode::Visible { threshold: 0.5 }, now)
            .unwrap();
        let archive = j
            .filter(persona, kind, FilterMode::Archive { threshold: 0.5 }, now)
            .unwrap();
        let full = j.filter(persona, kind, FilterMode::Full, now).unwrap();

        let vis_ids: HashSet<&str> = visible.iter().map(|r| r.id.as_str()).collect();
        let arc_ids: HashSet<&str> = archive.iter().map(|r| r.id.as_str()).collect();
        let full_ids: HashSet<&str> = full.iter().map(|r| r.id.as_str()).collect();

        // Union must equal Full
        let union_ids: HashSet<&str> = vis_ids.union(&arc_ids).copied().collect();
        assert_eq!(union_ids, full_ids, "Visible ∪ Archive must equal Full");

        // Intersection must be empty
        let intersection_ids: HashSet<&str> = vis_ids.intersection(&arc_ids).copied().collect();
        assert!(
            intersection_ids.is_empty(),
            "Visible ∩ Archive must be empty (disjoint partition)"
        );
    }

    /// filter::T4 (invariant): Partial(0.5, 3) ⊇ Visible(0.5) (superset).
    #[test]
    fn filter_partial_superset_of_visible() {
        use std::collections::HashSet;
        let (j, _tmp, _ids, now) = setup_filter_journal(5, &[0.1, 0.2, 0.5, 0.7, 1.0], vec![]);
        let persona = "alice";
        let kind = "filter_test_kind";

        let visible = j
            .filter(persona, kind, FilterMode::Visible { threshold: 0.5 }, now)
            .unwrap();
        let partial = j
            .filter(
                persona,
                kind,
                FilterMode::Partial {
                    threshold: 0.5,
                    top_k: 3,
                },
                now,
            )
            .unwrap();

        let vis_ids: HashSet<&str> = visible.iter().map(|r| r.id.as_str()).collect();
        let par_ids: HashSet<&str> = partial.iter().map(|r| r.id.as_str()).collect();

        for id in &vis_ids {
            assert!(
                par_ids.contains(id),
                "Partial must contain all Visible entries; missing {id}"
            );
        }
    }

    /// filter::T5 (boundary): Partial(0.5, 0) == Visible(0.5) (top_k=0 contributes nothing extra).
    #[test]
    fn filter_partial_top_k_zero_equals_visible() {
        use std::collections::HashSet;
        let (j, _tmp, _ids, now) = setup_filter_journal(4, &[0.1, 0.5, 0.7, 1.0], vec![]);
        let persona = "alice";
        let kind = "filter_test_kind";

        let visible = j
            .filter(persona, kind, FilterMode::Visible { threshold: 0.5 }, now)
            .unwrap();
        let partial_k0 = j
            .filter(
                persona,
                kind,
                FilterMode::Partial {
                    threshold: 0.5,
                    top_k: 0,
                },
                now,
            )
            .unwrap();

        let vis_ids: HashSet<&str> = visible.iter().map(|r| r.id.as_str()).collect();
        let par_ids: HashSet<&str> = partial_k0.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(vis_ids, par_ids, "Partial(t, 0) must equal Visible(t)");
    }

    /// filter::T6 (invariant): Full == query_by_retrieval(n=usize::MAX) in id order and tags.
    #[test]
    fn filter_full_equals_query_by_retrieval() {
        let (j, _tmp, _ids, now) = setup_filter_journal(
            4,
            &[0.2, 0.8, 0.5, 1.0],
            vec!["alpha".into(), "beta".into()],
        );
        let persona = "alice";
        let kind = "filter_test_kind";

        let full = j.filter(persona, kind, FilterMode::Full, now).unwrap();
        let qbr = j
            .query_by_retrieval(persona, kind, usize::MAX, now)
            .unwrap();

        assert_eq!(
            full.len(),
            qbr.len(),
            "Full must return same count as query_by_retrieval(usize::MAX)"
        );
        for (i, (f, q)) in full.iter().zip(qbr.iter()).enumerate() {
            assert_eq!(
                f.id, q.id,
                "Full[{i}].id must match query_by_retrieval[{i}].id"
            );
            assert_eq!(
                f.tags, q.tags,
                "Full[{i}].tags must match query_by_retrieval[{i}].tags (tags enrichment equivalence)"
            );
        }
    }

    /// filter::T7 (tags invariant): tags set on entry are preserved in all filter modes.
    #[test]
    fn filter_tags_preserved_in_all_modes() {
        let tagged_tags = vec!["alpha".into(), "beta".into()];
        let (j, _tmp, ids, now) = setup_filter_journal(3, &[0.8, 0.3, 0.6], tagged_tags.clone());
        let persona = "alice";
        let kind = "filter_test_kind";

        // ids[0] has score=0.8 (visible with t=0.5), ids[1] has score=0.3 (archive with t=0.5)
        let tagged_id = &ids[0];

        // Full
        let full = j.filter(persona, kind, FilterMode::Full, now).unwrap();
        let full_tagged = full.iter().find(|r| &r.id == tagged_id).unwrap();
        assert_eq!(
            full_tagged.tags, tagged_tags,
            "Full: tags must be preserved"
        );

        // Visible (ids[0] score=0.8 >= 0.5)
        let visible = j
            .filter(persona, kind, FilterMode::Visible { threshold: 0.5 }, now)
            .unwrap();
        let vis_tagged = visible.iter().find(|r| &r.id == tagged_id).unwrap();
        assert_eq!(
            vis_tagged.tags, tagged_tags,
            "Visible: tags must be preserved"
        );

        // Archive (ids[1] score=0.3 < 0.5; check we can still get tags from another archive entry)
        // Use Archive with threshold=1.0 so ids[0] (score=0.8) also goes to archive
        let archive = j
            .filter(persona, kind, FilterMode::Archive { threshold: 1.0 }, now)
            .unwrap();
        let arc_tagged = archive.iter().find(|r| &r.id == tagged_id).unwrap();
        assert_eq!(
            arc_tagged.tags, tagged_tags,
            "Archive: tags must be preserved"
        );

        // Partial (ids[0] is in visible set and top_k=3)
        let partial = j
            .filter(
                persona,
                kind,
                FilterMode::Partial {
                    threshold: 0.5,
                    top_k: 3,
                },
                now,
            )
            .unwrap();
        let par_tagged = partial.iter().find(|r| &r.id == tagged_id).unwrap();
        assert_eq!(
            par_tagged.tags, tagged_tags,
            "Partial: tags must be preserved"
        );
    }

    /// filter::T8 (boundary): threshold=0.0 → Visible=Full, Archive=∅.
    #[test]
    fn filter_threshold_zero_visible_equals_full() {
        let (j, _tmp, _ids, now) = setup_filter_journal(3, &[0.0, 0.5, 1.0], vec![]);
        let persona = "alice";
        let kind = "filter_test_kind";

        let visible = j
            .filter(persona, kind, FilterMode::Visible { threshold: 0.0 }, now)
            .unwrap();
        let archive = j
            .filter(persona, kind, FilterMode::Archive { threshold: 0.0 }, now)
            .unwrap();
        let full = j.filter(persona, kind, FilterMode::Full, now).unwrap();

        assert_eq!(
            visible.len(),
            full.len(),
            "Visible(0.0) must equal Full in count"
        );
        assert!(archive.is_empty(), "Archive(0.0) must be empty");
    }

    /// filter::T9 (boundary): top_k > total → Partial == Full.
    #[test]
    fn filter_partial_top_k_exceeds_total_equals_full() {
        use std::collections::HashSet;
        let (j, _tmp, _ids, now) = setup_filter_journal(4, &[0.1, 0.3, 0.6, 0.9], vec![]);
        let persona = "alice";
        let kind = "filter_test_kind";

        let partial = j
            .filter(
                persona,
                kind,
                FilterMode::Partial {
                    threshold: 0.5,
                    top_k: 100,
                },
                now,
            )
            .unwrap();
        let full = j.filter(persona, kind, FilterMode::Full, now).unwrap();

        let par_ids: HashSet<&str> = partial.iter().map(|r| r.id.as_str()).collect();
        let full_ids: HashSet<&str> = full.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            par_ids, full_ids,
            "Partial with top_k > total must equal Full (set equality)"
        );
    }

    /// filter::T10 (boundary): empty kind returns Vec::new() for all modes.
    #[test]
    fn filter_empty_kind_returns_empty() {
        use crate::schema::{DecayConfig, KindConfig, KindMode};

        let tmp = TempDir::new().unwrap();
        let persona = "alice";
        let kind = "empty_kind";

        let j = Journal::open(tmp.path().to_path_buf());
        let cfg = KindConfig {
            kind: kind.to_string(),
            mode: KindMode::Entries,
            source: None,
            path_template: "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md".to_string(),
            versioning: true,
            indexed: true,
            decay: DecayConfig {
                half_life_days: 1.0,
                weight: 1.0,
            },
            boost_factor: 1.0,
            tags: vec![],
            body_template: None,
            config_toml: String::new(),
        };
        j.kind_register(persona, &cfg).unwrap();

        let now = OffsetDateTime::now_utc();

        let full = j.filter(persona, kind, FilterMode::Full, now).unwrap();
        assert!(full.is_empty(), "Full on empty kind must return empty");

        let visible = j
            .filter(persona, kind, FilterMode::Visible { threshold: 0.5 }, now)
            .unwrap();
        assert!(
            visible.is_empty(),
            "Visible on empty kind must return empty"
        );

        let archive = j
            .filter(persona, kind, FilterMode::Archive { threshold: 0.5 }, now)
            .unwrap();
        assert!(
            archive.is_empty(),
            "Archive on empty kind must return empty"
        );

        let partial = j
            .filter(
                persona,
                kind,
                FilterMode::Partial {
                    threshold: 0.5,
                    top_k: 3,
                },
                now,
            )
            .unwrap();
        assert!(
            partial.is_empty(),
            "Partial on empty kind must return empty"
        );
    }

    /// filter::T11 (error boundary): threshold=NaN returns Err(Invalid) for Visible/Partial/Archive.
    #[test]
    fn filter_threshold_nan_rejected() {
        let (j, _tmp, _ids, now) = setup_filter_journal(2, &[0.5, 1.0], vec![]);
        let persona = "alice";
        let kind = "filter_test_kind";

        let r1 = j.filter(
            persona,
            kind,
            FilterMode::Visible {
                threshold: f64::NAN,
            },
            now,
        );
        assert!(
            matches!(r1, Err(Error::Invalid(_))),
            "Visible(NaN) must return Err(Invalid), got: {r1:?}"
        );

        let r2 = j.filter(
            persona,
            kind,
            FilterMode::Archive {
                threshold: f64::NAN,
            },
            now,
        );
        assert!(
            matches!(r2, Err(Error::Invalid(_))),
            "Archive(NaN) must return Err(Invalid), got: {r2:?}"
        );

        let r3 = j.filter(
            persona,
            kind,
            FilterMode::Partial {
                threshold: f64::NAN,
                top_k: 2,
            },
            now,
        );
        assert!(
            matches!(r3, Err(Error::Invalid(_))),
            "Partial(NaN) must return Err(Invalid), got: {r3:?}"
        );
    }

    /// filter::T12 (error boundary): threshold < 0.0 returns Err(Invalid) for Visible/Partial/Archive.
    #[test]
    fn filter_threshold_negative_rejected() {
        let (j, _tmp, _ids, now) = setup_filter_journal(2, &[0.5, 1.0], vec![]);
        let persona = "alice";
        let kind = "filter_test_kind";

        let r1 = j.filter(persona, kind, FilterMode::Visible { threshold: -0.1 }, now);
        assert!(
            matches!(r1, Err(Error::Invalid(_))),
            "Visible(-0.1) must return Err(Invalid), got: {r1:?}"
        );

        let r2 = j.filter(persona, kind, FilterMode::Archive { threshold: -1.0 }, now);
        assert!(
            matches!(r2, Err(Error::Invalid(_))),
            "Archive(-1.0) must return Err(Invalid), got: {r2:?}"
        );

        let r3 = j.filter(
            persona,
            kind,
            FilterMode::Partial {
                threshold: -0.5,
                top_k: 1,
            },
            now,
        );
        assert!(
            matches!(r3, Err(Error::Invalid(_))),
            "Partial(-0.5) must return Err(Invalid), got: {r3:?}"
        );
    }

    /// filter::T13 (boundary): Full with 10 entries returns all 10 (usize::MAX→i64 cast works).
    #[test]
    fn filter_full_returns_all_entries() {
        let strengths: Vec<f64> = (0..10).map(|i| i as f64 * 0.1).collect();
        let (j, _tmp, _ids, now) = setup_filter_journal(10, &strengths, vec![]);
        let persona = "alice";
        let kind = "filter_test_kind";

        let full = j.filter(persona, kind, FilterMode::Full, now).unwrap();
        assert_eq!(full.len(), 10, "Full must return all 10 entries");
    }

    // ── archive_index_render tests ──────────────────────────────────────────────

    /// T_archive_index_render_unregistered (Crux 3): returns Err(UnknownKind("archive"))
    /// when the archive kind is not registered.
    #[test]
    fn archive_index_render_unregistered_returns_unknown_kind() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        let persona = "alice";
        // Do NOT register archive kind — only ensure_default_kinds registers emo.
        j.ensure_default_kinds(persona).unwrap();

        let result = j.archive_index_render(persona);
        assert!(
            matches!(result, Err(Error::UnknownKind(ref s)) if s == "archive"),
            "expected Err(UnknownKind(\"archive\")), got: {result:?}"
        );
    }

    /// T_archive_index_render_empty (Crux 2): when no archive entries exist
    /// (filter returns empty Vec), the output contains only the title header,
    /// column header, and separator — no body rows.
    #[test]
    fn archive_index_render_empty_returns_header_only() {
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        let persona = "alice";

        // Register archive kind — no entries inserted.
        let archive_cfg = crate::schema::KindConfig::preset_archive();
        j.kind_register(persona, &archive_cfg).unwrap();

        let output = j.archive_index_render(persona).unwrap();

        // Title header and table header + separator must be present (Crux 2: 0-entry contract).
        assert!(
            output.contains("# alice_archive_index"),
            "output must contain title header: {output}"
        );
        assert!(
            output.contains(
                "| created_at | kind | entry_id | retrieval_strength | body (head 80 chars) |"
            ),
            "output must contain column header: {output}"
        );
        assert!(
            output.contains("|---|---|---|---|---|"),
            "output must contain separator: {output}"
        );
        // Count lines with '|' as row delimiter — only 2 (header + separator), no body rows.
        let pipe_lines: Vec<&str> = output.lines().filter(|l| l.starts_with('|')).collect();
        assert_eq!(
            pipe_lines.len(),
            2,
            "expected exactly 2 table lines (header + separator), got {}: {output}",
            pipe_lines.len()
        );
    }

    /// T_archive_index_render_happy (Crux 1 + 2): entries inserted with kind="archive" and
    /// low retrieval_strength (score < 0.01) appear in the archive table with correct column
    /// ordering and created_at DESC sort.
    ///
    /// Note: `say()` rejects NamedIndex kinds, so entries are inserted directly via
    /// `db.insert_entry()`. The archive kind is registered so the JOIN in
    /// `query_by_retrieval_with_scores` resolves correctly.
    #[test]
    fn archive_index_render_happy_path() {
        use time::Duration;
        let tmp = TempDir::new().unwrap();
        let j = Journal::open(tmp.path().to_path_buf());
        let persona = "alice";

        // Register archive kind (NamedIndex, decay {365.0, 0.5}).
        let archive_cfg = crate::schema::KindConfig::preset_archive();
        j.kind_register(persona, &archive_cfg).unwrap();

        let now = OffsetDateTime::now_utc();
        let now_iso = now.format(&Rfc3339).expect("Rfc3339 format must not fail");
        let ts_365d_ago = now - Duration::days(365);
        let ts_365d_str = ts_365d_ago
            .format(&Rfc3339)
            .expect("Rfc3339 format must not fail");

        // Insert two entries directly with kind="archive" — bypassing say() which
        // rejects NamedIndex mode. The archive kind is registered so the JOIN in
        // query_by_retrieval_with_scores resolves correctly.
        let id_recent = "archive-recent-test-entry";
        let id_old = "archive-old-test-entry";
        {
            let db_arc = j.open_db(persona).unwrap();
            let db = db_arc.lock().unwrap();
            db.insert_entry(id_recent, "archive", &now_iso, Some("recent summary"), &[])
                .unwrap();
            db.insert_entry(id_old, "archive", &ts_365d_str, Some("old summary"), &[])
                .unwrap();
        }

        // Set both to low retrieval_strength so score < 0.01.
        j.set_retrieval_strength(persona, id_recent, 0.005).unwrap();
        j.set_retrieval_strength(persona, id_old, 0.005).unwrap();

        let output = j.archive_index_render(persona).unwrap();

        // Title header must be present.
        assert!(
            output.contains("# alice_archive_index"),
            "output must contain title header: {output}"
        );
        // Column header with correct 5-column order (Crux 2).
        assert!(
            output.contains(
                "| created_at | kind | entry_id | retrieval_strength | body (head 80 chars) |"
            ),
            "output must contain 5-column header in correct order: {output}"
        );
        // Both archive-eligible entries must appear.
        assert!(
            output.contains(id_recent),
            "recent entry must appear in archive output: {output}"
        );
        assert!(
            output.contains(id_old),
            "old entry must appear in archive output: {output}"
        );
        // id_recent (created_at = now) must appear before id_old (created_at = 365d ago)
        // because output is sorted by created_at DESC.
        let pos_recent = output.find(id_recent).expect("id_recent must be in output");
        let pos_old = output.find(id_old).expect("id_old must be in output");
        assert!(
            pos_recent < pos_old,
            "recent entry (created_at=now) must appear before old entry (created_at=365d ago) in DESC order"
        );
    }
}
