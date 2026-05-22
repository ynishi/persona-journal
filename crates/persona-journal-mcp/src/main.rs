use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use persona_journal::{Error as JournalError, Journal};
use persona_journal_mcp::JournalService;
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(version, about = "persona-journal MCP server and maintenance CLI")]
struct Cli {
    /// Override the journal root. Defaults to $PERSONA_JOURNAL_ROOT or `~/.persona-journal`.
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run as an MCP server over stdio (default if no subcommand is given).
    Mcp,
    /// Rebuild FS projection (entry .md + _index.md) from DB SoT for a persona.
    ProjectionRebuild { persona: String },
    /// List registered kinds for a persona.
    KindList { persona: String },
    /// Import entries from a legacy FS directory (5-digit YYYY-MM_NNNNN.md filenames).
    ImportFs {
        #[arg(long)]
        persona: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        force_override: bool,
        #[arg(long)]
        dry_run: bool,
    },
}

/// Counters and conflict list collected during a single `run_import_fs` invocation.
struct ImportSummary {
    matched: usize,
    written: usize,
    skipped: usize,
    errors: usize,
    conflicts: Vec<String>,
}

fn resolve_root(cli_root: Option<PathBuf>) -> PathBuf {
    if let Some(p) = cli_root {
        return p;
    }
    if let Ok(p) = std::env::var("PERSONA_JOURNAL_ROOT") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".persona-journal")
}

fn init_tracing() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
}

/// Parse a legacy entry filename of the form `YYYY-MM_NNNNN.md` (5-digit seq).
///
/// # Returns
/// `Some((year, month, seq))` on success, `None` for non-matching or invalid names.
///
/// Month must be in 1–12; the 4-digit year and 5-digit seq are enforced by
/// character-count checks before parsing.
fn parse_entry_filename(name: &str) -> Option<(i32, u8, u32)> {
    let stem = name.strip_suffix(".md")?;
    let (ym, seq) = stem.split_once('_')?;
    let (y, m) = ym.split_once('-')?;
    if y.len() != 4 || m.len() != 2 || seq.len() != 5 {
        return None;
    }
    let year: i32 = y.parse().ok()?;
    let month: u8 = m.parse().ok()?;
    let seq: u32 = seq.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    Some((year, month, seq))
}

/// Import entries from a legacy FS directory into the journal.
///
/// Walks `source` for files matching `YYYY-MM_NNNNN.md`, reads their body,
/// and calls `Journal::import_entry` for each one. Returns an `ImportSummary`
/// with counts of matched / written / skipped / conflicted / errored files.
///
/// # Arguments
/// - `journal` — open `Journal` instance
/// - `persona` — target persona
/// - `kind` — entry kind (must be `entries` mode)
/// - `source` — directory containing legacy `.md` files
/// - `force_override` — if true, re-import conflicting entries as new versions
/// - `dry_run` — if true, print what would be imported without writing to DB
///
/// # Errors
/// Returns `Err` only for unrecoverable setup failures (e.g. `read_dir` on the
/// source path). Per-file failures are accumulated in the returned summary.
fn run_import_fs(
    journal: &Journal,
    persona: &str,
    kind: &str,
    source: &Path,
    force_override: bool,
    dry_run: bool,
) -> anyhow::Result<ImportSummary> {
    let mut summary = ImportSummary {
        matched: 0,
        written: 0,
        skipped: 0,
        errors: 0,
        conflicts: Vec::new(),
    };

    let entries = std::fs::read_dir(source)
        .map_err(|e| anyhow::anyhow!("failed to read source dir {}: {e}", source.display()))?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error=?e, "failed to read dir entry, skipping");
                summary.errors += 1;
                continue;
            }
        };

        let file_name_os = entry.file_name();
        let file_name = match file_name_os.to_str() {
            Some(s) => s.to_owned(),
            None => {
                summary.skipped += 1;
                continue;
            }
        };

        let parsed = match parse_entry_filename(&file_name) {
            Some(p) => p,
            None => {
                summary.skipped += 1;
                continue;
            }
        };

        let (year, month, seq) = parsed;
        summary.matched += 1;

        let ym = format!("{year:04}-{month:02}");
        let path = entry.path();

        if dry_run {
            // CRUX-2: use format!("{seq:06}") here for display only — no DB write.
            // Journal::import_entry is NOT called, so seq_in_kind_str is not bypassed.
            println!(
                "[would import] persona={persona} kind={kind} uname={kind}/{ym}_{seq:06} path={}",
                path.display()
            );
            // dry_run does not increment written
            continue;
        }

        let body = match std::fs::read_to_string(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error=?e, path=%path.display(), "failed to read file, skipping");
                summary.errors += 1;
                continue;
            }
        };

        let created_at = format!("{ym}-01T00:00:00Z");

        // CRUX-2: seq (u32) is passed as-is; Journal::import_entry calls
        // seq_in_kind_str(year_month, seq) internally to produce the 6-digit form.
        // CRUX-3: UUID lookup before add_version is handled inside import_entry.
        match journal.import_entry(
            persona,
            kind,
            &ym,
            seq,
            &created_at,
            &body,
            vec![],
            force_override,
        ) {
            Ok(uname) => {
                // CRUX-1: only uname (String) is exposed; UUID never surfaces here.
                println!("imported: {uname}");
                summary.written += 1;
            }
            Err(JournalError::AlreadyExists(uname)) => {
                // CRUX-1: conflict payload is uname, not UUID.
                tracing::warn!(uname=%uname, "entry already exists, skipping (use --force-override to re-import)");
                summary.conflicts.push(uname);
            }
            Err(e) => {
                tracing::warn!(error=?e, path=%path.display(), "import failed");
                summary.errors += 1;
            }
        }
    }

    Ok(summary)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = resolve_root(cli.root);
    init_tracing();

    match cli.command.unwrap_or(Cmd::Mcp) {
        Cmd::Mcp => {
            tracing::info!(?root, "persona-journal-mcp starting");
            let svc = JournalService::new(root).serve(stdio()).await?;
            svc.waiting().await?;
            Ok(())
        }
        Cmd::ProjectionRebuild { persona } => {
            let j = Journal::open(root);
            j.projection_rebuild(&persona)?;
            println!("ok");
            Ok(())
        }
        Cmd::KindList { persona } => {
            let j = Journal::open(root);
            let kinds = j.kind_list(&persona)?;
            for k in kinds {
                println!("{}\t{}", k.kind, k.mode.as_str());
            }
            Ok(())
        }
        Cmd::ImportFs {
            persona,
            kind,
            source,
            force_override,
            dry_run,
        } => {
            let j = Journal::open(root);
            let summary = run_import_fs(&j, &persona, &kind, &source, force_override, dry_run)?;
            println!(
                "imported: persona={persona} kind={kind} matched={} written={} skipped={} conflicts={} errors={}",
                summary.matched,
                summary.written,
                summary.skipped,
                summary.conflicts.len(),
                summary.errors
            );
            if !summary.conflicts.is_empty() && !force_override && !dry_run {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── helpers ─────────────────────────────────────────────────────────────

    /// Set up a Journal with the default kinds ("emo" preset) registered.
    fn setup_journal(root: &Path, persona: &str) -> Journal {
        let j = Journal::open(root.to_path_buf());
        j.ensure_default_kinds(persona).unwrap();
        j
    }

    /// Write a file into `dir` with the given `filename` and `body`.
    fn write_src_file(dir: &Path, filename: &str, body: &str) {
        // safety: test-only, panic on write failure is acceptable
        std::fs::write(dir.join(filename), body).unwrap();
    }

    // ── T1: parse accepts canonical shape ───────────────────────────────────

    #[test]
    fn parse_entry_filename_accepts_canonical_shape() {
        let result = parse_entry_filename("2026-05_00001.md");
        assert_eq!(result, Some((2026, 5, 1)));
    }

    #[test]
    fn parse_entry_filename_accepts_max_seq() {
        let result = parse_entry_filename("2023-12_99999.md");
        assert_eq!(result, Some((2023, 12, 99999)));
    }

    // ── T2: parse rejects bad shapes ────────────────────────────────────────

    #[test]
    fn parse_entry_filename_rejects_bad_shapes() {
        // README.md — no underscore separator
        assert_eq!(parse_entry_filename("README.md"), None);
        // _format.md — no YYYY-MM prefix
        assert_eq!(parse_entry_filename("_format.md"), None);
        // invalid month 13
        assert_eq!(parse_entry_filename("2026-13_00001.md"), None);
        // month 00
        assert_eq!(parse_entry_filename("2026-00_00001.md"), None);
        // non-numeric seq
        assert_eq!(parse_entry_filename("2026-05_xxx.md"), None);
        // 6-digit seq (legacy is 5-digit only)
        assert_eq!(parse_entry_filename("2026-05_000001.md"), None);
        // missing .md suffix
        assert_eq!(parse_entry_filename("2026-05_00001.txt"), None);
        // .DS_Store
        assert_eq!(parse_entry_filename(".DS_Store"), None);
    }

    // ── T3: dry-run does not write ───────────────────────────────────────────

    #[test]
    fn run_import_fs_dry_run_does_not_write() {
        let root_dir = TempDir::new().unwrap();
        let src_dir = TempDir::new().unwrap();

        let j = setup_journal(root_dir.path(), "shi");
        write_src_file(src_dir.path(), "2026-05_00001.md", "dry run body");

        let summary = run_import_fs(&j, "shi", "emo", src_dir.path(), false, true).unwrap();

        // dry_run must not increment written
        assert_eq!(summary.written, 0);
        assert_eq!(summary.matched, 1);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.errors, 0);
        assert!(summary.conflicts.is_empty());

        // DB must have no entries (verify via query_latest with n=100)
        let entries = j.query_latest("shi", "emo", 100).unwrap();
        assert!(entries.is_empty(), "dry_run must not write to DB");
    }

    // ── T1/T3: real then conflict then force (3-stage) ───────────────────────

    #[test]
    fn run_import_fs_real_then_conflict_then_force() {
        let root_dir = TempDir::new().unwrap();
        let src_dir = TempDir::new().unwrap();

        let j = setup_journal(root_dir.path(), "shi");
        write_src_file(src_dir.path(), "2026-05_00001.md", "v1 body");

        // Stage 1: normal import → written=1
        let s1 = run_import_fs(&j, "shi", "emo", src_dir.path(), false, false).unwrap();
        assert_eq!(s1.written, 1);
        assert_eq!(s1.matched, 1);
        assert!(s1.conflicts.is_empty());
        assert_eq!(s1.errors, 0);

        // Stage 2: re-import without force_override → conflict=1, written=0
        let s2 = run_import_fs(&j, "shi", "emo", src_dir.path(), false, false).unwrap();
        assert_eq!(s2.written, 0);
        assert_eq!(s2.conflicts.len(), 1);
        // CRUX-1: conflict payload is uname (not UUID)
        let conflict_uname = s2.conflicts[0].clone();
        assert!(
            conflict_uname.starts_with("emo/"),
            "conflict uname must start with kind prefix, got: {conflict_uname}"
        );

        // Stage 3: re-import with force_override=true → written=1, creates v2
        // overwrite file content to verify body change
        write_src_file(src_dir.path(), "2026-05_00001.md", "v2 body");
        let s3 = run_import_fs(&j, "shi", "emo", src_dir.path(), true, false).unwrap();
        assert_eq!(s3.written, 1);
        assert!(s3.conflicts.is_empty());

        // Verify v2 body is now current via entry_read (None = latest)
        // entry_read returns Result<String> (the body directly)
        let v2_body = j.entry_read("shi", &conflict_uname, None).unwrap();
        assert!(
            v2_body.contains("v2 body"),
            "latest body must be v2, got: {v2_body}"
        );

        // Verify v1 body is preserved as version 1
        let v1_body = j.entry_read("shi", &conflict_uname, Some(1)).unwrap();
        assert!(
            v1_body.contains("v1 body"),
            "v1 body must be preserved, got: {v1_body}"
        );
    }
}
