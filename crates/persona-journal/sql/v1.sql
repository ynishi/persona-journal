
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
    uname             TEXT NOT NULL UNIQUE,
    kind              TEXT NOT NULL REFERENCES kinds(name),
    seq_in_kind       TEXT NOT NULL,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    current_version   INTEGER NOT NULL DEFAULT 1,
    first_line_cache  TEXT,
    retrieval_strength REAL NOT NULL DEFAULT 1.0,
    CHECK(uname = kind || '/' || seq_in_kind)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_entries_kind_seq ON entries(kind, seq_in_kind);
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
