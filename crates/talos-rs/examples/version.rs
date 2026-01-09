//! Example: Get version info from a Talos cluster
//!
//! Run with: cargo run --example version -p talos-rs

use talos_rs::TalosClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install the ring crypto provider for rustls
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Connect using the default talosconfig (~/.talos/config)
    println!("Connecting to Talos cluster...");
    let client = TalosClient::from_default_config().await?;

    // Get version info from all nodes
    println!("\nVersion Info:");
    println!("{:-<60}", "");
    let versions = client.version().await?;
    for v in &versions {
        println!("Node:     {}", v.node);
        println!("Version:  {}", v.version);
        println!("SHA:      {}", v.sha);
        println!("OS/Arch:  {}/{}", v.os, v.arch);
        println!("{:-<60}", "");
    }

    // Get services
    println!("\nServices:");
    println!("{:-<60}", "");
    let services = client.services().await?;
    for node_svc in &services {
        println!("Node: {}", node_svc.node);
        for svc in &node_svc.services {
            let health = svc
                .health
                .as_ref()
                .map(|h| if h.healthy { "●" } else { "○" })
                .unwrap_or("?");
            println!("  {} {} ({})", health, svc.id, svc.state);
        }
        println!();
    }

    // Get memory info
    println!("Memory:");
    println!("{:-<60}", "");
    let memories = client.memory().await?;
    for mem in &memories {
        if let Some(info) = &mem.meminfo {
            println!(
                "{}: {:.1}% used ({} MB / {} MB)",
                mem.node,
                info.usage_percent(),
                info.mem_available / 1024 / 1024,
                info.mem_total / 1024 / 1024
            );
        }
    }

    // Get load average
    println!("\nLoad Average:");
    println!("{:-<60}", "");
    let loads = client.load_avg().await?;
    for load in &loads {
        println!(
            "{}: {:.2} {:.2} {:.2} (1/5/15 min)",
            load.node, load.load1, load.load5, load.load15
        );
    }

    // Get CPU info
    println!("\nCPU Info:");
    println!("{:-<60}", "");
    let cpus = client.cpu_info().await?;
    for cpu in &cpus {
        println!(
            "{}: {} cores @ {:.0} MHz - {}",
            cpu.node, cpu.cpu_count, cpu.mhz, cpu.model_name
        );
    }

    // Get logs for apid service (last 5 lines)
    println!("\nLogs (apid, last 5 lines):");
    println!("{:-<60}", "");
    let logs = client.logs("apid", 5).await?;
    for line in logs.lines().take(10) {
        println!("{}", line);
    }

    Ok(())
}
