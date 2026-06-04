# persona-journal

> *Local-first, file-synced, versioned diary for a persona — backed by SQLite + (planned) restic.*

If `persona-pack` holds **persona identity (who)**,
`persona-journal` holds **persona time (what happened)**.
They form a paired noun set, linked by a shared `persona_id`.

## Position

| Concept | Main axis | Difference from persona-journal |
|---|---|---|
| **mem0 / vector-RAG** | embedding-based retrieval memory | persona-journal has no embeddings. The DB is the source of truth; FS `.md` files are a read projection |
| **character.ai / companion runtime** | dialogue runtime + persona features | persona-journal is runtime-agnostic, purely a *recording medium* |
| **Agentic RAG** | read files on demand and turn them into context | persona-journal sits on the *file side* of that pattern (retrieval itself is the agent's responsibility) |
| **Continuous AI / self-evolving memory** | self-modifying memory theory | persona-journal is the persistent layer that provides a **container that never loses** the modification history |

retrieval / embedding / self-modification are the agent's responsibility.
The journal sticks to being "a DB that never loses persona time"; FS `.md` files
are provided as a read projection regenerated from the DB.

## Core Principles

1. **DB is SoT (body + meta)** — entry body and meta are authoritative in SQLite (`versions.body`). FS `.md` files are a read projection regenerated from the DB.
2. **Persona-scoped** — every entry is bound to a `persona_id`.
3. **Append-only by default** — versioning is ON by default.
4. **Schema: Default-minimal / Extensible / Not-fat** — the shipped schema is small and fixed; user-extension keys are passed through opaquely.
5. **Backup-ready** — designed for restic + S3/B2 (planned).
6. **FS projection repair** — if FS `.md` files go stale, `projection_rebuild` regenerates all entries from the DB.

## Layout

```
<root>/                              # PERSONA_JOURNAL_ROOT (default: ~/.persona-journal/)
  <persona>/
    _journal.db                      # SQLite meta (source of truth)
    _index.md                        # RootIndex (auto, DB projection)
    <kind>/
      <seq_in_kind>.md               # versioning OFF  (e.g. emo/2024-08_000001.md)
      <seq_in_kind>/                 # versioning ON
        <seq_in_kind>_v1.md
      _index.md                      # kind _index (optional)
    <persona>_<kind>_index.md        # named_index (hand or query projection)
```

Entry `uname` format: `{kind}/{ym}_{seq:06}` (e.g. `emo/2024-08_000001`).
`uname` is the sole entry identifier across all MCP tools, CLI commands, and public APIs.
The internal UUID v7 primary key is never exposed outside the library.

## MCP Tools

- `journal_say` — append a new entry, return `{id}` (uname, e.g. `"emo/2024-08_000001"`)
- `journal_query_latest` — latest N entries of a kind
- `journal_entry_read` — read entry body (optional version)
- `journal_kind_register` — register / replace a kind config (TOML body)
- `journal_kind_list` — list registered kinds
- `journal_projection_rebuild` — rebuild FS projection (entry .md + `_index.md`) from DB SoT
- `journal_reload_kinds` — re-scan `<root>/<persona>/.journal.toml` and insert new kinds
- `journal_query_by_retrieval` — query entries ranked by decay-weighted retrieval strength (top-N)
- `journal_filter` — filter entries by retrieval score (Visible / Archive / Partial / Full modes)
- `journal_pin` — pin an entry by setting its `retrieval_strength`
- `journal_unpin` — unpin an entry by resetting `retrieval_strength` to 1.0 (neutral)
- `journal_boost_kind` — set a kind-wide retrieval boost factor

The `emo` and `archive` presets are available via `KindConfig::preset_emo()` and
`KindConfig::preset_archive()` respectively. `emo` is auto-registered on first use.

## `.journal.toml` — Persona Kind Presets

Place a `.journal.toml` file in `<root>/<persona>/` to define kind presets. The loader
runs on first access per persona (insert-if-absent semantics: kinds already registered
via `journal_kind_register` are never overwritten).

```toml
# <root>/<persona>/.journal.toml
# `[[kinds]]` array-of-tables: list multiple kinds in one file.
# Loader uses insert-if-absent — existing kinds are not touched.

[[kinds]]
kind = "emo"
mode = "entries"
path_template = "{persona}/{kind}/{seq_in_kind}.md"
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
```

Fields match the single-kind TOML accepted by `journal_kind_register` (§9.5).
To reload after editing the file, call `journal_reload_kinds`.

## Install

```sh
cargo install --path crates/persona-journal-mcp
```

Add to your MCP client config (Claude Code project `.mcp.json` example):

```json
{
  "mcpServers": {
    "persona-journal": {
      "command": "persona-journal-mcp",
      "env": { "PERSONA_JOURNAL_ROOT": "/path/to/journal/root" }
    }
  }
}
```

## Maintenance CLI

The same binary doubles as a CLI:

```sh
persona-journal-mcp kind-list <persona>
persona-journal-mcp projection-rebuild <persona>
persona-journal-mcp import-fs <persona> <kind> <source> [--force-override] [--dry-run]
```

`import-fs` behavior depends on the kind's mode:

- **`entries` mode** — `<source>` is a directory. Walks it for `YYYY-MM_NNNNN.md` files and imports them via `Journal::import_entry`. `--force-override` appends a new version instead of returning a conflict error.
- **`named_index` mode** — `<source>` is a file. Reads it line by line; blank lines and lines starting with `#` are skipped. Each remaining line is inserted as an independent row via `Journal::append_named_index`. `--force-override` is accepted but has no effect (auto-generated sequence never conflicts).

`--dry-run` performs all parsing and existence checks without writing to the DB in both modes.

(default subcommand is `mcp`, which serves over stdio.)

## Workspace Crates

| Crate | Role |
|---|---|
| `persona-journal-core` | Pure-parse library — `schema` / `loader` / `storage` helpers / `CoreError`. No `rusqlite` dependency. |
| `persona-journal` | Main library — `Journal` API, SQLite I/O, MCP handler logic. Re-exports core types. |
| `persona-journal-mcp` | Binary — MCP stdio server + maintenance CLI. |


## License

Dual-licensed under MIT or Apache-2.0 at your option.
