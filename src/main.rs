//! talos-pilot: A terminal UI for managing Talos Linux clusters

use clap::Parser;
use color_eyre::Result;
use std::fs::File;
use talos_pilot_tui::App;
use tracing::Level;
use tracing_subscriber::{prelude::*, EnvFilter};

/// talos-pilot: Terminal UI for Talos Linux clusters
#[derive(Parser, Debug)]
#[command(name = "talos-pilot")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Talos context to use (from talosconfig)
    #[arg(short, long)]
    context: Option<String>,

    /// Path to talosconfig file
    #[arg(long)]
    config: Option<String>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Log file path (default: /tmp/talos-pilot.log)
    #[arg(long, default_value = "/tmp/talos-pilot.log")]
    log_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Initialize error handling
    color_eyre::install()?;

    // Initialize logging to file (not stdout, which would corrupt TUI)
    let log_level = if cli.debug { Level::DEBUG } else { Level::INFO };
    let log_file = File::create(&cli.log_file)?;

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(log_file)
                .with_ansi(true)
                .with_target(false),
        )
        .with(
            EnvFilter::from_default_env()
                .add_directive(log_level.into()),
        )
        .init();

    tracing::info!("Starting talos-pilot");

    if let Some(ctx) = &cli.context {
        tracing::info!("Using context: {}", ctx);
    }

    // Run the TUI with the specified context
    let mut app = App::new(cli.context);
    app.run().await?;

    tracing::info!("Goodbye!");
    Ok(())
}
