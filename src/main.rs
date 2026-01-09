//! talos-pilot: A terminal UI for managing Talos Linux clusters

use clap::Parser;
use color_eyre::Result;
use talos_pilot_tui::App;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Initialize error handling
    color_eyre::install()?;

    // Initialize logging
    let log_level = if cli.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(log_level.parse().unwrap()),
        )
        .with_target(false)
        .init();

    tracing::info!("Starting talos-pilot");

    if let Some(ctx) = &cli.context {
        tracing::info!("Using context: {}", ctx);
    }

    // Run the TUI
    let mut app = App::new();
    app.run().await?;

    tracing::info!("Goodbye!");
    Ok(())
}
