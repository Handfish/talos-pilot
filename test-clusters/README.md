# Test Clusters

Test Talos clusters for developing and testing talos-pilot features.

## Cluster Types

| Type | Use Case | Physical Disks | Setup Time |
|------|----------|----------------|------------|
| Docker | Most development | No | ~30 seconds |
| QEMU | Storage/Disks view testing | Yes | ~2 minutes |

---

## QEMU Clusters (Physical Disks)

QEMU-based clusters run real VMs with virtual disks that appear as physical devices (`/dev/sda`, etc.). **Required for testing the Storage/Disks view.**

### Quick Start

```bash
# 1. Stop Docker clusters (they use ports 50000/6443)
sudo systemctl stop docker

# 2. Create the QEMU cluster (runs in foreground)
./test-clusters/scripts/test-cluster-qemu.sh create

# 3. In another terminal, wait for "maintenance mode" then:
./test-clusters/scripts/test-cluster-qemu.sh apply

# 4. Wait for install to complete (watch QEMU window), then:
./test-clusters/scripts/test-cluster-qemu.sh bootstrap

# 5. Test in talos-pilot
cargo run
# Switch to 'talos-qemu' context, select node, press 's' for Storage

# 6. Destroy when done
./test-clusters/scripts/test-cluster-qemu.sh destroy

# 7. Restart Docker
sudo systemctl start docker
```

### Commands

| Command | Description |
|---------|-------------|
| `create` | Create VM, download ISO, generate config, start in foreground |
| `create-bg` | Same as create but runs VM in background |
| `apply` | Apply config to VM in maintenance mode |
| `bootstrap` | Bootstrap the cluster after config is applied |
| `status` | Show cluster status and connection info |
| `destroy` | Stop VM, delete files, remove talosconfig context |
| `connect` | Show connection info and useful commands |

### Prerequisites

- `qemu-system-x86_64` installed (`sudo apt install qemu-system-x86`)
- KVM enabled (`/dev/kvm` accessible, user in `kvm` group)
- Ports 50000 and 6443 available (stop Docker clusters first)

### Why Not `talosctl cluster create qemu`?

The official command has TLS issues in maintenance mode. Our script works around this by running QEMU directly with user-mode networking.

---

## Docker Clusters (Quick Setup)

Docker-based Talos clusters for testing most features.

## Quick Start

```bash
# Create a cluster with Cilium eBPF + Hubble
./scripts/cluster.sh create cilium-hubble

# Or replace existing cluster with --force
./scripts/cluster.sh create cilium-ebpf --force

# Export kubeconfig
export KUBECONFIG=$(pwd)/output/kubeconfig

# Check what you have
./scripts/cluster.sh status

# Add comprehensive test workloads (all scenarios)
./scripts/cluster.sh workloads kitchen-sink

# Run talos-pilot against it
cd ../
cargo run --bin talos-pilot-tui

# Clean up when done
./scripts/cluster.sh destroy
```

## Available Profiles

| Profile | CNI | KubeSpan | kube-proxy | Notes |
|---------|-----|----------|------------|-------|
| `flannel` | Flannel | No | Yes | Simplest setup |
| `cilium` | Cilium | No | Yes | Legacy mode |
| `cilium-ebpf` | Cilium | No | No | Modern eBPF mode |
| `cilium-hubble` | Cilium | No | No | With Hubble observability |
| `kubespan` | Flannel | Yes | Yes | WireGuard mesh |
| `cilium-kubespan` | Cilium | Yes | No | Problematic combo (for testing warnings) |

## Test Scenarios

### Testing CNI + KubeSpan Warnings

```bash
# Create the problematic combination
./scripts/cluster.sh create cilium-kubespan

# Run talos-pilot - should show warning in diagnostics
cargo run --bin talos-pilot-tui
# Navigate to Diagnostics (d) and check CNI section
```

### Testing Workload Health

```bash
# Create any cluster profile
./scripts/cluster.sh create cilium-ebpf

# RECOMMENDED: Use kitchen-sink for comprehensive testing
# This removes the control plane taint and creates all workload scenarios
./scripts/cluster.sh workloads kitchen-sink

# Or add individual workload scenarios:
./scripts/cluster.sh workloads healthy      # Healthy nginx + redis
./scripts/cluster.sh workloads crashloop    # CrashLoopBackOff
./scripts/cluster.sh workloads imagepull    # ImagePullBackOff
./scripts/cluster.sh workloads pending      # Pending (resource-constrained)
./scripts/cluster.sh workloads pdb          # With PodDisruptionBudget
./scripts/cluster.sh workloads oomkill      # OOMKilled (memory limit exceeded)
./scripts/cluster.sh workloads highrestarts # High restart count
./scripts/cluster.sh workloads degraded     # Partial replicas ready
./scripts/cluster.sh workloads statefulset  # StatefulSet workloads
./scripts/cluster.sh workloads daemonset    # DaemonSet workloads
./scripts/cluster.sh workloads mixed        # Various workload types

# Check what you have
kubectl get pods -A
```

#### Kitchen-Sink Details

The `kitchen-sink` command creates 7 test namespaces with all workload scenarios:

| Namespace | Workloads | Pod States |
|-----------|-----------|------------|
| `test-healthy` | nginx, redis | Running |
| `test-failing` | crasher, bad-image, pending-pod | CrashLoopBackOff, ImagePullBackOff, Pending |
| `test-oomkill` | memory-hog | OOMKilled (high restarts) |
| `test-restarts` | flaky-app | High restart count |
| `test-degraded` | partial-deploy | Partial replicas ready |
| `test-stateful` | postgres, redis-cluster | StatefulSet pods |
| `test-daemonset` | node-agent, fluentd | DaemonSet pods |

This provides comprehensive coverage for testing the Workloads screen (`w` hotkey).

**Note:** On single-node clusters, `kitchen-sink` automatically removes the control plane NoSchedule taint to allow all pods to schedule.

### Testing Hubble Flows

```bash
# Create cluster with Hubble
./scripts/cluster.sh create cilium-hubble

# Access Hubble UI
kubectl port-forward -n kube-system svc/hubble-ui 12000:80

# Or use Hubble CLI
hubble observe --follow
```

### Testing Rolling Operations (Multi-Node)

For testing rolling drain/reboot across multiple nodes, use the dedicated script:

```bash
# Create 4-node cluster (1 control plane + 3 workers)
./scripts/rolling-ops-test.sh create

# Check node layout and workload distribution
./scripts/rolling-ops-test.sh status

# Run talos-pilot
export KUBECONFIG=$(pwd)/output/kubeconfig
cargo run --bin talos-pilot

# In talos-pilot:
# 1. Press 'O' (capital O) in cluster view
# 2. Select nodes with Space/Enter - shows [1], [2], [3] order
# 3. Press 'd' for rolling drain or 'r' for rolling reboot
# 4. Confirm with 'y'

# Clean up
./scripts/rolling-ops-test.sh destroy
```

The rolling ops test cluster includes:
- 4 nodes for realistic multi-node testing
- Pre-configured `test-drainable` workloads with PDB
- Audit logging to `~/.talos-pilot/audit.log`

### Testing Pre-Operation Health Checks

```bash
# Create cluster
./scripts/cluster.sh create cilium-ebpf

# Add workloads with issues
./scripts/cluster.sh workloads crashloop
./scripts/cluster.sh workloads pdb

# Now test pre-operation checks - should show:
# - Pods in CrashLoopBackOff
# - PDBs that would block drain
```

## Directory Structure

```
test-clusters/
├── README.md           # This file
├── patches/            # Talos machine config patches
│   ├── kubespan.yaml       # Enable KubeSpan
│   ├── cilium-cni.yaml     # Disable default CNI for Cilium
│   ├── cilium-ebpf.yaml    # Cilium eBPF mode (no kube-proxy)
│   └── hubble.yaml         # Hubble config reference
├── scripts/
│   ├── cluster.sh          # Main cluster management script
│   ├── rolling-ops-test.sh # 4-node cluster for rolling operations
│   └── nuke.sh             # Complete cluster cleanup
└── output/             # Generated files (gitignored)
    └── kubeconfig      # Cluster kubeconfig
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `TALOS_CLUSTER_NAME` | `talos-pilot` | Name of the Docker cluster |
| `TALOS_CONTROLPLANES` | `1` | Number of control plane nodes |
| `TALOS_WORKERS` | `0` | Number of worker nodes |
| `TALOS_VERSION` | `v1.9.0` | Talos version to use |
| `TALOS_PROVISIONER` | `docker` | Provisioner (docker or qemu) |

## Tips

### Run Multiple Clusters

```bash
# Cluster 1: Cilium
TALOS_CLUSTER_NAME=cilium-test ./scripts/cluster.sh create cilium-ebpf

# Cluster 2: Flannel + KubeSpan
TALOS_CLUSTER_NAME=kubespan-test ./scripts/cluster.sh create kubespan

# Switch between them
talosctl config context cilium-test
talosctl config context kubespan-test
```

### 3-Node Control Plane (for etcd quorum testing)

```bash
TALOS_CONTROLPLANES=3 TALOS_WORKERS=2 ./scripts/cluster.sh create cilium-ebpf
```

### Check Cilium Status

```bash
# Cilium CLI (if installed)
cilium status

# Or via kubectl
kubectl -n kube-system exec ds/cilium -- cilium status
```

### Check KubeSpan Peers

```bash
talosctl get kubespanpeerstatus -n 10.5.0.2
```

## Troubleshooting

### Nodes NotReady after Cilium install

Cilium takes a minute to initialize. Check pod status:

```bash
kubectl get pods -n kube-system -l k8s-app=cilium
```

### KubeSpan peers not connecting

Check discovery service:

```bash
talosctl get discoveryservice -n 10.5.0.2
```

### Cluster won't start

Check Docker resources - Talos needs memory:

```bash
docker system df
docker system prune  # Clean up if needed
```
