use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use persona_journal::Journal;
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
    }
}
