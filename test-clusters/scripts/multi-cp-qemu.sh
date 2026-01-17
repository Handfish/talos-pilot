#!/bin/bash
#
# Multi-Control-Plane QEMU Test Cluster
#
# Creates a 3 control plane + 3 worker Talos cluster using QEMU
# for testing etcd quorum and multi-node scenarios.
#
# This reproduces the user's production config pattern:
#   endpoints: [vip, cp1, cp2, cp3]
#   nodes: [cp1, cp2, cp3, w1, w2, w3]
#
# Usage:
#   ./multi-cp-qemu.sh create     - Create and start all VMs
#   ./multi-cp-qemu.sh destroy    - Destroy all VMs and clean up
#   ./multi-cp-qemu.sh status     - Show cluster status
#   ./multi-cp-qemu.sh bootstrap  - Bootstrap the cluster (after config applied)
#   ./multi-cp-qemu.sh config     - Generate and show talosconfig
#   ./multi-cp-qemu.sh help       - Show this help
#

set -e

# Configuration
CLUSTER_NAME="multi-cp-test"
WORK_DIR="/tmp/talos-multi-cp"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/talos-pilot"
TALOS_VERSION="v1.12.1"
ISO_URL="https://factory.talos.dev/image/376567988ad370138ad8b2698212367b8edcb69b5fd68c80be1f2ec7d603b4ba/${TALOS_VERSION}/metal-amd64.iso"
ISO_PATH="$CACHE_DIR/talos-${TALOS_VERSION}.iso"

# Minimum requirements per Talos docs
CP_MEMORY="2048"    # 2 GiB
CP_CPUS="2"
CP_DISK="10G"

WORKER_MEMORY="1024"  # 1 GiB
WORKER_CPUS="1"
WORKER_DISK="10G"

# Network configuration - using a bridge network
# We'll use 192.168.100.0/24 subnet
BRIDGE_NAME="talos-br0"
SUBNET="192.168.100"

# Node IPs (static)
CP1_IP="${SUBNET}.11"
CP2_IP="${SUBNET}.12"
CP3_IP="${SUBNET}.13"
W1_IP="${SUBNET}.21"
W2_IP="${SUBNET}.22"
W3_IP="${SUBNET}.23"
GATEWAY_IP="${SUBNET}.1"

# Virtual IP for the cluster endpoint
VIP="${SUBNET}.10"

# Hostnames (to simulate user's production config with DNS names)
VIP_HOSTNAME="cluster.test.local"
CP1_HOSTNAME="cp1.test.local"
CP2_HOSTNAME="cp2.test.local"
CP3_HOSTNAME="cp3.test.local"
W1_HOSTNAME="w1.test.local"
W2_HOSTNAME="w2.test.local"
W3_HOSTNAME="w3.test.local"

# Port forwards from host (for accessing from localhost)
# We'll forward to CP1 by default
HOST_TALOS_PORT="50000"
HOST_K8S_PORT="6443"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[INFO]${NC} $*"; }
log_success() { echo -e "${GREEN}[OK]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

usage() {
    cat << EOF
Multi-Control-Plane QEMU Test Cluster

Creates a 3 CP + 3 worker Talos cluster for testing etcd quorum.
Uses HOSTNAMES (via /etc/hosts) to match user's production config pattern.

USAGE:
    $0 <command>

COMMANDS:
    create      Create and start all 6 VMs (requires sudo for bridge + /etc/hosts)
    destroy     Destroy all VMs and clean up
    status      Show cluster status
    apply       Apply configs to VMs in maintenance mode
    bootstrap   Bootstrap the cluster
    config      Generate talosconfig for the cluster
    connect     Show connection info
    help        Show this help

WORKFLOW:
    1. $0 create          # Creates bridge, /etc/hosts entries, and starts 6 VMs
    2. Wait for VMs to boot into maintenance mode (~30s)
    3. $0 apply           # Apply Talos configs to all nodes
    4. Wait for nodes to install and reboot (~2-3 min)
    5. $0 bootstrap       # Bootstrap etcd on first control plane
    6. cargo run          # Test with talos-pilot

ALTERNATIVELY (wizard mode):
    1. $0 create
    2. cargo run -- --insecure --endpoint $CP1_IP
    3. Use wizard to bootstrap

HOSTNAMES (added to /etc/hosts):
    $VIP_HOSTNAME     -> $VIP (Virtual IP)
    $CP1_HOSTNAME     -> $CP1_IP
    $CP2_HOSTNAME     -> $CP2_IP
    $CP3_HOSTNAME     -> $CP3_IP
    $W1_HOSTNAME      -> $W1_IP
    $W2_HOSTNAME      -> $W2_IP
    $W3_HOSTNAME      -> $W3_IP

TALOSCONFIG PATTERN (matches user's production):
    endpoints: [$VIP_HOSTNAME, $CP1_HOSTNAME, $CP2_HOSTNAME, $CP3_HOSTNAME]
    nodes: [$CP1_HOSTNAME, $CP2_HOSTNAME, $CP3_HOSTNAME, $W1_HOSTNAME, $W2_HOSTNAME, $W3_HOSTNAME]

REQUIREMENTS:
    - qemu-system-x86_64
    - KVM enabled (/dev/kvm)
    - sudo access (for bridge network + /etc/hosts)
    - ~12 GB RAM available (6 GB for CPs + 3 GB for workers + overhead)

EOF
    exit 0
}

check_prereqs() {
    local missing=()

    command -v qemu-system-x86_64 &>/dev/null || missing+=("qemu-system-x86_64")
    command -v ip &>/dev/null || missing+=("iproute2")
    command -v talosctl &>/dev/null || missing+=("talosctl")
    command -v dnsmasq &>/dev/null || missing+=("dnsmasq")

    if [[ ${#missing[@]} -gt 0 ]]; then
        log_error "Missing required tools: ${missing[*]}"
        echo "Install with: sudo apt install qemu-system-x86 iproute2 dnsmasq"
        exit 1
    fi

    if [[ ! -r /dev/kvm ]]; then
        log_error "/dev/kvm not accessible. Add yourself to kvm group: sudo usermod -aG kvm \$USER"
        exit 1
    fi

    # Check available memory
    local avail_mem=$(awk '/MemAvailable/ {print int($2/1024)}' /proc/meminfo)
    local needed_mem=$((CP_MEMORY * 3 + WORKER_MEMORY * 3))
    if [[ $avail_mem -lt $needed_mem ]]; then
        log_warn "Low memory: ${avail_mem}MB available, ${needed_mem}MB needed"
        log_warn "Cluster may be slow or fail to start"
    fi
}

download_iso() {
    mkdir -p "$CACHE_DIR"

    if [[ -f "$ISO_PATH" ]]; then
        log_info "ISO cached: $ISO_PATH"
        return
    fi

    log_info "Downloading Talos ISO..."
    curl -L -o "$ISO_PATH" "$ISO_URL"
    log_success "ISO downloaded"
}

setup_bridge() {
    log_info "Setting up bridge network (requires sudo)..."

    # Check if bridge already exists
    if ip link show "$BRIDGE_NAME" &>/dev/null; then
        log_info "Bridge $BRIDGE_NAME already exists"
        return
    fi

    # Create bridge
    sudo ip link add name "$BRIDGE_NAME" type bridge
    sudo ip addr add "${GATEWAY_IP}/24" dev "$BRIDGE_NAME"
    sudo ip link set "$BRIDGE_NAME" up

    # Save original ip_forward value before changing it
    local original_ip_forward=$(cat /proc/sys/net/ipv4/ip_forward)
    echo "$original_ip_forward" > "$WORK_DIR/ip_forward.orig"

    # Enable IP forwarding and NAT for internet access
    sudo sysctl -w net.ipv4.ip_forward=1 >/dev/null

    # Get default interface for NAT
    local default_iface=$(ip route | grep default | awk '{print $5}' | head -1)
    if [[ -n "$default_iface" ]]; then
        # Save default interface for teardown
        echo "$default_iface" > "$WORK_DIR/default_iface"
        sudo iptables -t nat -A POSTROUTING -s "${SUBNET}.0/24" -o "$default_iface" -j MASQUERADE
        sudo iptables -A FORWARD -i "$BRIDGE_NAME" -o "$default_iface" -j ACCEPT
        sudo iptables -A FORWARD -i "$default_iface" -o "$BRIDGE_NAME" -m state --state RELATED,ESTABLISHED -j ACCEPT
    fi

    log_success "Bridge network created: $BRIDGE_NAME (${GATEWAY_IP}/24)"
}

teardown_bridge() {
    log_info "Tearing down bridge network..."

    # Use saved default interface if available (ensures we remove the same rules we added)
    local default_iface=""
    if [[ -f "$WORK_DIR/default_iface" ]]; then
        default_iface=$(cat "$WORK_DIR/default_iface")
    else
        # Fallback to current default interface
        default_iface=$(ip route | grep default | awk '{print $5}' | head -1)
    fi

    # Remove iptables rules
    if [[ -n "$default_iface" ]]; then
        sudo iptables -t nat -D POSTROUTING -s "${SUBNET}.0/24" -o "$default_iface" -j MASQUERADE 2>/dev/null || true
        sudo iptables -D FORWARD -i "$BRIDGE_NAME" -o "$default_iface" -j ACCEPT 2>/dev/null || true
        sudo iptables -D FORWARD -i "$default_iface" -o "$BRIDGE_NAME" -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || true
    fi

    # Restore original ip_forward value
    if [[ -f "$WORK_DIR/ip_forward.orig" ]]; then
        local original_ip_forward=$(cat "$WORK_DIR/ip_forward.orig")
        sudo sysctl -w net.ipv4.ip_forward="$original_ip_forward" >/dev/null
        log_info "Restored ip_forward to $original_ip_forward"
    fi

    # Remove bridge
    if ip link show "$BRIDGE_NAME" &>/dev/null; then
        sudo ip link set "$BRIDGE_NAME" down
        sudo ip link delete "$BRIDGE_NAME"
    fi

    log_success "Bridge removed"
}

setup_hosts() {
    log_info "Setting up /etc/hosts entries (requires sudo)..."

    # Remove any existing entries for our test domain
    sudo sed -i '/\.test\.local/d' /etc/hosts

    # Add new entries
    {
        echo "$VIP $VIP_HOSTNAME"
        echo "$CP1_IP $CP1_HOSTNAME"
        echo "$CP2_IP $CP2_HOSTNAME"
        echo "$CP3_IP $CP3_HOSTNAME"
        echo "$W1_IP $W1_HOSTNAME"
        echo "$W2_IP $W2_HOSTNAME"
        echo "$W3_IP $W3_HOSTNAME"
    } | sudo tee -a /etc/hosts > /dev/null

    log_success "Added /etc/hosts entries for *.test.local"
}

setup_dhcp() {
    log_info "Starting DHCP server (dnsmasq) on bridge..."

    # Check if dnsmasq is installed
    if ! command -v dnsmasq &>/dev/null; then
        log_error "dnsmasq not installed. Install with: sudo apt install dnsmasq"
        exit 1
    fi

    # Kill any existing dnsmasq for our bridge
    sudo pkill -f "dnsmasq.*${BRIDGE_NAME}" 2>/dev/null || true

    # Create dnsmasq config in /run (AppArmor allows dnsmasq to read from /run)
    local dnsmasq_conf="/run/talos-dnsmasq.conf"
    local dnsmasq_pid="/run/talos-dnsmasq.pid"
    local dnsmasq_lease="/run/talos-dnsmasq.leases"

    sudo tee "$dnsmasq_conf" > /dev/null << EOF
# DHCP server for Talos test cluster
interface=${BRIDGE_NAME}
bind-interfaces
port=0
dhcp-range=${SUBNET}.100,${SUBNET}.199,12h
dhcp-option=option:router,${GATEWAY_IP}
dhcp-option=option:dns-server,8.8.8.8,8.8.4.4
dhcp-leasefile=${dnsmasq_lease}
EOF

    # Store paths for cleanup
    echo "$dnsmasq_conf" > "$WORK_DIR/dnsmasq_conf_path"
    echo "$dnsmasq_pid" > "$WORK_DIR/dnsmasq_pid_path"

    # Start dnsmasq (use aa-exec to bypass AppArmor restrictions if available)
    if command -v aa-exec &>/dev/null; then
        sudo aa-exec -p unconfined -- dnsmasq --conf-file="$dnsmasq_conf" --pid-file="$dnsmasq_pid" --log-facility="$WORK_DIR/dnsmasq.log"
    else
        sudo dnsmasq --conf-file="$dnsmasq_conf" --pid-file="$dnsmasq_pid" --log-facility="$WORK_DIR/dnsmasq.log"
    fi

    log_success "DHCP server started (range: ${SUBNET}.100-199)"
}

stop_dhcp() {
    # Try stored PID path first
    local pid_path="/run/talos-dnsmasq.pid"
    if [[ -f "$WORK_DIR/dnsmasq_pid_path" ]]; then
        pid_path=$(cat "$WORK_DIR/dnsmasq_pid_path")
    fi

    if [[ -f "$pid_path" ]]; then
        local pid=$(cat "$pid_path")
        if kill -0 "$pid" 2>/dev/null; then
            sudo kill "$pid" 2>/dev/null || true
        fi
        sudo rm -f "$pid_path"
    fi

    # Cleanup config and lease files
    local conf_path="/run/talos-dnsmasq.conf"
    if [[ -f "$WORK_DIR/dnsmasq_conf_path" ]]; then
        conf_path=$(cat "$WORK_DIR/dnsmasq_conf_path")
    fi
    sudo rm -f "$conf_path" /run/talos-dnsmasq.leases

    # Fallback: kill any dnsmasq for our bridge
    sudo pkill -f "dnsmasq.*${BRIDGE_NAME}" 2>/dev/null || true
}

teardown_hosts() {
    log_info "Removing /etc/hosts entries..."
    sudo sed -i '/\.test\.local/d' /etc/hosts
    log_success "Removed /etc/hosts entries"
}

create_tap() {
    local tap_name="$1"

    # Delete existing tap if it exists (might be orphaned from previous run)
    if ip link show "$tap_name" &>/dev/null; then
        sudo ip link set "$tap_name" down 2>/dev/null || true
        sudo ip tuntap del dev "$tap_name" mode tap 2>/dev/null || true
    fi

    sudo ip tuntap add dev "$tap_name" mode tap user "$USER"
    sudo ip link set "$tap_name" master "$BRIDGE_NAME"
    sudo ip link set "$tap_name" up
}

delete_tap() {
    local tap_name="$1"

    if ip link show "$tap_name" &>/dev/null; then
        sudo ip link set "$tap_name" down
        sudo ip tuntap del dev "$tap_name" mode tap
    fi
}

create_disk() {
    local disk_path="$1"
    local size="$2"

    if [[ ! -f "$disk_path" ]]; then
        qemu-img create -f qcow2 "$disk_path" "$size" >/dev/null
    fi
}

start_vm() {
    local name="$1"
    local ip="$2"
    local memory="$3"
    local cpus="$4"
    local disk_size="$5"
    local role="$6"  # controlplane or worker

    local tap_name="tap-${name}"
    local disk_path="$WORK_DIR/${name}.qcow2"
    local pid_file="$WORK_DIR/${name}.pid"
    local mac=$(printf '52:54:00:%02x:%02x:%02x' $((RANDOM%256)) $((RANDOM%256)) $((RANDOM%256)))

    log_info "Starting $name ($role) - IP: $ip, RAM: ${memory}MB, CPUs: $cpus"

    # Create TAP interface
    create_tap "$tap_name"

    # Create disk
    create_disk "$disk_path" "$disk_size"

    # Start QEMU
    qemu-system-x86_64 \
        -name "$name" \
        -m "$memory" \
        -smp "$cpus" \
        -cpu host \
        -enable-kvm \
        -drive "file=$disk_path,format=qcow2,if=virtio" \
        -cdrom "$ISO_PATH" \
        -boot d \
        -netdev "tap,id=net0,ifname=$tap_name,script=no,downscript=no" \
        -device "virtio-net-pci,netdev=net0,mac=$mac" \
        -display none \
        -daemonize \
        -pidfile "$pid_file" \
        2>/dev/null

    # Store IP mapping
    echo "$ip" > "$WORK_DIR/${name}.ip"
    echo "$mac" > "$WORK_DIR/${name}.mac"
}

stop_vm() {
    local name="$1"
    local pid_file="$WORK_DIR/${name}.pid"
    local tap_name="tap-${name}"

    if [[ -f "$pid_file" ]]; then
        local pid=$(cat "$pid_file")
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
        rm -f "$pid_file"
    fi

    delete_tap "$tap_name"
}

create_cluster() {
    check_prereqs

    log_info "Creating multi-CP test cluster..."
    echo ""
    log_info "This will create:"
    echo "  - 3 control plane VMs (${CP_MEMORY}MB RAM, ${CP_CPUS} CPUs each)"
    echo "  - 3 worker VMs (${WORKER_MEMORY}MB RAM, ${WORKER_CPUS} CPU each)"
    echo "  - Bridge network: $BRIDGE_NAME (${SUBNET}.0/24)"
    echo ""

    # Ensure work directory exists and is owned by current user
    # (may have been left owned by root from a previous partial run)
    if [[ -d "$WORK_DIR" ]]; then
        sudo rm -rf "$WORK_DIR"
    fi
    mkdir -p "$WORK_DIR"

    download_iso
    setup_bridge
    setup_dhcp
    setup_hosts

    # Start control plane VMs
    start_vm "cp1" "$CP1_IP" "$CP_MEMORY" "$CP_CPUS" "$CP_DISK" "controlplane"
    start_vm "cp2" "$CP2_IP" "$CP_MEMORY" "$CP_CPUS" "$CP_DISK" "controlplane"
    start_vm "cp3" "$CP3_IP" "$CP_MEMORY" "$CP_CPUS" "$CP_DISK" "controlplane"

    # Start worker VMs
    start_vm "w1" "$W1_IP" "$WORKER_MEMORY" "$WORKER_CPUS" "$WORKER_DISK" "worker"
    start_vm "w2" "$W2_IP" "$WORKER_MEMORY" "$WORKER_CPUS" "$WORKER_DISK" "worker"
    start_vm "w3" "$W3_IP" "$WORKER_MEMORY" "$WORKER_CPUS" "$WORKER_DISK" "worker"

    echo ""
    log_success "All VMs started!"
    echo ""
    echo -e "${CYAN}=== Next Steps ===${NC}"
    echo ""
    echo "1. Wait for VMs to boot into maintenance mode (~30-60 seconds)"
    echo "   Check with: $0 status"
    echo ""
    echo "2. Apply Talos configuration to all nodes:"
    echo "   $0 apply"
    echo ""
    echo "3. Wait for nodes to install and reboot (~2-3 minutes)"
    echo ""
    echo "4. Bootstrap the cluster:"
    echo "   $0 bootstrap"
    echo ""
    echo "5. Test with talos-pilot:"
    echo "   cargo run"
    echo ""
    echo -e "${CYAN}=== OR use wizard mode ===${NC}"
    echo ""
    echo "   cargo run -- --insecure --endpoint $CP1_IP"
    echo ""
}

generate_config() {
    log_info "Generating Talos configuration..."

    # Generate base config with VIP as endpoint
    talosctl gen config "$CLUSTER_NAME" "https://${VIP}:6443" \
        --additional-sans "$VIP,$CP1_IP,$CP2_IP,$CP3_IP" \
        --output-dir "$WORK_DIR" \
        --force

    # Patch controlplane.yaml to add VIP and fix install disk (virtio = /dev/vda)
    cat > "$WORK_DIR/vip-patch.yaml" << EOF
machine:
  install:
    disk: /dev/vda
  network:
    interfaces:
      - interface: ens3
        dhcp: false
        addresses:
          - \${IP}/24
        routes:
          - network: 0.0.0.0/0
            gateway: $GATEWAY_IP
        vip:
          ip: $VIP
EOF

    # Create per-node configs
    for node in cp1 cp2 cp3; do
        local ip_var="${node^^}_IP"
        local ip="${!ip_var}"

        # Substitute IP and create node-specific config
        sed "s/\${IP}/$ip/g" "$WORK_DIR/vip-patch.yaml" > "$WORK_DIR/${node}-patch.yaml"

        talosctl machineconfig patch "$WORK_DIR/controlplane.yaml" \
            --patch @"$WORK_DIR/${node}-patch.yaml" \
            --output "$WORK_DIR/${node}.yaml"
    done

    # Create worker configs with static IPs and install disk
    for node in w1 w2 w3; do
        local ip_var="${node^^}_IP"
        local ip="${!ip_var}"

        cat > "$WORK_DIR/${node}-patch.yaml" << EOF
machine:
  install:
    disk: /dev/vda
  network:
    interfaces:
      - interface: ens3
        dhcp: false
        addresses:
          - ${ip}/24
        routes:
          - network: 0.0.0.0/0
            gateway: $GATEWAY_IP
EOF

        talosctl machineconfig patch "$WORK_DIR/worker.yaml" \
            --patch @"$WORK_DIR/${node}-patch.yaml" \
            --output "$WORK_DIR/${node}.yaml"
    done

    log_success "Generated configs in $WORK_DIR"
}

apply_configs() {
    log_info "Applying configurations to nodes..."

    # Generate configs first
    generate_config

    # Discover nodes in maintenance mode (use leases file for speed)
    log_info "Discovering nodes in maintenance mode..."
    local maintenance_ips=()
    local ips_to_scan=()

    # First try to get IPs from leases file
    if [[ -f /run/talos-dnsmasq.leases ]] && [[ -s /run/talos-dnsmasq.leases ]]; then
        while read -r _ _ ip _; do
            ips_to_scan+=("$ip")
        done < /run/talos-dnsmasq.leases
        log_info "Found ${#ips_to_scan[@]} DHCP leases"
    fi

    # Fallback to scanning if no leases
    if [[ ${#ips_to_scan[@]} -eq 0 ]]; then
        log_info "No leases found, scanning range..."
        for i in $(seq 100 120); do
            ips_to_scan+=("${SUBNET}.$i")
        done
    fi

    for ip in "${ips_to_scan[@]}"; do
        # Check for maintenance mode error (means node is alive and in maintenance)
        if timeout 2 talosctl version --insecure --nodes "$ip" 2>&1 | grep -q "maintenance mode"; then
            maintenance_ips+=("$ip")
            log_info "  Found: $ip"
        fi
    done

    if [[ ${#maintenance_ips[@]} -eq 0 ]]; then
        log_error "No nodes found in maintenance mode!"
        log_error "Wait for VMs to boot and try again."
        exit 1
    fi

    log_info "Found ${#maintenance_ips[@]} nodes in maintenance mode"

    if [[ ${#maintenance_ips[@]} -lt 6 ]]; then
        log_warn "Expected 6 nodes but found ${#maintenance_ips[@]}"
        log_warn "Some VMs may still be booting. Continue anyway? (y/n)"
        read -r answer
        if [[ "$answer" != "y" ]]; then
            exit 1
        fi
    fi

    # Apply configs in order: first 3 are CPs, next 3 are workers
    local configs=(cp1 cp2 cp3 w1 w2 w3)
    local idx=0
    for maint_ip in "${maintenance_ips[@]}"; do
        if [[ $idx -ge ${#configs[@]} ]]; then
            log_warn "More nodes than configs, skipping $maint_ip"
            continue
        fi

        local node="${configs[$idx]}"
        log_info "Applying $node config to $maint_ip..."
        if talosctl apply-config --insecure --nodes "$maint_ip" --file "$WORK_DIR/${node}.yaml"; then
            log_success "$node configured (was $maint_ip, will become ${node^^}_IP)"
        else
            log_warn "Failed to configure $node at $maint_ip"
        fi
        idx=$((idx + 1))
    done

    echo ""
    log_success "Configs applied! Nodes are now installing Talos to disk."
    echo ""
    echo -e "${CYAN}=== IMPORTANT: Next Steps ===${NC}"
    echo ""
    echo "1. Wait ~2 minutes for installation to complete"
    echo "   (You can watch disk sizes grow: ls -lh $WORK_DIR/*.qcow2)"
    echo ""
    echo "2. Restart VMs to boot from disk (NOT the CD):"
    echo -e "   ${YELLOW}$0 reboot-disk${NC}"
    echo ""
    echo "3. Wait ~30 seconds, then verify nodes are up:"
    echo "   $0 check"
    echo ""
    echo "4. Once all nodes show ✓, bootstrap the cluster:"
    echo "   $0 bootstrap"
    echo ""
    echo "After reboot-disk, nodes will have these IPs:"
    echo "  cp1: $CP1_IP"
    echo "  cp2: $CP2_IP"
    echo "  cp3: $CP3_IP"
    echo "  w1:  $W1_IP"
    echo "  w2:  $W2_IP"
    echo "  w3:  $W3_IP"
}

setup_talosconfig() {
    log_info "Setting up talosconfig with IPs..."

    # Merge the generated config
    talosctl config merge "$WORK_DIR/talosconfig"

    # Set endpoints using IPs (vIP + CP IPs)
    # Note: Using IPs because node certs have random hostnames
    talosctl --context "$CLUSTER_NAME" config endpoint \
        "$VIP" "$CP1_IP" "$CP2_IP" "$CP3_IP"

    # Set nodes to all nodes using IPs
    talosctl --context "$CLUSTER_NAME" config node \
        "$CP1_IP" "$CP2_IP" "$CP3_IP" \
        "$W1_IP" "$W2_IP" "$W3_IP"

    log_success "talosconfig updated"
    echo ""
    echo "Context: $CLUSTER_NAME"
    echo "Endpoints: $VIP, $CP1_IP, $CP2_IP, $CP3_IP"
    echo "Nodes: $CP1_IP, $CP2_IP, $CP3_IP, $W1_IP, $W2_IP, $W3_IP"
}

bootstrap_cluster() {
    log_info "Bootstrapping cluster on $CP1_IP..."

    # Setup talosconfig first
    setup_talosconfig

    # Bootstrap on first control plane using direct talosconfig path and IP
    if talosctl --talosconfig "$WORK_DIR/talosconfig" --endpoints "$CP1_IP" --nodes "$CP1_IP" bootstrap; then
        log_success "Bootstrap initiated!"
        echo ""
        log_info "Waiting for cluster to form..."
        sleep 15

        # Check etcd members
        log_info "Checking etcd members..."
        talosctl --talosconfig "$WORK_DIR/talosconfig" --endpoints "$CP1_IP" --nodes "$CP1_IP" etcd members

        echo ""
        log_success "Cluster bootstrapped! Test with: cargo run"
        log_info "Select context: $CLUSTER_NAME"
    else
        log_error "Bootstrap failed. Are nodes configured and rebooted?"
        log_error "Run: $0 check   to verify nodes are up"
    fi
}

destroy_cluster() {
    log_info "Destroying multi-CP cluster..."

    # Stop all VMs
    for node in cp1 cp2 cp3 w1 w2 w3; do
        stop_vm "$node"
    done

    # Stop DHCP server
    stop_dhcp

    # Teardown bridge and hosts
    teardown_bridge
    teardown_hosts

    # Remove work directory
    if [[ -d "$WORK_DIR" ]]; then
        rm -rf "$WORK_DIR"
    fi

    # Remove talosconfig context
    talosctl config remove "$CLUSTER_NAME" --noconfirm 2>/dev/null || true

    log_success "Cluster destroyed"
}

show_status() {
    echo ""
    echo -e "${CYAN}=== Multi-CP Cluster Status ===${NC}"
    echo ""

    # Check bridge
    if ip link show "$BRIDGE_NAME" &>/dev/null; then
        echo -e "Bridge: ${GREEN}$BRIDGE_NAME (up)${NC}"
    else
        echo -e "Bridge: ${RED}not created${NC}"
    fi

    # Check DHCP server
    if [[ -f /run/talos-dnsmasq.pid ]] && kill -0 "$(cat /run/talos-dnsmasq.pid)" 2>/dev/null; then
        echo -e "DHCP:   ${GREEN}running${NC}"
    else
        echo -e "DHCP:   ${RED}not running${NC}"
    fi
    echo ""

    # Check each VM process
    echo "VMs (processes):"
    for node in cp1 cp2 cp3 w1 w2 w3; do
        local pid_file="$WORK_DIR/${node}.pid"
        if [[ -f "$pid_file" ]] && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
            echo -e "  $node: ${GREEN}running${NC}"
        else
            echo -e "  $node: ${RED}stopped${NC}"
        fi
    done
    echo ""

    # Show DHCP leases (maintenance mode IPs)
    echo "DHCP Leases (maintenance mode):"
    if [[ -f /run/talos-dnsmasq.leases ]]; then
        cat /run/talos-dnsmasq.leases 2>/dev/null | while read ts mac ip host _; do
            echo "  $ip ($mac) - $host"
        done
    else
        echo "  (no leases yet)"
    fi
    echo ""

    # Scan for maintenance mode nodes (use leases file if available for speed)
    echo "Scanning for Talos nodes in maintenance mode..."
    local found=0
    local ips_to_scan=()

    # First try to get IPs from leases file
    if [[ -f /run/talos-dnsmasq.leases ]] && [[ -s /run/talos-dnsmasq.leases ]]; then
        while read -r _ _ ip _; do
            ips_to_scan+=("$ip")
        done < /run/talos-dnsmasq.leases
    fi

    # Fallback to scanning if no leases
    if [[ ${#ips_to_scan[@]} -eq 0 ]]; then
        for i in $(seq 100 120); do
            ips_to_scan+=("${SUBNET}.$i")
        done
    fi

    for ip in "${ips_to_scan[@]}"; do
        # Check for maintenance mode error (means node is alive and in maintenance)
        if timeout 2 talosctl version --insecure --nodes "$ip" 2>&1 | grep -q "maintenance mode"; then
            echo -e "  ${GREEN}Found:${NC} $ip (maintenance mode)"
            found=$((found + 1))
        fi
    done
    if [[ $found -eq 0 ]]; then
        echo "  (no nodes found yet - still booting?)"
    fi
    echo ""

    # Check configured nodes (after apply)
    echo "Configured nodes (static IPs):"
    for node in cp1 cp2 cp3 w1 w2 w3; do
        local ip_var="${node^^}_IP"
        local ip="${!ip_var}"
        if talosctl version --insecure --nodes "$ip" &>/dev/null; then
            local mode=$(talosctl version --insecure --nodes "$ip" 2>&1 | grep -q "maintenance" && echo "maintenance" || echo "running")
            echo -e "  $node ($ip): ${GREEN}$mode${NC}"
        else
            echo -e "  $node ($ip): ${YELLOW}not reachable${NC}"
        fi
    done
    echo ""

    # Check talosconfig
    if talosctl config contexts 2>/dev/null | grep -q "$CLUSTER_NAME"; then
        echo "Talosconfig: $CLUSTER_NAME context exists"

        # Try to get etcd members
        echo ""
        echo "etcd members:"
        if talosctl --context "$CLUSTER_NAME" etcd members --nodes "$CP1_IP" 2>/dev/null; then
            :
        else
            echo "  (cannot connect - cluster may not be bootstrapped)"
        fi
    else
        echo "Talosconfig: not configured"
    fi
    echo ""
}

show_connect_info() {
    cat << EOF

=== Multi-CP Cluster Connection Info ===

Network: $BRIDGE_NAME (${SUBNET}.0/24)

Hostnames (/etc/hosts):
  $VIP_HOSTNAME -> $VIP (Virtual IP)
  $CP1_HOSTNAME -> $CP1_IP
  $CP2_HOSTNAME -> $CP2_IP
  $CP3_HOSTNAME -> $CP3_IP
  $W1_HOSTNAME -> $W1_IP
  $W2_HOSTNAME -> $W2_IP
  $W3_HOSTNAME -> $W3_IP

talosctl commands:
  talosctl --context $CLUSTER_NAME get members
  talosctl --context $CLUSTER_NAME etcd members
  talosctl --context $CLUSTER_NAME dashboard

talos-pilot:
  cargo run
  # Select '$CLUSTER_NAME' context
  # Check etcd view - should show 3/3 members

Test the fix:
  The talosconfig uses HOSTNAMES like the user's production config:
    endpoints: [$VIP_HOSTNAME, $CP1_HOSTNAME, $CP2_HOSTNAME, $CP3_HOSTNAME]
    nodes: [$CP1_HOSTNAME, $CP2_HOSTNAME, $CP3_HOSTNAME, $W1_HOSTNAME, $W2_HOSTNAME, $W3_HOSTNAME]

  This matches the user's real config:
    endpoints: [cluster.example.com, kubec01.example.com, kubec02.example.com, kubec03.example.com]
    nodes: [kubec01.example.com, kubec02.example.com, kubec03.example.com, kubew01.example.com, ...]

  etcd should show 3/3 healthy members (not 1/3 or 0/3)

EOF
}

check_nodes() {
    echo "Checking static IP nodes..."
    for node in cp1 cp2 cp3 w1 w2 w3; do
        local ip_var="${node^^}_IP"
        local ip="${!ip_var}"
        if timeout 1 bash -c "echo >/dev/tcp/$ip/50000" 2>/dev/null; then
            echo -e "  ${GREEN}✓${NC} $node ($ip)"
        else
            echo -e "  ${RED}✗${NC} $node ($ip)"
        fi
    done
}

restart_vms() {
    local boot_mode="${1:-cd}"  # cd or disk

    if [[ "$boot_mode" == "disk" ]]; then
        log_info "Restarting VMs to boot from disk..."
    else
        log_info "Restarting VMs to boot from CD (maintenance mode)..."
    fi

    for node in cp1 cp2 cp3 w1 w2 w3; do
        local pid_file="$WORK_DIR/${node}.pid"
        local disk_path="$WORK_DIR/${node}.qcow2"
        local tap_name="tap-${node}"
        local mac_file="$WORK_DIR/${node}.mac"

        # Get memory/cpu settings
        local memory cpus
        if [[ "$node" == cp* ]]; then
            memory="$CP_MEMORY"
            cpus="$CP_CPUS"
        else
            memory="$WORKER_MEMORY"
            cpus="$WORKER_CPUS"
        fi

        # Get MAC address
        local mac=""
        if [[ -f "$mac_file" ]]; then
            mac=$(cat "$mac_file")
        else
            mac=$(printf '52:54:00:%02x:%02x:%02x' $((RANDOM%256)) $((RANDOM%256)) $((RANDOM%256)))
        fi

        # Kill old VM
        if [[ -f "$pid_file" ]]; then
            local pid=$(cat "$pid_file")
            kill "$pid" 2>/dev/null || true
        fi
    done

    sleep 2

    for node in cp1 cp2 cp3 w1 w2 w3; do
        local pid_file="$WORK_DIR/${node}.pid"
        local disk_path="$WORK_DIR/${node}.qcow2"
        local tap_name="tap-${node}"
        local mac_file="$WORK_DIR/${node}.mac"

        local memory cpus
        if [[ "$node" == cp* ]]; then
            memory="$CP_MEMORY"
            cpus="$CP_CPUS"
        else
            memory="$WORKER_MEMORY"
            cpus="$WORKER_CPUS"
        fi

        local mac=$(cat "$mac_file" 2>/dev/null || printf '52:54:00:%02x:%02x:%02x' $((RANDOM%256)) $((RANDOM%256)) $((RANDOM%256)))

        log_info "Starting $node..."

        if [[ "$boot_mode" == "disk" ]]; then
            # Boot from disk only
            qemu-system-x86_64 \
                -name "$node" \
                -m "$memory" \
                -smp "$cpus" \
                -cpu host \
                -enable-kvm \
                -drive "file=$disk_path,format=qcow2,if=virtio" \
                -boot c \
                -netdev "tap,id=net0,ifname=$tap_name,script=no,downscript=no" \
                -device "virtio-net-pci,netdev=net0,mac=$mac" \
                -display none \
                -daemonize \
                -pidfile "$pid_file" \
                2>/dev/null
        else
            # Boot from CD (maintenance mode)
            qemu-system-x86_64 \
                -name "$node" \
                -m "$memory" \
                -smp "$cpus" \
                -cpu host \
                -enable-kvm \
                -drive "file=$disk_path,format=qcow2,if=virtio" \
                -cdrom "$ISO_PATH" \
                -boot d \
                -netdev "tap,id=net0,ifname=$tap_name,script=no,downscript=no" \
                -device "virtio-net-pci,netdev=net0,mac=$mac" \
                -display none \
                -daemonize \
                -pidfile "$pid_file" \
                2>/dev/null
        fi
    done

    if [[ "$boot_mode" == "disk" ]]; then
        log_success "VMs restarted (disk boot). Wait ~30s then: $0 check"
    else
        log_success "VMs restarted (CD boot). Wait ~30s then: $0 status"
    fi
}

# Main
case "${1:-help}" in
    create)
        create_cluster
        ;;
    destroy)
        destroy_cluster
        ;;
    status)
        show_status
        ;;
    check)
        check_nodes
        ;;
    reboot)
        restart_vms cd
        ;;
    reboot-disk)
        restart_vms disk
        ;;
    apply)
        apply_configs
        ;;
    bootstrap)
        bootstrap_cluster
        ;;
    config)
        generate_config
        setup_talosconfig
        ;;
    connect)
        show_connect_info
        ;;
    help|--help|-h)
        usage
        ;;
    *)
        log_error "Unknown command: $1"
        usage
        ;;
esac
