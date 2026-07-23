//! Smoke-test harness for multi-endpoint failover.
//!
//! Loads a talosconfig context and times a real `version()` RPC through
//! `TalosClient` (which goes via `create_channel`). Prints the endpoints, the
//! wall-clock time, and whether the call succeeded. A 30s bound keeps a dead
//! endpoint from stalling the run so old-vs-new behavior is easy to compare.
//!
//! Usage: cargo run -p talos-rs --example failover_check -- <talosconfig> <context>

use std::time::{Duration, Instant};
use talos_rs::{TalosClient, TalosConfig};

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls crypto provider");

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: failover_check <talosconfig_path> <context_name>");
        std::process::exit(2);
    }
    let cfg_path = std::path::PathBuf::from(&args[1]);
    let ctx_name = &args[2];

    let config = TalosConfig::load_from(&cfg_path).expect("load talosconfig");
    let ctx = config.get_context(ctx_name).expect("context not found");

    println!("context   : {}", ctx_name);
    println!("endpoints : {:?}", ctx.endpoints);
    println!("nodes     : {:?}", ctx.nodes);

    // Time the connect (now eager: races all endpoints, first reachable wins).
    let start = Instant::now();
    let built = tokio::time::timeout(Duration::from_secs(35), TalosClient::from_context(ctx)).await;
    let client = match built {
        Err(_) => {
            println!(
                "RESULT    : CONNECT TIMED OUT after {:.2?} (>35s)",
                start.elapsed()
            );
            std::process::exit(1);
        }
        Ok(Err(e)) => {
            println!(
                "RESULT    : NO ENDPOINT REACHABLE after {:.2?} — {}",
                start.elapsed(),
                e
            );
            std::process::exit(1);
        }
        Ok(Ok(c)) => {
            println!("connected : in {:.2?}", start.elapsed());
            c
        }
    };

    let outcome = tokio::time::timeout(Duration::from_secs(30), client.version()).await;
    let elapsed = start.elapsed();

    match outcome {
        Ok(Ok(versions)) => {
            let first = versions
                .first()
                .map(|v| format!("{} ({})", v.node, v.version))
                .unwrap_or_else(|| "<none>".to_string());
            println!(
                "RESULT    : OK in {:.2?} — {} node(s), first = {}",
                elapsed,
                versions.len(),
                first
            );
        }
        Ok(Err(e)) => {
            println!("RESULT    : RPC ERROR after {:.2?} — {}", elapsed, e);
            std::process::exit(1);
        }
        Err(_) => {
            println!(
                "RESULT    : TIMED OUT after {:.2?} (>30s, no failover)",
                elapsed
            );
            std::process::exit(1);
        }
    }
}
