//! SQLite meta layer. One `_journal.db` per persona under `<root>/<persona>/`.

use rusqlite::{functions::FunctionFlags, params, Connection, OptionalExtension, Transaction};
use std::path::Path;

use crate::error::Result;
use crate::schema::{DecayConfig, KindConfig, KindMode, NamedSource};

pub struct Db {
    conn: Connection,
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS kinds (
    name           TEXT PRIMARY KEY,
    mode           TEXT NOT NULL,
    source         TEXT,
    path_template  TEXT NOT NULL,
    versioning     INTEGER NOT NULL,
    indexed        INTEGER NOT NULL,
    decay_half_life REAL NOT NULL,
    decay_weight    REAL NOT NULL,
    body_template  TEXT,
    config_toml    TEXT NOT NULL,
    tags           TEXT NOT NULL DEFAULT '[]',
    boost_factor   REAL NOT NULL DEFAULT 1.0
);

CREATE TABLE IF NOT EXISTS entries (
    id                TEXT PRIMARY KEY,
    kind              TEXT NOT NULL REFERENCES kinds(name),
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    current_version   INTEGER NOT NULL DEFAULT 1,
    first_line_cache  TEXT,
    retrieval_strength REAL NOT NULL DEFAULT 1.0
);

CREATE INDEX IF NOT EXISTS idx_entries_kind_created ON entries(kind, created_at DESC);

CREATE TABLE IF NOT EXISTS tags (
    entry_id TEXT NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    tag      TEXT NOT NULL,
    PRIMARY KEY(entry_id, tag)
);

CREATE INDEX IF NOT EXISTS idx_tags_tag ON tags(tag);

CREATE TABLE IF NOT EXISTS tag_history (
    entry_id TEXT NOT NULL,
    tag      TEXT NOT NULL,
    op       TEXT NOT NULL CHECK(op IN ('add','remove')),
    ts       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS versions (
    entry_id     TEXT NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    version      INTEGER NOT NULL,
    body         TEXT NOT NULL,
    file_path    TEXT NOT NULL,
    content_hash TEXT,
    ts           TEXT NOT NULL,
    PRIMARY KEY(entry_id, version)
);
"#;

#[derive(Debug, Clone)]
pub struct EntryMetaRow {
    pub id: String,
    pub kind: String,
    pub created_at: String,
    pub updated_at: String,
    pub current_version: u32,
    pub first_line_cache: Option<String>,
    pub retrieval_strength: f64,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct VersionRow {
    pub entry_id: String,
    pub version: u32,
    pub body: String,
    pub file_path: String,
    pub kind: String,
    pub ts: String,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA_SQL)?;
        // Idempotent migration: add retrieval_strength column to pre-existing DBs.
        // PRAGMA table_info returns one row per column; column index 1 is the name.
        // SQLite DEFAULT handling means no full row rewrite occurs; existing rows
        // read back 1.0 transparently without blocking concurrent reads.
        {
            let mut stmt = conn.prepare("PRAGMA table_info(entries)")?;
            let exists = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|x| x.ok())
                .any(|name| name == "retrieval_strength");
            if !exists {
                conn.execute_batch(
                    "ALTER TABLE entries ADD COLUMN retrieval_strength REAL NOT NULL DEFAULT 1.0",
                )?;
            }
        }
        // Idempotent migration: add boost_factor column to pre-existing kinds tables.
        // Same pattern as retrieval_strength above; existing rows read back 1.0 via DEFAULT.
        {
            let mut stmt = conn.prepare("PRAGMA table_info(kinds)")?;
            let exists = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|x| x.ok())
                .any(|name| name == "boost_factor");
            if !exists {
                conn.execute_batch(
                    "ALTER TABLE kinds ADD COLUMN boost_factor REAL NOT NULL DEFAULT 1.0",
                )?;
            }
        }
        // Idempotent migration: add tags column to pre-existing kinds tables.
        // Earlier schema lacked kinds.tags; SCHEMA_SQL adds it for new DBs but legacy
        // DBs need ALTER. Existing rows read back '[]' via DEFAULT.
        {
            let mut stmt = conn.prepare("PRAGMA table_info(kinds)")?;
            let exists = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|x| x.ok())
                .any(|name| name == "tags");
            if !exists {
                conn.execute_batch("ALTER TABLE kinds ADD COLUMN tags TEXT NOT NULL DEFAULT '[]'")?;
            }
        }
        // Register host-side exp() so the Ebbinghaus decay formula in SQL ORDER BY
        // can call exp() without requiring SQLITE_ENABLE_MATH_FUNCTIONS at build time.
        conn.create_scalar_function(
            "exp",
            1,
            FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_UTF8,
            |ctx| {
                let x: f64 = ctx.get(0)?;
                Ok(x.exp())
            },
        )?;
        Ok(Self { conn })
    }

    pub fn upsert_kind(&self, k: &KindConfig) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO kinds(name, mode, source, path_template, versioning, indexed,
                              decay_half_life, decay_weight, body_template, config_toml, tags,
                              boost_factor)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ON CONFLICT(name) DO UPDATE SET
                mode = excluded.mode,
                source = excluded.source,
                path_template = excluded.path_template,
                versioning = excluded.versioning,
                indexed = excluded.indexed,
                decay_half_life = excluded.decay_half_life,
                decay_weight = excluded.decay_weight,
                body_template = excluded.body_template,
                config_toml = excluded.config_toml,
                tags = excluded.tags,
                boost_factor = excluded.boost_factor
            "#,
            params![
                k.kind,
                k.mode.as_str(),
                k.source.map(|s| s.as_str()),
                k.path_template,
                k.versioning as i64,
                k.indexed as i64,
                k.decay.half_life_days,
                k.decay.weight,
                k.body_template,
                k.config_toml,
                serde_json::to_string(&k.tags)?,
                k.boost_factor,
            ],
        )?;
        Ok(())
    }

    pub fn get_kind(&self, name: &str) -> Result<Option<KindConfig>> {
        let row = self
            .conn
            .query_row(
                // Column index mapping (0-based):
                // 0: name, 1: mode, 2: source, 3: path_template, 4: versioning, 5: indexed,
                // 6: decay_half_life, 7: decay_weight, 8: body_template, 9: config_toml,
                // 10: tags, 11: boost_factor
                "SELECT name, mode, source, path_template, versioning, indexed,
                        decay_half_life, decay_weight, body_template, config_toml, tags,
                        boost_factor
                 FROM kinds WHERE name = ?1",
                params![name],
                |r| {
                    let mode_s: String = r.get(1)?;
                    let source_s: Option<String> = r.get(2)?;
                    let tags_s: String = r.get(10)?; // idx 10: tags (unchanged)
                    let boost_factor: f64 = r.get(11)?; // idx 11: boost_factor
                    Ok((
                        KindConfig {
                            kind: r.get(0)?,
                            mode: KindMode::parse(&mode_s).unwrap_or(KindMode::Entries),
                            source: source_s.as_deref().and_then(NamedSource::parse),
                            path_template: r.get(3)?,
                            versioning: r.get::<_, i64>(4)? != 0,
                            indexed: r.get::<_, i64>(5)? != 0,
                            decay: DecayConfig {
                                half_life_days: r.get(6)?,
                                weight: r.get(7)?,
                            },
                            boost_factor,
                            tags: vec![],
                            body_template: r.get(8)?,
                            config_toml: r.get(9)?,
                        },
                        tags_s,
                    ))
                },
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((mut cfg, tags_s)) => {
                cfg.tags = serde_json::from_str::<Vec<String>>(&tags_s)?;
                Ok(Some(cfg))
            }
        }
    }

    pub fn list_kinds(&self) -> Result<Vec<KindConfig>> {
        let mut stmt = self.conn.prepare("SELECT name FROM kinds ORDER BY name")?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            if let Some(k) = self.get_kind(&n)? {
                out.push(k);
            }
        }
        Ok(out)
    }

    /// Per-month / per-kind sequence number. Within a `YYYY-MM` window, returns `MAX(NNNNN) + 1`.
    /// The id format `YYYY-MM_NNNNN` has a fixed 8-char prefix (`YYYY-MM_`).
    pub fn next_seq(&self, kind: &str, year_month: &str) -> Result<u32> {
        let like = format!("{}_%", year_month);
        let max: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(CAST(substr(id, 9) AS INTEGER))
                 FROM entries WHERE kind = ?1 AND id LIKE ?2",
                params![kind, like],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(max.map(|v| v as u32 + 1).unwrap_or(1))
    }

    /// Insert an entry row plus its tag rows and tag-history rows inside an
    /// existing transaction.
    ///
    /// # Arguments
    /// - `tx`: the active transaction owned by the caller; this helper never
    ///   opens, commits, or rolls back a transaction.
    /// - `id`: entry id
    /// - `kind`: kind name (must already be registered)
    /// - `created_at`: ISO 8601 timestamp string used for both `created_at` and
    ///   `updated_at` columns, and for the tag-history `ts` column.
    /// - `first_line`: optional summary extracted from the body
    /// - `tags`: slice of tag strings to associate with this entry
    ///
    /// # Errors
    /// Returns `Err` if any SQLite operation fails; the caller's transaction is
    /// left intact so it can be rolled back by dropping it.
    fn insert_entry_in_tx(
        &self,
        tx: &Transaction,
        id: &str,
        kind: &str,
        created_at: &str,
        first_line: Option<&str>,
        tags: &[String],
    ) -> Result<()> {
        tx.execute(
            "INSERT INTO entries(id, kind, created_at, updated_at, current_version, first_line_cache)
             VALUES (?1, ?2, ?3, ?3, 1, ?4)",
            params![id, kind, created_at, first_line],
        )?;
        for t in tags {
            tx.execute(
                "INSERT OR IGNORE INTO tags(entry_id, tag) VALUES (?1, ?2)",
                params![id, t],
            )?;
            tx.execute(
                "INSERT INTO tag_history(entry_id, tag, op, ts) VALUES (?1, ?2, 'add', ?3)",
                params![id, t, created_at],
            )?;
        }
        Ok(())
    }

    pub fn insert_entry(
        &self,
        id: &str,
        kind: &str,
        created_at: &str,
        first_line: Option<&str>,
        tags: &[String],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.insert_entry_in_tx(&tx, id, kind, created_at, first_line, tags)?;
        tx.commit()?;
        Ok(())
    }

    /// Insert a version row and update the parent entry's `current_version` and
    /// `updated_at` columns inside an existing transaction.
    ///
    /// # Arguments
    /// - `tx`: the active transaction owned by the caller; this helper never
    ///   opens, commits, or rolls back a transaction.
    /// - `entry_id`: the entry identifier this version belongs to
    /// - `version`: version number (1 for the initial version)
    /// - `body`: full entry body text stored as the DB source of truth
    /// - `file_path`: relative path hint for the filesystem projection
    /// - `ts`: ISO 8601 timestamp string used for the version `ts` column and
    ///   the entry `updated_at` column
    ///
    /// # Errors
    /// Returns `Err` if any SQLite operation fails; the caller's transaction is
    /// left intact so it can be rolled back by dropping it.
    fn add_version_in_tx(
        &self,
        tx: &Transaction,
        entry_id: &str,
        version: u32,
        body: &str,
        file_path: &str,
        ts: &str,
    ) -> Result<()> {
        tx.execute(
            "INSERT INTO versions(entry_id, version, body, file_path, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![entry_id, version as i64, body, file_path, ts],
        )?;
        tx.execute(
            "UPDATE entries SET current_version = ?1, updated_at = ?2 WHERE id = ?3",
            params![version as i64, ts, entry_id],
        )?;
        Ok(())
    }

    pub fn add_version(
        &self,
        entry_id: &str,
        version: u32,
        body: &str,
        file_path: &str,
        ts: &str,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.add_version_in_tx(&tx, entry_id, version, body, file_path, ts)?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically insert an entry and record the version with body in the DB.
    ///
    /// `body` is stored directly in `versions.body` (DB SoT). Filesystem
    /// projection is written by the caller after this call returns successfully.
    /// The transaction is scoped to DB only; no filesystem I/O occurs here.
    ///
    /// # Arguments
    /// - `id`: entry id
    /// - `kind`: kind name (must already be registered)
    /// - `created_at`: ISO 8601 timestamp string
    /// - `first_line`: optional summary extracted from body
    /// - `tags`: slice of tag strings to associate
    /// - `version`: version number (1 for new entries)
    /// - `file_path`: relative path hint for the FS projection
    /// - `body`: full entry body text (stored as DB SoT)
    ///
    /// # Errors
    /// Returns `Err` if any DB operation fails; the transaction is rolled back automatically.
    #[allow(clippy::too_many_arguments)]
    pub fn say_atomic(
        &self,
        id: &str,
        kind: &str,
        created_at: &str,
        first_line: Option<&str>,
        tags: &[String],
        version: u32,
        file_path: &str,
        body: &str,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.insert_entry_in_tx(&tx, id, kind, created_at, first_line, tags)?;
        self.add_version_in_tx(&tx, id, version, body, file_path, created_at)?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_entry(&self, id: &str) -> Result<Option<EntryMetaRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, kind, created_at, updated_at, current_version, first_line_cache,
                        retrieval_strength
                 FROM entries WHERE id = ?1",
                params![id],
                |r| {
                    Ok(EntryMetaRow {
                        id: r.get(0)?,
                        kind: r.get(1)?,
                        created_at: r.get(2)?,
                        updated_at: r.get(3)?,
                        current_version: r.get::<_, i64>(4)? as u32,
                        first_line_cache: r.get(5)?,
                        retrieval_strength: r.get(6)?,
                        tags: vec![],
                    })
                },
            )
            .optional()?;
        let Some(mut row) = row else { return Ok(None) };
        row.tags = self.tags_for(&row.id)?;
        Ok(Some(row))
    }

    pub fn tags_for(&self, entry_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tag FROM tags WHERE entry_id = ?1 ORDER BY tag")?;
        let out = stmt
            .query_map(params![entry_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub fn version_path(&self, entry_id: &str, version: u32) -> Result<Option<String>> {
        let p = self
            .conn
            .query_row(
                "SELECT file_path FROM versions WHERE entry_id = ?1 AND version = ?2",
                params![entry_id, version as i64],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(p)
    }

    /// Return the body text stored in `versions.body` for a specific entry version.
    ///
    /// # Arguments
    /// - `entry_id`: the entry identifier
    /// - `version`: the version number to look up
    ///
    /// # Returns
    /// `Ok(Some(body))` if the version exists, `Ok(None)` if not found.
    ///
    /// # Errors
    /// Returns `Err` if a SQLite error occurs.
    pub fn version_body(&self, entry_id: &str, version: u32) -> Result<Option<String>> {
        let b = self
            .conn
            .query_row(
                "SELECT body FROM versions WHERE entry_id = ?1 AND version = ?2",
                params![entry_id, version as i64],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(b)
    }

    pub fn query_latest(&self, kind: &str, n: usize) -> Result<Vec<EntryMetaRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, created_at, updated_at, current_version, first_line_cache,
                    retrieval_strength
             FROM entries WHERE kind = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![kind, n as i64], |r| {
                Ok(EntryMetaRow {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    created_at: r.get(2)?,
                    updated_at: r.get(3)?,
                    current_version: r.get::<_, i64>(4)? as u32,
                    first_line_cache: r.get(5)?,
                    retrieval_strength: r.get(6)?,
                    tags: vec![],
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for mut row in rows {
            row.tags = self.tags_for(&row.id)?;
            out.push(row);
        }
        Ok(out)
    }

    /// Return up to `n` entries for `kind`, ordered by Ebbinghaus decay score (highest first).
    ///
    /// # Decay formula
    ///
    /// ```text
    /// score = weight * exp(-ln(2) * age_days / half_life_days)
    /// ```
    ///
    /// `half_life_days` is the number of days after which the score halves.
    /// The constant `-0.6931471805599453` is `-ln(2)` in f64 precision.
    /// The `exp()` function is registered as a host-side scalar function in `Db::open`.
    ///
    /// # Arguments
    /// - `kind`: kind name (must be registered in the `kinds` table)
    /// - `n`: maximum number of rows to return
    /// - `now_iso`: current time as an RFC 3339 string; used as the reference point
    ///   for `julianday()` age computation
    ///
    /// # Errors
    /// Returns `Err(Error::Sqlite(...))` if any SQLite operation fails.
    pub fn query_by_retrieval(
        &self,
        kind: &str,
        n: usize,
        now_iso: &str,
    ) -> Result<Vec<EntryMetaRow>> {
        let mut stmt = self.conn.prepare(
            // score = retrieval_strength * boost_factor * decay_weight * exp(-ln(2) * age_days / half_life)
            "SELECT e.id, e.kind, e.created_at, e.updated_at, e.current_version, e.first_line_cache,
                    e.retrieval_strength
             FROM entries e
             JOIN kinds k ON e.kind = k.name
             WHERE e.kind = ?1
             ORDER BY (e.retrieval_strength * k.boost_factor * k.decay_weight * exp(-0.6931471805599453 * (julianday(?2) - julianday(e.created_at)) / k.decay_half_life)) DESC
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![kind, now_iso, n as i64], |r| {
                Ok(EntryMetaRow {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    created_at: r.get(2)?,
                    updated_at: r.get(3)?,
                    current_version: r.get::<_, i64>(4)? as u32,
                    first_line_cache: r.get(5)?,
                    retrieval_strength: r.get(6)?,
                    tags: vec![],
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for mut row in rows {
            row.tags = self.tags_for(&row.id)?;
            out.push(row);
        }
        Ok(out)
    }

    pub fn list_indexed_kinds(&self) -> Result<Vec<KindConfig>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM kinds WHERE indexed = 1 ORDER BY name")?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            if let Some(k) = self.get_kind(&n)? {
                out.push(k);
            }
        }
        Ok(out)
    }

    /// Return all versions across all entries, joined with their entry kind.
    ///
    /// Each row contains `entry_id`, `version`, `body`, `file_path`, `kind`, and `ts`.
    /// Used by `Journal::projection_rebuild` to regenerate the full FS projection from DB.
    ///
    /// # Returns
    /// `Ok(Vec<VersionRow>)` with all rows ordered by `entry_id, version`.
    ///
    /// # Errors
    /// Returns `Err` if a SQLite error occurs.
    pub fn list_all_versions(&self) -> Result<Vec<VersionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT v.entry_id, v.version, v.body, v.file_path, e.kind, v.ts
             FROM versions v
             JOIN entries e ON e.id = v.entry_id
             ORDER BY v.entry_id, v.version",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(VersionRow {
                    entry_id: r.get(0)?,
                    version: r.get::<_, i64>(1)? as u32,
                    body: r.get(2)?,
                    file_path: r.get(3)?,
                    kind: r.get(4)?,
                    ts: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Set the `retrieval_strength` for the entry with the given `id`.
    ///
    /// # Arguments
    /// - `id`: entry identifier (text id, e.g. `"2024-01_00001"`)
    /// - `value`: new retrieval strength; must be pre-validated by the caller
    ///   (see `Journal::set_retrieval_strength` for the validation gate)
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// Returns `Err(Error::EntryNotFound)` if no row with the given `id` exists.
    /// Returns `Err(Error::Sqlite(...))` on any other SQLite failure.
    pub fn set_retrieval_strength(&self, id: &str, value: f64) -> crate::error::Result<()> {
        let changed = self.conn.execute(
            "UPDATE entries SET retrieval_strength = ?1 WHERE id = ?2",
            params![value, id],
        )?;
        if changed == 0 {
            return Err(crate::error::Error::EntryNotFound(id.to_string()));
        }
        Ok(())
    }

    /// Get the `retrieval_strength` for the entry with the given `id`.
    ///
    /// # Arguments
    /// - `id`: entry identifier (text id, e.g. `"2024-01_00001"`)
    ///
    /// # Returns
    /// `Ok(value)` with the current retrieval strength.
    ///
    /// # Errors
    /// Returns `Err(Error::EntryNotFound)` if no row with the given `id` exists.
    /// Returns `Err(Error::Sqlite(...))` on any other SQLite failure.
    pub fn get_retrieval_strength(&self, id: &str) -> crate::error::Result<f64> {
        let value = self
            .conn
            .query_row(
                "SELECT retrieval_strength FROM entries WHERE id = ?1",
                params![id],
                |r| r.get::<_, f64>(0),
            )
            .optional()?
            .ok_or_else(|| crate::error::Error::EntryNotFound(id.to_string()))?;
        Ok(value)
    }

    /// Set the `boost_factor` for the kind with the given `name`.
    ///
    /// # Arguments
    /// - `name`: kind name (must already be registered in the `kinds` table)
    /// - `factor`: new boost factor value; must be pre-validated by the caller
    ///   (see `Journal::boost_kind` for the NaN / <= 0.0 validation gate)
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// Returns `Err(Error::UnknownKind)` if no row with the given `name` exists.
    /// Returns `Err(Error::Sqlite(...))` on any other SQLite failure.
    pub fn set_boost_factor(&self, name: &str, factor: f64) -> crate::error::Result<()> {
        let changed = self.conn.execute(
            "UPDATE kinds SET boost_factor = ?1 WHERE name = ?2",
            params![factor, name],
        )?;
        if changed == 0 {
            return Err(crate::error::Error::UnknownKind(name.to_string()));
        }
        Ok(())
    }

    /// Return entries with their retrieval scores ordered by score DESC.
    ///
    /// Same SQL as `query_by_retrieval` but also SELECTs the computed score column.
    /// Used by `Journal::filter` to partition entries by mode without reimplementing
    /// the 4-factor formula (crux: Score formula identity with ST3).
    ///
    /// Tags are enriched via `tags_for()` before returning, matching the contract of
    /// `query_by_retrieval` (every `EntryMetaRow.tags` is fully populated on return).
    pub(crate) fn query_by_retrieval_with_scores(
        &self,
        kind: &str,
        n: usize,
        now_iso: &str,
    ) -> Result<Vec<(EntryMetaRow, f64)>> {
        let mut stmt = self.conn.prepare(
            // score = retrieval_strength * boost_factor * decay_weight * exp(-ln(2) * age_days / half_life)
            "SELECT e.id, e.kind, e.created_at, e.updated_at, e.current_version, e.first_line_cache,
                    e.retrieval_strength,
                    (e.retrieval_strength * k.boost_factor * k.decay_weight * exp(-0.6931471805599453 * (julianday(?2) - julianday(e.created_at)) / k.decay_half_life)) AS score
             FROM entries e
             JOIN kinds k ON e.kind = k.name
             WHERE e.kind = ?1
             ORDER BY score DESC
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![kind, now_iso, n as i64], |r| {
                Ok((
                    EntryMetaRow {
                        id: r.get(0)?,
                        kind: r.get(1)?,
                        created_at: r.get(2)?,
                        updated_at: r.get(3)?,
                        current_version: r.get::<_, i64>(4)? as u32,
                        first_line_cache: r.get(5)?,
                        retrieval_strength: r.get(6)?,
                        tags: vec![],
                    },
                    r.get::<_, f64>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (mut row, score) in rows {
            row.tags = self.tags_for(&row.id)?;
            out.push((row, score));
        }
        Ok(out)
    }
}

#[cfg(test)]
impl Db {
    /// Test-only: overwrite `created_at` for an entry via the existing connection.
    ///
    /// Use this in tests to simulate aged entries without spawning a second connection
    /// (which could conflict with the process-global db_cache).
    pub(crate) fn set_created_at_for_test(&self, id: &str, ts: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE entries SET created_at = ?1 WHERE id = ?2",
            params![ts, id],
        )?;
        Ok(())
    }
}
