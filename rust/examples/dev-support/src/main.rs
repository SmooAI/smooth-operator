//! `dev-support` — stand up a dev-team knowledge & support agent over your
//! GitHub repo in minutes.
//!
//! ```text
//! export GITHUB_TOKEN=…  SMOOAI_GATEWAY_KEY=…
//! # edit dev-support.toml (owner/repo)
//! dev-support ingest        # pull prose + code + issues into a knowledge store
//! dev-support chat          # grounded Q&A REPL over the repo
//! dev-support serve         # run the WS server over the repo for the chat-widget
//! ```
//!
//! See `README.md` for the full quickstart, a sample transcript, and how to
//! point the chat-widget at `dev-support serve`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use dev_support::config::{DevSupportConfig, DEFAULT_GATEWAY_URL};
use dev_support::ingest::{build_connector, ingest_into_memory};
use dev_support::runtime::{gateway_llm_config, tool_github_auth, DevSupportRuntime};
use dev_support::serve::run_serve;
use smooth_operator::StorageAdapter;

/// `max_tokens` per turn — kept modest because the gateway is paid-per-token.
const CHAT_MAX_TOKENS: u32 = 768;

#[derive(Parser)]
#[command(
    name = "dev-support",
    about = "Stand up a dev-team knowledge & support agent over your GitHub repo in minutes.",
    long_about = "Ingest a GitHub repo's prose, code, and issues into a smooth-operator knowledge \
                  store, then chat — grounded in the repo, with a live github_search for anything \
                  newer than the last ingest.\n\nSecrets come from the environment: $GITHUB_TOKEN \
                  (when github.auth = \"token\") and $SMOOAI_GATEWAY_KEY (for the chat gateway)."
)]
struct Cli {
    /// Path to the dev-support.toml config file.
    #[arg(long, short, default_value = "dev-support.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Ingest the configured repo (prose + code + issues) and print a summary.
    Ingest,
    /// Ingest the repo, then start an interactive grounded Q&A REPL.
    Chat,
    /// Ingest the configured repo, then run the smooth-operator WebSocket server
    /// over that knowledge so the chat-widget can connect (full-page UI).
    Serve,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Readable logs; quiet by default, raise with RUST_LOG=info.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();
    let config = DevSupportConfig::from_path(&cli.config).with_context(|| {
        format!(
            "loading config from {} — pass --config <path> or create a dev-support.toml \
             (see dev-support.example.toml)",
            cli.config.display()
        )
    })?;

    match cli.command {
        Command::Ingest => run_ingest(&config).await,
        Command::Chat => run_chat(&config).await,
        Command::Serve => run_serve(&config).await,
    }
}

/// `ingest`: build the connector, pull the repo, print a summary.
async fn run_ingest(config: &DevSupportConfig) -> Result<()> {
    let slug = config.repo_slug();
    eprintln!("==> Ingesting {slug} (prose+code+issues) …");
    let connector = build_connector(config)?;
    let (storage, report) = ingest_into_memory(&connector, &config.org_id()).await?;

    println!("Ingest complete for {slug}:");
    println!("  documents pulled:   {}", report.documents_pulled);
    println!("  documents skipped:  {}", report.documents_skipped);
    println!("  chunks indexed:     {}", report.chunks_stored);
    println!("  embedding dim:      {}", report.embedding_dim);
    // Touch the store so the summary reflects a live, queryable index.
    let probe = storage
        .knowledge()
        .query(&slug, 1)
        .map(|h| h.len())
        .unwrap_or(0);
    println!("  knowledge store:    in-memory (queryable; {probe}+ hits for '{slug}')");
    println!();
    println!("Note: this demo uses an in-memory store (gone on exit). For persistence across");
    println!("restarts, point the pipeline at the Postgres (pgvector) adapter — same connector");
    println!("and pipeline code, only the StorageAdapter changes.");
    Ok(())
}

/// `chat`: ingest, then run an interactive grounded REPL against the gateway.
async fn run_chat(config: &DevSupportConfig) -> Result<()> {
    let gateway_key = std::env::var("SMOOAI_GATEWAY_KEY").unwrap_or_default();
    if gateway_key.trim().is_empty() {
        anyhow::bail!(
            "SMOOAI_GATEWAY_KEY is unset/empty — export your llm.smoo.ai gateway key to chat. \
             (Ingest works without it; chat needs the LLM.)"
        );
    }
    let gateway_url =
        std::env::var("SMOOAI_GATEWAY_URL").unwrap_or_else(|_| DEFAULT_GATEWAY_URL.to_string());

    let slug = config.repo_slug();
    eprintln!("==> Ingesting {slug} …");
    let connector = build_connector(config)?;
    let (storage, report) = ingest_into_memory(&connector, &config.org_id()).await?;
    eprintln!(
        "==> Indexed {} chunks from {} documents. Ask away (Ctrl-D or 'exit' to quit).\n",
        report.chunks_stored, report.documents_pulled
    );

    let llm = gateway_llm_config(
        &config.agent.model,
        gateway_key,
        gateway_url,
        CHAT_MAX_TOKENS,
    );
    let github_auth = tool_github_auth(config)?;
    let runtime = DevSupportRuntime::new(
        config,
        llm,
        github_auth,
        Arc::clone(&storage) as Arc<dyn StorageAdapter>,
    );

    let stdin = io::stdin();
    loop {
        print!("you ▸ ");
        io::stdout().flush().ok();
        let mut line = String::new();
        let n = stdin.read_line(&mut line).context("reading stdin")?;
        if n == 0 {
            println!();
            break; // EOF (Ctrl-D)
        }
        let question = line.trim();
        if question.is_empty() {
            continue;
        }
        if matches!(question, "exit" | "quit" | ":q") {
            break;
        }

        match runtime.run_turn(question).await {
            Ok(outcome) => {
                println!("\nagent ▸ {}\n", outcome.reply.trim());
                let tools = outcome.tools_used();
                if !tools.is_empty() {
                    println!("        (tools used: {})\n", tools.join(", "));
                }
            }
            Err(e) => eprintln!("\n[error] {e:#}\n"),
        }
    }
    eprintln!("bye 👋");
    Ok(())
}
