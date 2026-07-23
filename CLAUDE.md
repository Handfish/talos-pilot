# CLAUDE.md - Talos Pilot Development Guide

## Project Overview

**Talos Pilot** is a terminal UI (TUI) for managing and monitoring Talos Linux Kubernetes clusters. It provides real-time diagnostics, log streaming, network analysis, and cluster health monitoring.

### Crate Structure

```
crates/
├── talos-rs/           # Low-level Talos gRPC client library
├── talos-pilot-core/   # Shared business logic (~1,760 lines, 47 tests)
└── talos-pilot-tui/    # Terminal UI application (ratatui-based)
```

### Key Technologies
- **Rust 2024 edition**
- **Async runtime:** tokio
- **TUI framework:** ratatui + crossterm + tachyonfx
- **gRPC client:** tonic + prost
- **Kubernetes client:** kube-rs
- **Error handling:** color-eyre + thiserror

---

## Feature Status

### Implemented Features

| Feature | Component | Status |
|---------|-----------|--------|
| Multi-cluster overview | `cluster.rs` | Complete |
| Node details (CPU, memory, load) | `cluster.rs` | Complete |
| Service logs with search | `logs.rs` | Complete |
| Multi-service logs (Stern-style) | `multi_logs.rs` | Complete |
| Process tree view | `processes.rs` | Complete |
| Network stats, KubeSpan, packet capture | `network.rs` | Complete |
| etcd status & quorum | `etcd.rs` | Complete |
| K8s workload health | `workloads.rs` | Complete |
| System diagnostics | `diagnostics/` | Complete |
| CNI detection (Flannel/Cilium/Calico) | `diagnostics/cni/` | Complete |
| Addon detection | `diagnostics/addons/` | Complete |
| Security/PKI audit | `security.rs` | Complete |
| Lifecycle (versions, config drift) | `lifecycle.rs` | Complete |
| Node operations (drain/reboot) | `node_operations.rs` | Complete |
| Rolling operations | `rolling_operations.rs` | Complete |
| Audit logging | `audit.rs` | Complete |

### Planned Features

| Feature | Priority | Notes |
|---------|----------|-------|
| Container namespace support | Medium | Show pod/container names for connections |
| Upgrade availability alerts | Low | Check for new Talos/K8s versions |

---

## Core Modules (talos-pilot-core)

| Module | Lines | Tests | Purpose |
|--------|-------|-------|---------|
| `indicators` | ~300 | 6 | HealthIndicator, HasHealth trait, QuorumState, SafetyStatus |
| `formatting` | ~280 | 10 | format_bytes, format_duration, format_percent, pluralize |
| `selection` | ~320 | 6 | SelectableList<T>, MultiSelectList<T> for UI navigation |
| `async_state` | ~200 | 7 | AsyncState<T> for loading/error/refresh management |
| `errors` | ~180 | 4 | format_talos_error, ErrorCategory, user-friendly messages |
| `network` | ~180 | 6 | port_to_service, connection classification |
| `diagnostics` | ~200 | 5 | CheckStatus, CniType, CniInfo, PodHealthInfo |
| `constants` | ~100 | 3 | Thresholds, CRD lists, refresh intervals |

---

## Talos Linux Reference

### Network Ports

| Port | Protocol | Service | Used By |
|------|----------|---------|---------|
| 50000 | TCP | apid (Talos API) | talosctl, control plane nodes |
| 50001 | TCP | trustd | Worker nodes for TLS certs |
| 6443 | TCP | kube-apiserver | kubectl, kubelets |
| 2379 | TCP | etcd client | kube-apiserver |
| 2380 | TCP | etcd peer | etcd cluster members |
| 10250 | TCP | kubelet | kube-apiserver |
| 10259 | TCP | kube-scheduler | Health checks |
| 10257 | TCP | kube-controller-manager | Health checks |

### Talos Machine API Methods

Key gRPC methods we use (all in `MachineService`):

| Method | Description |
|--------|-------------|
| `Version` | Get Talos version |
| `ServiceList` | List all services |
| `ServiceRestart` | Restart a service |
| `Logs` | Stream service logs |
| `Memory` | Memory usage |
| `LoadAvg` | CPU load averages |
| `CPUInfo` | CPU info |
| `Processes` | Process list |
| `NetworkDeviceStats` | Network interface stats |
| `Netstat` | Network connections |
| `Read` | Read file from node |
| `Dmesg` | Kernel ring buffer |
| `EtcdStatus` | etcd member status |
| `EtcdMemberList` | etcd cluster members |
| `EtcdAlarmList` | etcd alarms |
| `Kubeconfig` | Get kubeconfig |
| `ApplyConfiguration` | Apply config patch |
| `PacketCapture` | Capture packets (pcap) |
| `Reboot` | Reboot node |
| `Shutdown` | Shutdown node |

### COSI Resource API - NOT EXTERNALLY ACCESSIBLE

> **IMPORTANT:** The COSI State API is an **internal service** and is **NOT exposed** through port 50000.

**Do NOT attempt to implement COSI gRPC client code** - it will fail with `PermissionDenied`. Use `talosctl get` as a subprocess instead.

---

## Core Philosophies

### 1. State Over Logs

**The most important principle for diagnostics:**

> Check actual system state, not log messages. Logs are history; APIs and files are truth.

```rust
// GOOD: Direct state check
let healthy = client.read_file("/run/flannel/subnet.env").await.is_ok();

// GOOD: K8s API query for current state
let pods = kube_client.list::<Pod>(&params).await?;

// BAD: Log parsing for health determination
let logs = client.logs("kubelet", 100).await?;
let healthy = !logs.contains("error");  // DON'T DO THIS
```

### 2. Reliability Hierarchy

When implementing any check, prefer data sources in this order:

| Tier | Source Type | Example | Reliability |
|------|-------------|---------|-------------|
| 1 | File/procfs state | `/run/flannel/subnet.env` | Highest |
| 2 | API responses | Talos API, K8s API | High |
| 3 | Log parsing | kubelet logs | Last resort |

### 3. Graceful Degradation

When data sources are unavailable, degrade gracefully:

```rust
// Good: Show unknown state, don't crash
match client.read_file("/some/path").await {
    Ok(content) => DiagnosticCheck::pass(...),
    Err(_) => DiagnosticCheck::unknown("check_id", "Check Name"),
}
```

### 4. No False Positives

A diagnostic showing failure for a healthy system is worse than showing unknown.

---

## Code Patterns

### Using AsyncState<T>

All data-holding components use `AsyncState<T>` for consistent loading/error handling:

```rust
pub struct MyComponent {
    state: AsyncState<MyData>,
    // ... UI state (selection, scroll, etc.)
}

#[derive(Debug, Clone, Default)]
pub struct MyData {
    // All async-loaded data goes here
}

impl MyComponent {
    pub async fn refresh(&mut self, client: &TalosClient) -> Result<()> {
        self.state.start_loading();

        match load_data(client).await {
            Ok(data) => self.state.set_data(data),
            Err(e) => self.state.set_error_with_retry(format_talos_error(&e)),
        }
        Ok(())
    }

    fn draw(&self, frame: &mut Frame, area: Rect) {
        if self.state.is_loading() && !self.state.has_data() {
            // Show loading spinner
        } else if let Some(error) = self.state.error() {
            // Show error message
        } else if let Some(data) = self.state.data() {
            // Render data
        }
    }
}
```

### Using HasHealth Trait

Implement `HasHealth` for health-related enums, then use extension traits for UI colors:

```rust
// In core
impl HasHealth for MyStatus {
    fn health(&self) -> HealthIndicator {
        match self {
            MyStatus::Good => HealthIndicator::Healthy,
            MyStatus::Bad => HealthIndicator::Error,
        }
    }
}

// In TUI - use HealthIndicatorExt for colors
let (symbol, color) = my_status.health().symbol_and_color();
```

### Diagnostic Checks

```rust
// Creating checks
DiagnosticCheck::pass("memory", "Memory", "2.1 GB / 4.0 GB (52%)")
DiagnosticCheck::fail("cni", "CNI", "Not initialized", Some(fix))
DiagnosticCheck::warn("cpu_load", "CPU Load", "High load: 4.5")
DiagnosticCheck::unknown("etcd", "Etcd")  // When data unavailable
```

---

## Adding New Features

### Adding a New Component

1. Create `components/myfeature.rs`
2. Define `MyFeatureData` struct for async-loaded data
3. Use `AsyncState<MyFeatureData>` for state management
4. Implement `Component` trait with `init()`, `update()`, `draw()`
5. Add keyboard handling in `update()` → `Action`
6. Register in `components/mod.rs`
7. Add view switching in `app.rs`

### Adding a New Diagnostic Check

1. Identify the **source of truth** (file, API, not logs)
2. Add check function to appropriate module (`core.rs` or CNI/addon provider)
3. Use `DiagnosticCheck::pass/fail/warn/unknown` constructors
4. Provide actionable `DiagnosticFix` when possible
5. Handle unavailable data gracefully (return `unknown`, don't crash)

### Adding CNI Support

1. Create `diagnostics/cni/<name>.rs` with provider-specific checks
2. Add detection logic to `cni/mod.rs` (K8s API first, file fallback)
3. Document CNI-specific requirements (kernel modules, etc.)
4. Add pod health checks for CNI pods

### Adding Addon Support

1. Add CRD names to `constants.rs` (e.g., `MY_ADDON_CRDS`)
2. Add detection in `addons/mod.rs`
3. Create `addons/<name>.rs` with specific checks if needed

---

## Project Structure

### Key Files

| File | Purpose |
|------|---------|
| `talos-rs/src/client.rs` | Talos gRPC client wrapper |
| `core/src/async_state.rs` | Loading/error state management |
| `core/src/indicators.rs` | Health indicator types |
| `core/src/diagnostics.rs` | Diagnostic types (CheckStatus, CniType) |
| `core/src/constants.rs` | Shared constants |
| `tui/src/ui_ext.rs` | Extension traits for ratatui colors |
| `tui/src/components/` | All UI components |

### Diagnostics Module Structure

```
components/diagnostics/
├── mod.rs          # DiagnosticsComponent (UI orchestrator)
├── types.rs        # DiagnosticCheck, DiagnosticFix, DiagnosticContext
├── core.rs         # Core checks (memory, cpu, services, etcd)
├── k8s.rs          # K8s client helper, CNI detection via K8s API
├── cni/
│   ├── mod.rs      # CNI detection + provider dispatch
│   └── flannel.rs  # Flannel-specific checks
└── addons/
    ├── mod.rs      # Addon detection (CRDs + pods)
    └── cert_manager.rs
```

---

## Quick Reference

### Do

- Check actual system state (files, APIs)
- Use `AsyncState<T>` for component data
- Implement `HasHealth` for health enums
- Provide actionable fix suggestions
- Degrade gracefully when sources unavailable
- Add tests for new core functionality
- Use constants from `talos_pilot_core::constants`

### Don't

- Parse logs to determine health status
- Use string matching without timestamp validation
- Crash when data sources are unavailable
- Show "failed" when "unknown" is more accurate
- Use VIP for Talos API endpoint (won't work if etcd is down)
- Duplicate health indicator logic (use HasHealth trait)
- Store UI state mixed with async data

### Reliability Checklist for New Checks

- [ ] What is the source of truth for this state?
- [ ] Can we check that source directly (file, API)?
- [ ] If using logs, do we validate timestamps?
- [ ] What happens if the data source is unavailable?
- [ ] Can this check produce false positives?
- [ ] Is the failure mode graceful?

---

## Testing

```bash
# Run all tests
cargo test --all

# Run specific crate tests
cargo test --package talos-pilot-core

# Current test counts
# - Core: 47 unit tests + 10 doc tests
# - TUI: 8 tests
# - talos-rs: 32 tests + 1 doc test (includes gRPC metadata format tests)
```

### Before Merging

1. **No false positives:** Create error condition, fix it, verify check shows healthy
2. **Graceful degradation:** Disconnect API, verify no crashes
3. **Stale log immunity:** Ensure old log messages don't affect current state
4. **Zero warnings:** `cargo clippy --all --all-targets -- -D warnings` should pass clean
5. **Formatting:** `cargo fmt --all -- --check` should pass

---

## Documentation

| Doc | Purpose |
|-----|---------|
| `README.md` | User-facing documentation |
| `internal-docs/talos-pilot-design-doc.md` | Original design document |
| `internal-docs/phase-2-features.md` | Feature implementation tracking |
| `internal-docs/refactoring_report.md` | Code refactoring progress |
| `internal-docs/*-plan.md` | Feature planning documents |
| `internal-docs/*-progress.md` | Feature implementation progress |
