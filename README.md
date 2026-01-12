# talos-pilot

A terminal UI (TUI) for managing and monitoring [Talos Linux](https://www.talos.dev/) Kubernetes clusters.

**talos-pilot** provides real-time cluster visibility, diagnostics, log streaming, network analysis, and production-ready node operations - all from your terminal.

![Rust](https://img.shields.io/badge/rust-2024%20edition-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

https://github.com/user-attachments/assets/4c946c32-1f7e-4ab8-9d88-9937516015d1

## Why talos-pilot?

Talos Linux removes SSH access for security, replacing it with an API-driven management model. While `talosctl` is powerful, it requires memorizing many subcommands. **talos-pilot** provides:

- **Interactive cluster overview** - See all nodes, services, and health at a glance
- **Real-time monitoring** - CPU, memory, network stats with auto-refresh
- **Unified log viewer** - Stream logs from multiple services simultaneously (Stern-style)
- **Production operations** - Drain, reboot, rolling upgrades with safety checks
- **Diagnostics** - Automated health checks with actionable fix suggestions

### Relationship to k9s

**talos-pilot is complementary to k9s, not a replacement.** They operate at different layers:

| Tool | Layer | API Port | Use Case |
|------|-------|----------|----------|
| **k9s** | Kubernetes | `:6443` | Pods, deployments, services, workload debugging |
| **talos-pilot** | Operating System | `:50000` | Talos services, etcd, kubelet, node health, OS config |

Use **k9s** for "why won't my pod start?"
Use **talos-pilot** for "why won't my node join the cluster?"

## Features

### Cluster Management

| Feature | Description |
|---------|-------------|
| **Cluster Overview** | Multi-cluster monitoring, node list with health indicators |
| **Node Details** | CPU, memory, load averages, Talos/K8s versions |
| **Service Status** | All Talos services with health indicators |

### Monitoring

| Feature | Description |
|---------|-------------|
| **Service Logs** | Scrollable, searchable (`/`), color-coded by level |
| **Multi-Service Logs** | Stern-style interleaved logs from multiple services |
| **Processes View** | htop-like process list with tree view, CPU/MEM sorting |
| **Network Stats** | Interface traffic, connections, KubeSpan peers, packet capture |
| **etcd Status** | Quorum health, member list, alarms, leader tracking |
| **Workload Health** | K8s deployments, statefulsets, pod issues by namespace |
| **Lifecycle View** | Version status, config drift detection, cluster alerts |

### Diagnostics & Security

| Feature | Description |
|---------|-------------|
| **System Diagnostics** | Automated health checks with actionable fixes |
| **CNI Detection** | Flannel, Cilium, Calico with provider-specific checks |
| **Addon Detection** | cert-manager, ArgoCD, Flux, and more |
| **Security Audit** | PKI certificate expiry, encryption status |

### Operations

| Feature | Description |
|---------|-------------|
| **Node Drain** | PDB-aware with configurable timeouts |
| **Node Reboot** | Post-reboot verification, auto-uncordon |
| **Rolling Operations** | Sequential multi-node with progress tracking |
| **Audit Logging** | All operations logged to `~/.talos-pilot/audit.log` |

## Installation

### From Releases (Recommended)

Download the latest release for your platform from the [Releases](https://github.com/yourorg/talos-pilot/releases) page.

```bash
# Linux x64
curl -LO https://github.com/yourorg/talos-pilot/releases/latest/download/talos-pilot-<version>-x86_64-unknown-linux-gnu.tar.gz
tar xzf talos-pilot-*.tar.gz
sudo mv talos-pilot /usr/local/bin/

# macOS (Apple Silicon)
curl -LO https://github.com/yourorg/talos-pilot/releases/latest/download/talos-pilot-<version>-aarch64-apple-darwin.tar.gz
tar xzf talos-pilot-*.tar.gz
sudo mv talos-pilot /usr/local/bin/

# macOS (Intel)
curl -LO https://github.com/yourorg/talos-pilot/releases/latest/download/talos-pilot-<version>-x86_64-apple-darwin.tar.gz
tar xzf talos-pilot-*.tar.gz
sudo mv talos-pilot /usr/local/bin/
```

### From Source

```bash
git clone https://github.com/yourorg/talos-pilot
cd talos-pilot
cargo build --release
./target/release/talos-pilot
```

### Requirements

- Valid `~/.talos/config` (talosconfig)
- Network access to Talos nodes on port 50000
- (Building from source) Rust 2024 edition (1.85+)

## Usage

```bash
# Use default context from talosconfig
talos-pilot

# Use specific context
talos-pilot --context homelab

# Set log tail limit
talos-pilot --tail 1000

# Enable debug logging
talos-pilot --debug --log-file ~/talos-pilot.log
```

### Keyboard Navigation

| Key | Action |
|-----|--------|
| `?` | Help |
| `q` / `Ctrl+C` | Quit |
| `Esc` | Back / Close |
| `j/k` or `↑/↓` | Navigate |
| `Enter` | Select / Expand |
| `Tab` | Next panel |
| `r` | Refresh |
| `a` | Toggle auto-refresh |
| `/` | Search (in logs) |
| `n/N` | Next/prev search match |

### View Shortcuts

| Key | View | Description |
|-----|------|-------------|
| `c` | Cluster | Node overview |
| `s` | Services | Service list for selected node |
| `l` | Logs | Single service logs |
| `L` | Multi-Logs | Interleaved multi-service logs |
| `p` | Processes | Process tree view |
| `n` | Network | Interface stats, connections |
| `e` | etcd | Cluster health, members |
| `w` | Workloads | K8s deployment health |
| `y` | Lifecycle | Version status, alerts |
| `d` | Diagnostics | System health checks |
| `S` | Security | PKI and encryption audit |
| `o` | Operations | Single node operations |
| `O` | Rolling | Multi-node rolling operations |

## Architecture

```
crates/
├── talos-rs/           # Talos gRPC client library
├── talos-pilot-core/   # Shared business logic
└── talos-pilot-tui/    # Terminal UI (ratatui)
```

### Core Modules

| Module | Purpose |
|--------|---------|
| `indicators` | HealthIndicator, QuorumState, SafetyStatus |
| `formatting` | format_bytes, format_duration, pluralize |
| `selection` | SelectableList<T>, MultiSelectList<T> |
| `async_state` | Loading/error/refresh state management |
| `diagnostics` | CheckStatus, CniType, PodHealthInfo |
| `constants` | Thresholds, CRD lists, refresh intervals |
| `network` | Port-to-service mapping, classification |
| `errors` | User-friendly error formatting |

### Key Technologies

- **Rust 2024 edition** with async/await
- **tokio** - Async runtime
- **ratatui + crossterm** - TUI framework
- **tonic + prost** - gRPC client
- **kube-rs** - Kubernetes client
- **color-eyre** - Error handling

## Development

```bash
# Run all tests
cargo test --all

# Run with debug output
RUST_LOG=debug cargo run

# Watch logs in another terminal
tail -f /tmp/talos-pilot.log

# Check for warnings
cargo clippy --all
```

### Local Testing with Docker

See [docs/local-talos-setup.md](docs/local-talos-setup.md) for setting up a local Talos cluster.

### Current Stats

- **Core library**: ~1,760 lines across 8 modules
- **Tests**: 70 total (47 core + 6 TUI + 6 talos-rs + 11 doc)
- **Components**: 12 TUI components
- **Build warnings**: 0

## Contributing

### Key Principles

1. **State over logs** - Check actual system state, not log messages
2. **Graceful degradation** - Show "unknown" rather than crash
3. **No false positives** - When in doubt, show unknown not failed

## Roadmap

| Feature | Priority |
|---------|----------|
| Container namespace support | Medium |
| Upgrade availability alerts | Low |

## License

MIT License - see [LICENSE](./LICENSE) for details.

## Acknowledgments

- [Talos Linux](https://www.talos.dev/) by Sidero Labs
- [k9s](https://k9scli.io/) for TUI inspiration
- [ratatui](https://ratatui.rs/) for the TUI framework
