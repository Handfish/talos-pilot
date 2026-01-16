#!/bin/bash
# Test cluster management script for QEMU-based Talos clusters
# Used for testing features that require physical disks (Storage view)
#
# This script uses direct QEMU instead of talosctl cluster create
# because talosctl has TLS issues in maintenance mode.

set -e

# Configuration
CLUSTER_NAME="talos-cluster"
WORK_DIR="/tmp/talos-qemu-test"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/talos-pilot"
DISK_SIZE="20G"
MEMORY="2048"
CPUS="2"
TALOS_VERSION="v1.12.1"
ISO_URL="https://factory.talos.dev/image/376567988ad370138ad8b2698212367b8edcb69b5fd68c80be1f2ec7d603b4ba/${TALOS_VERSION}/metal-amd64.iso"
ISO_PATH="$CACHE_DIR/talos-${TALOS_VERSION}.iso"

# Port mappings (host:guest)
TALOS_API_PORT="50000"
K8S_API_PORT="6443"

# Display mode (set by --gui flag)
GUI_MODE="false"

usage() {
    cat << EOF
Usage: $0 [--gui] <command>

Options:
  --gui       - Show QEMU graphical display (default: headless)

Commands:
  create      - Create VM and generate config (script-managed flow)
  create-bg   - Same as create, but runs in background
  wizard      - Create VM for talos-pilot wizard (no config generation)
  wizard-bg   - Same as wizard, but runs in background
  start       - Start VM from installed disk (after apply)
  destroy     - Destroy the cluster and clean up
  status      - Show cluster status
  apply       - Apply config to running VM in maintenance mode
  bootstrap   - Bootstrap the cluster (after apply)
  connect     - Show connection info

Workflows:
  Script-managed:  $0 create -> apply -> start -> bootstrap
  Wizard:          $0 wizard -> cargo run -- --insecure --endpoint 127.0.0.1

Prerequisites:
  - qemu-system-x86_64 installed
  - KVM enabled (/dev/kvm accessible)
  - Ports $TALOS_API_PORT and $K8S_API_PORT available

EOF
    exit 1
}

check_prereqs() {
    if ! command -v qemu-system-x86_64 &>/dev/null; then
        echo "Error: qemu-system-x86_64 not found. Install with: sudo apt install qemu-system-x86"
        exit 1
    fi

    if [[ ! -r /dev/kvm ]]; then
        echo "Error: /dev/kvm not accessible. Add yourself to kvm group: sudo usermod -aG kvm \$USER"
        exit 1
    fi

    if lsof -i :$TALOS_API_PORT &>/dev/null; then
        echo "Error: Port $TALOS_API_PORT is in use. Stop the process using it first."
        exit 1
    fi

    if lsof -i :$K8S_API_PORT &>/dev/null; then
        echo "Error: Port $K8S_API_PORT is in use. Stop Docker clusters or other services."
        exit 1
    fi
}

download_iso() {
    # Create cache directory if needed
    mkdir -p "$CACHE_DIR"

    if [[ -f "$ISO_PATH" ]]; then
        echo "ISO already cached: $ISO_PATH"
        return
    fi

    echo "Downloading Talos ISO (will be cached for future use)..."
    echo "  URL: $ISO_URL"
    echo "  Destination: $ISO_PATH"
    curl -L -o "$ISO_PATH" "$ISO_URL"
    echo "Downloaded and cached: $ISO_PATH"
}

create_disk() {
    local disk_path="$WORK_DIR/talos-disk.raw"

    if [[ -f "$disk_path" ]]; then
        echo "Disk already exists: $disk_path"
        echo "Run '$0 destroy' first to recreate."
        exit 1
    fi

    echo "Creating ${DISK_SIZE} disk image..."
    qemu-img create -f raw "$disk_path" "$DISK_SIZE"
}

generate_config() {
    echo "Generating Talos configuration..."
    talosctl gen config "$CLUSTER_NAME" "https://127.0.0.1:$K8S_API_PORT" \
        --additional-sans 127.0.0.1 \
        --output-dir "$WORK_DIR" \
        --force

    echo "Merging config into ~/.talos/config..."
    talosctl config merge "$WORK_DIR/talosconfig"
    talosctl --context "$CLUSTER_NAME" config endpoint 127.0.0.1
    talosctl --context "$CLUSTER_NAME" config node 127.0.0.1
}

start_vm() {
    local background="$1"

    echo "Starting QEMU VM..."
    echo "  Memory: ${MEMORY}MB"
    echo "  CPUs: $CPUS"
    echo "  Disk: $WORK_DIR/talos-disk.raw"
    echo "  Display: $(if [[ "$GUI_MODE" == "true" ]]; then echo "GUI"; else echo "headless"; fi)"
    echo ""
    echo "Port mappings:"
    echo "  localhost:$TALOS_API_PORT -> Talos API"
    echo "  localhost:$K8S_API_PORT -> Kubernetes API"
    echo ""

    local qemu_cmd=(
        qemu-system-x86_64
        -m "$MEMORY"
        -smp "$CPUS"
        -cpu host
        -enable-kvm
        -drive "file=$WORK_DIR/talos-disk.raw,format=raw,if=ide"
        -cdrom "$ISO_PATH"
        -boot d
        -netdev "user,id=net0,hostfwd=tcp::${TALOS_API_PORT}-:50000,hostfwd=tcp::${K8S_API_PORT}-:6443"
        -device virtio-net-pci,netdev=net0
    )

    if [[ "$background" == "true" ]]; then
        echo "Starting in background..."
        if [[ "$GUI_MODE" == "true" ]]; then
            "${qemu_cmd[@]}" -daemonize -pidfile "$WORK_DIR/qemu.pid"
        else
            "${qemu_cmd[@]}" -display none -daemonize -pidfile "$WORK_DIR/qemu.pid"
        fi
        echo "VM started. PID file: $WORK_DIR/qemu.pid"
        echo ""
        echo "Next steps:"
        echo "  1. Wait for maintenance mode (check with: $0 status)"
        echo "  2. Apply config: $0 apply"
        echo "  3. Wait for install to complete"
        echo "  4. Bootstrap: $0 bootstrap"
    else
        echo "Starting in foreground (Ctrl+C to stop)..."
        echo ""
        if [[ "$GUI_MODE" == "true" ]]; then
            "${qemu_cmd[@]}"
        else
            "${qemu_cmd[@]}" -display none
        fi
    fi
}

create_cluster() {
    local background="${1:-false}"
    local skip_config="${2:-false}"

    check_prereqs

    mkdir -p "$WORK_DIR"

    download_iso
    create_disk

    if [[ "$skip_config" == "false" ]]; then
        generate_config

        echo ""
        echo "========================================="
        echo "VM will boot into maintenance mode."
        echo ""
        echo "After boot, run in another terminal:"
        echo "  $0 apply      # Apply configuration"
        echo "  $0 bootstrap  # Bootstrap cluster"
        echo "========================================="
        echo ""
    else
        echo ""
        echo "========================================="
        echo "VM will boot into maintenance mode."
        echo ""
        echo "Use talos-pilot wizard to configure:"
        echo "  cargo run -- --insecure --endpoint 127.0.0.1"
        echo "========================================="
        echo ""
    fi

    start_vm "$background"
}

apply_config() {
    if [[ ! -f "$WORK_DIR/controlplane.yaml" ]]; then
        echo "Error: Config not found. Run '$0 create' first."
        exit 1
    fi

    echo "Applying configuration to VM..."
    if talosctl apply-config --insecure --nodes 127.0.0.1 --file "$WORK_DIR/controlplane.yaml"; then
        echo ""
        echo "Config applied! The VM will install Talos and shut down."
        echo ""
        echo "Next steps:"
        echo "  1. Run: $0 start     # Boot from installed disk"
        echo "  2. Run: $0 bootstrap # Bootstrap the cluster"
    else
        echo "Failed to apply config. Is the VM in maintenance mode?"
    fi
}

start_from_disk() {
    if [[ ! -f "$WORK_DIR/talos-disk.raw" ]]; then
        echo "Error: Disk not found. Run '$0 create' first."
        exit 1
    fi

    check_prereqs

    echo "Starting QEMU VM from installed disk..."
    echo "  Memory: ${MEMORY}MB"
    echo "  CPUs: $CPUS"
    echo "  Disk: $WORK_DIR/talos-disk.raw"
    echo "  Display: $(if [[ "$GUI_MODE" == "true" ]]; then echo "GUI"; else echo "headless"; fi)"
    echo ""
    echo "Port mappings:"
    echo "  localhost:$TALOS_API_PORT -> Talos API"
    echo "  localhost:$K8S_API_PORT -> Kubernetes API"
    echo ""

    local qemu_cmd=(
        qemu-system-x86_64
        -m "$MEMORY"
        -smp "$CPUS"
        -cpu host
        -enable-kvm
        -drive "file=$WORK_DIR/talos-disk.raw,format=raw,if=ide"
        -netdev "user,id=net0,hostfwd=tcp::${TALOS_API_PORT}-:50000,hostfwd=tcp::${K8S_API_PORT}-:6443"
        -device virtio-net-pci,netdev=net0
    )

    if [[ "$GUI_MODE" == "true" ]]; then
        "${qemu_cmd[@]}" -daemonize -pidfile "$WORK_DIR/qemu.pid"
    else
        "${qemu_cmd[@]}" -display none -daemonize -pidfile "$WORK_DIR/qemu.pid"
    fi

    echo "VM started in background. PID file: $WORK_DIR/qemu.pid"
    echo ""
    echo "Wait for boot, then run: $0 bootstrap"
    echo "Check status with: $0 status"
}

bootstrap_cluster() {
    echo "Checking connection..."
    if ! talosctl --context "$CLUSTER_NAME" version &>/dev/null; then
        echo "Error: Cannot connect to VM. Is it running and configured?"
        exit 1
    fi

    echo "Bootstrapping cluster..."
    talosctl --context "$CLUSTER_NAME" bootstrap

    echo ""
    echo "Bootstrap initiated!"
    echo ""
    echo "Check cluster status with:"
    echo "  talosctl --context $CLUSTER_NAME get disks"
    echo "  talosctl --context $CLUSTER_NAME get members"
    echo ""
    echo "Test in talos-pilot:"
    echo "  cargo run"
    echo "  # Switch to '$CLUSTER_NAME' context, select node, press 's'"
}

destroy_cluster() {
    echo "Destroying QEMU cluster..."

    # Kill QEMU process
    if [[ -f "$WORK_DIR/qemu.pid" ]]; then
        local pid=$(cat "$WORK_DIR/qemu.pid")
        if kill -0 "$pid" 2>/dev/null; then
            echo "Killing QEMU process (PID: $pid)..."
            kill "$pid" 2>/dev/null || true
        fi
    fi

    # Also try pkill as backup
    pkill -f "qemu.*talos-disk.raw" 2>/dev/null || true

    # Remove work directory
    if [[ -d "$WORK_DIR" ]]; then
        echo "Removing $WORK_DIR..."
        rm -rf "$WORK_DIR"
    fi

    # Remove talosconfig contexts (including numbered variants like talos-cluster-1, talos-cluster-2, etc.)
    echo "Removing talosconfig contexts matching '$CLUSTER_NAME*'..."

    # Get all context names (skip header, get NAME column which is $2, handle * in CURRENT column)
    local all_contexts
    all_contexts=$(talosctl config contexts 2>/dev/null | tail -n +2 | awk '{if ($1 == "*") print $2; else print $1}' || true)

    # Filter to just talos-cluster* contexts
    local contexts
    contexts=$(echo "$all_contexts" | grep "^${CLUSTER_NAME}" || true)

    if [[ -n "$contexts" ]]; then
        # Get current context
        local current_ctx
        current_ctx=$(talosctl config contexts 2>/dev/null | grep '^\*' | awk '{print $2}' || true)
        echo "  Current context: $current_ctx"

        # Check if current context is one we want to delete
        if echo "$current_ctx" | grep -q "^${CLUSTER_NAME}"; then
            # Get first non-matching context to switch to
            local other_ctx
            other_ctx=$(echo "$all_contexts" | grep -v "^${CLUSTER_NAME}" | head -1 || true)

            if [[ -n "$other_ctx" ]]; then
                echo "  Switching from '$current_ctx' to '$other_ctx'..."
                talosctl config context "$other_ctx" 2>/dev/null || true
            else
                # No other context - clear current context in config file directly
                echo "  No other context available, clearing current context..."
                local config_file="${TALOSCONFIG:-$HOME/.talos/config}"
                if [[ -f "$config_file" ]]; then
                    # Use sed to remove or clear the 'context:' line
                    sed -i 's/^context: .*/context: ""/' "$config_file" 2>/dev/null || true
                fi
            fi
        fi

        # Now remove all matching contexts
        for ctx in $contexts; do
            echo "  Removing context: $ctx"
            talosctl config remove "$ctx" --noconfirm 2>/dev/null || true
        done
    fi

    echo "Done."
}

show_status() {
    echo "QEMU Cluster Status"
    echo "==================="
    echo ""

    # Check if VM is running
    if pgrep -f "qemu.*talos-disk.raw" &>/dev/null; then
        echo "VM: Running"
    else
        echo "VM: Not running"
    fi

    # Check files
    echo ""
    echo "Files:"
    [[ -f "$WORK_DIR/talos-disk.raw" ]] && echo "  Disk: $WORK_DIR/talos-disk.raw" || echo "  Disk: Not created"
    [[ -f "$ISO_PATH" ]] && echo "  ISO: $ISO_PATH (cached)" || echo "  ISO: Not downloaded"
    [[ -f "$WORK_DIR/controlplane.yaml" ]] && echo "  Config: $WORK_DIR/controlplane.yaml" || echo "  Config: Not generated"

    # Check context
    echo ""
    echo "Talosconfig context:"
    if talosctl config contexts 2>/dev/null | grep -q "$CLUSTER_NAME"; then
        talosctl config contexts | grep "$CLUSTER_NAME"
    else
        echo "  Not configured"
    fi

    # Try to connect
    echo ""
    echo "Connection test:"
    if talosctl --context "$CLUSTER_NAME" version 2>/dev/null; then
        echo ""
        echo "Disks:"
        talosctl --context "$CLUSTER_NAME" get disks 2>/dev/null || echo "  Cannot get disks"
    else
        # Try maintenance mode
        if talosctl version --insecure --nodes 127.0.0.1 2>&1 | grep -q "maintenance"; then
            echo "  VM is in maintenance mode. Run: $0 apply"
        else
            echo "  Cannot connect (VM not running or not ready)"
        fi
    fi
}

show_connect_info() {
    cat << EOF
QEMU Cluster Connection Info
============================

Talos API:      127.0.0.1:$TALOS_API_PORT
Kubernetes API: 127.0.0.1:$K8S_API_PORT

talosctl commands:
  talosctl --context $CLUSTER_NAME version
  talosctl --context $CLUSTER_NAME get disks
  talosctl --context $CLUSTER_NAME dashboard

kubectl (after bootstrap):
  talosctl --context $CLUSTER_NAME kubeconfig
  kubectl --context admin@$CLUSTER_NAME get nodes

talos-pilot:
  cargo run
  # Switch to '$CLUSTER_NAME' context, select node, press 's' for Storage view

EOF
}

# Parse flags
while [[ $# -gt 0 ]]; do
    case "$1" in
        --gui)
            GUI_MODE="true"
            shift
            ;;
        *)
            break
            ;;
    esac
done

# Main
case "${1:-}" in
    create)
        create_cluster false false
        ;;
    create-bg)
        create_cluster true false
        ;;
    wizard)
        create_cluster false true
        ;;
    wizard-bg)
        create_cluster true true
        ;;
    start)
        start_from_disk
        ;;
    destroy)
        destroy_cluster
        ;;
    status)
        show_status
        ;;
    apply)
        apply_config
        ;;
    bootstrap)
        bootstrap_cluster
        ;;
    connect)
        show_connect_info
        ;;
    *)
        usage
        ;;
esac
