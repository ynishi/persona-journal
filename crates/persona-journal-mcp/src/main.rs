use std::path::PathBuf;

use anyhow::{Context, Result};
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
    /// Import legacy FS entries (`YYYY-MM_NNNNN.md`) into the DB.
    ///
    /// Phase 0.5 migration helper. Entries-mode kinds only (emo / states / memories).
    /// Non-rem (named_index mode) is out of scope and tracked in a separate issue.
    ImportFs {
        /// Persona name.
        #[arg(long)]
        persona: String,
        /// Kind name. Must be registered and use entries mode.
        #[arg(long)]
        kind: String,
        /// Source directory containing `YYYY-MM_NNNNN.md` files.
        #[arg(long)]
        source: PathBuf,
        /// Overwrite existing entries by appending version+1.
        #[arg(long, default_value_t = false)]
        force_override: bool,
        /// Parse and report only; do not touch the DB.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
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

/// Parse a filename like `2024-08_00012.md` into `(year, month, seq)`.
///
/// Returns `None` if the filename does not match the expected shape.
fn parse_entry_filename(name: &str) -> Option<(i32, u8, u32)> {
    let stem = name.strip_suffix(".md")?;
    // Expected: "YYYY-MM_NNNNN" — len 13 with separators at positions 4, 7.
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

struct ImportSummary {
    matched: usize,
    written: usize,
    skipped: usize,
    errors: usize,
    conflicts: Vec<String>,
}

fn run_import_fs(
    journal: &Journal,
    persona: &str,
    kind: &str,
    source: &PathBuf,
    force_override: bool,
    dry_run: bool,
) -> Result<ImportSummary> {
    let mut summary = ImportSummary {
        matched: 0,
        written: 0,
        skipped: 0,
        errors: 0,
        conflicts: Vec::new(),
    };

    let mut entries: Vec<(PathBuf, i32, u8, u32)> = Vec::new();
    let read_dir = std::fs::read_dir(source)
        .with_context(|| format!("failed to read source dir: {}", source.display()))?;
    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("warn: read_dir error: {e}");
                summary.errors += 1;
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".md") {
            continue;
        }
        let Some((year, month, seq)) = parse_entry_filename(name) else {
            eprintln!("warn: skip non-matching filename: {name}");
            summary.skipped += 1;
            continue;
        };
        entries.push((path, year, month, seq));
    }

    // Stable order: by (year, month, seq).
    entries.sort_by_key(|t| (t.1, t.2, t.3));
    summary.matched = entries.len();

    for (path, year, month, seq) in entries {
        let body = match std::fs::read_to_string(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("error: read {}: {e}", path.display());
                summary.errors += 1;
                continue;
            }
        };
        let created_at = format!("{year:04}-{month:02}-01T00:00:00Z");
        let id_preview = format!("{year:04}-{month:02}_{seq:05}");

        if dry_run {
            println!(
                "[would import] persona={persona} kind={kind} id={id_preview} path={}",
                path.display()
            );
            summary.written += 1;
            continue;
        }

        match journal.import_entry(
            persona,
            kind,
            year,
            month,
            seq,
            &created_at,
            &body,
            vec![],
            force_override,
        ) {
            Ok(id) => {
                summary.written += 1;
                eprintln!("imported: {id}");
            }
            Err(JournalError::AlreadyExists(id)) => {
                summary.conflicts.push(id);
            }
            Err(e) => {
                eprintln!("error: import {id_preview}: {e}");
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

            if !summary.conflicts.is_empty() {
                println!(
                    "conflicts (DB already has these ids, no write performed for these):"
                );
                for id in &summary.conflicts {
                    println!("  {id}");
                }
                println!("total conflicts: {}", summary.conflicts.len());
                println!("rerun with --force-override to overwrite (version+1).");
            }

            let mode = if dry_run { "dry-run" } else { "imported" };
            println!(
                "{mode}: persona={persona} kind={kind} matched={} written={} skipped={} conflicts={} errors={}",
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

    #[test]
    fn parse_entry_filename_accepts_canonical_shape() {
        assert_eq!(parse_entry_filename("2024-08_00012.md"), Some((2024, 8, 12)));
        assert_eq!(parse_entry_filename("2024-01_00001.md"), Some((2024, 1, 1)));
        assert_eq!(
            parse_entry_filename("2099-12_99999.md"),
            Some((2099, 12, 99999))
        );
    }

    #[test]
    fn parse_entry_filename_rejects_bad_shapes() {
        // Missing .md
        assert_eq!(parse_entry_filename("2024-08_00012"), None);
        // Wrong separator
        assert_eq!(parse_entry_filename("2024_08-00012.md"), None);
        // Bad month
        assert_eq!(parse_entry_filename("2024-13_00012.md"), None);
        assert_eq!(parse_entry_filename("2024-00_00012.md"), None);
        // Wrong seq width
        assert_eq!(parse_entry_filename("2024-08_12.md"), None);
        // Index file
        assert_eq!(parse_entry_filename("_index.md"), None);
        // Non-numeric
        assert_eq!(parse_entry_filename("abcd-08_00012.md"), None);
    }

    #[test]
    fn run_import_fs_dry_run_does_not_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("2024-08_00001.md"), "# entry1\nbody").unwrap();
        std::fs::write(src.join("2024-08_00002.md"), "# entry2\nbody").unwrap();
        // noise files (should be skipped)
        std::fs::write(src.join("_index.md"), "ignore").unwrap();
        std::fs::write(src.join("README.md"), "ignore").unwrap();

        let j = Journal::open(root.clone());
        j.ensure_default_kinds("shi").unwrap();

        let summary =
            run_import_fs(&j, "shi", "emo", &src, false, true).unwrap();
        assert_eq!(summary.matched, 2);
        assert_eq!(summary.written, 2);
        assert_eq!(summary.skipped, 2); // _index.md + README.md
        assert!(summary.conflicts.is_empty());

        let rows = j.query_latest("shi", "emo", 10).unwrap();
        assert!(rows.is_empty(), "dry-run must not write to DB");
    }

    #[test]
    fn run_import_fs_real_write_then_conflict() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("2024-08_00001.md"), "# entry1\nfirst body").unwrap();

        let j = Journal::open(root.clone());
        j.ensure_default_kinds("shi").unwrap();

        // First run: 1 written.
        let s1 = run_import_fs(&j, "shi", "emo", &src, false, false).unwrap();
        assert_eq!(s1.written, 1);
        assert!(s1.conflicts.is_empty());

        // Second run with same input: 1 conflict, 0 written.
        let s2 = run_import_fs(&j, "shi", "emo", &src, false, false).unwrap();
        assert_eq!(s2.written, 0);
        assert_eq!(s2.conflicts, vec!["2024-08_00001".to_string()]);

        // Body unchanged.
        let body = j.entry_read("shi", "2024-08_00001", None).unwrap();
        assert!(body.contains("first body"));

        // Third run with --force-override: 1 written (v2), 0 conflicts.
        std::fs::write(src.join("2024-08_00001.md"), "# entry1\nsecond body").unwrap();
        let s3 = run_import_fs(&j, "shi", "emo", &src, true, false).unwrap();
        assert_eq!(s3.written, 1);
        assert!(s3.conflicts.is_empty());
        let body = j.entry_read("shi", "2024-08_00001", None).unwrap();
        assert!(body.contains("second body"));
    }
}
