#!/bin/bash
# vip-test-setup.sh - Clean setup with vIP endpoint test scenario
#
# This script:
# 1. Cleans up all existing Talos Docker clusters
# 2. Removes ~/.talos/config
# 3. Creates a fresh 3-node cluster (1 CP + 2 workers)
# 4. Sets up talosconfig with vIP scenario to test the etcd fix
#
# The vIP scenario: endpoint is a hostname that also appears in nodes list
# This reproduces the bug where vIP gets passed to node targeting header

set -e

CLUSTER_NAME="test-cluster"
CLUSTER_NETWORK="10.5.0.0/24"
CP_IP="10.5.0.2"
WORKER1_IP="10.5.0.3"
WORKER2_IP="10.5.0.4"
VIP_HOSTNAME="cluster.local"

echo "=========================================="
echo "Talos vIP Test Scenario Setup"
echo "=========================================="
echo ""

# Step 1: Clean up existing Talos clusters
echo "[1/6] Cleaning up existing Talos Docker clusters..."

# Find containers running the Talos image
TALOS_CONTAINERS=$(docker ps -a --filter "ancestor=ghcr.io/siderolabs/talos:v1.12.1" --format "{{.Names}}" 2>/dev/null || true)
# Also find by common naming patterns (cluster-*, talos-*)
CLUSTER_CONTAINERS=$(docker ps -a --format "{{.Names}}" 2>/dev/null | grep -E "^(cluster-|talos-)" || true)
ALL_CONTAINERS=$(echo -e "$TALOS_CONTAINERS\n$CLUSTER_CONTAINERS" | sort -u | grep -v '^$' || true)

if [ -n "$ALL_CONTAINERS" ]; then
    echo "  Stopping and removing containers:"
    for container in $ALL_CONTAINERS; do
        echo "    - $container"
        docker rm -f "$container" 2>/dev/null || true
    done
else
    echo "  No Talos containers found"
fi

# Remove talos/cluster networks
TALOS_NETWORKS=$(docker network ls --format "{{.Name}}" 2>/dev/null | grep -E "^(talos|cluster-)" || true)
if [ -n "$TALOS_NETWORKS" ]; then
    echo "  Removing networks:"
    for network in $TALOS_NETWORKS; do
        echo "    - $network"
        docker network rm "$network" 2>/dev/null || true
    done
else
    echo "  No Talos networks found"
fi

# Step 2: Clean up ~/.talos/config
echo ""
echo "[2/6] Cleaning up ~/.talos/config..."
if [ -f ~/.talos/config ]; then
    rm -f ~/.talos/config
    echo "  Removed ~/.talos/config"
else
    echo "  No existing config found"
fi

# Step 3: Create fresh cluster
echo ""
echo "[3/6] Creating fresh Talos cluster: $CLUSTER_NAME"
echo "  Network: $CLUSTER_NETWORK"
echo "  Control Plane: $CP_IP"
echo "  Workers: $WORKER1_IP, $WORKER2_IP"
echo ""

sudo -E talosctl cluster create \
    --name "$CLUSTER_NAME" \
    --cidr "$CLUSTER_NETWORK" \
    --controlplanes 1 \
    --workers 2 \
    --wait-timeout 10m

echo ""
echo "  Cluster created successfully!"

# Step 4: Get node hostnames
echo ""
echo "[4/6] Getting node information..."
CP_HOSTNAME=$(talosctl get hostname -n "$CP_IP" -o json 2>/dev/null | jq -r '.spec.hostname' || echo "talos-cp-1")
WORKER1_HOSTNAME=$(talosctl get hostname -n "$WORKER1_IP" -o json 2>/dev/null | jq -r '.spec.hostname' || echo "talos-worker-1")
WORKER2_HOSTNAME=$(talosctl get hostname -n "$WORKER2_IP" -o json 2>/dev/null | jq -r '.spec.hostname' || echo "talos-worker-2")

echo "  Control Plane: $CP_HOSTNAME ($CP_IP)"
echo "  Worker 1: $WORKER1_HOSTNAME ($WORKER1_IP)"
echo "  Worker 2: $WORKER2_HOSTNAME ($WORKER2_IP)"

# Step 5: Add /etc/hosts entry for vIP hostname
echo ""
echo "[5/6] Setting up vIP hostname in /etc/hosts..."
# Remove old entry if exists
sudo sed -i "/$VIP_HOSTNAME/d" /etc/hosts 2>/dev/null || true
# Add new entry pointing vIP hostname to control plane
echo "$CP_IP $VIP_HOSTNAME" | sudo tee -a /etc/hosts > /dev/null
echo "  Added: $CP_IP $VIP_HOSTNAME"

# Step 6: Create talosconfig with vIP test scenarios
echo ""
echo "[6/6] Creating talosconfig with test contexts..."

# Backup original config created by talosctl cluster create
ORIGINAL_CONFIG=~/.talos/config

# Extract CA, CRT, and KEY from the original config using grep/awk
# The config format has these on their own lines after the context
CA=$(grep -A 20 "contexts:" "$ORIGINAL_CONFIG" | grep "ca:" | head -1 | awk '{print $2}')
CRT=$(grep -A 20 "contexts:" "$ORIGINAL_CONFIG" | grep "crt:" | head -1 | awk '{print $2}')
KEY=$(grep -A 20 "contexts:" "$ORIGINAL_CONFIG" | grep "key:" | head -1 | awk '{print $2}')

# Create new config with multiple test contexts
cat > ~/.talos/config << EOF
context: vip-with-nodes
contexts:
  # Context 1: Normal setup (single endpoint, no nodes specified)
  # Expected: Works normally, targets endpoint node only
  normal:
    endpoints:
      - $CP_IP
    ca: $CA
    crt: $CRT
    key: $KEY

  # Context 2: vIP endpoint with nodes list including the vIP
  # This is the BUG SCENARIO we fixed:
  # - endpoint is $VIP_HOSTNAME (resolves to $CP_IP)
  # - nodes includes $VIP_HOSTNAME AND actual node hostnames
  # Before fix: vIP would be passed in node header -> empty etcd results
  # After fix: vIP should be filtered out -> correct etcd results
  vip-with-nodes:
    endpoints:
      - $VIP_HOSTNAME
    nodes:
      - $VIP_HOSTNAME
      - $CP_HOSTNAME
      - $WORKER1_HOSTNAME
      - $WORKER2_HOSTNAME
    ca: $CA
    crt: $CRT
    key: $KEY

  # Context 3: IP endpoint with nodes list including the IP
  # Similar bug scenario but with IP instead of hostname
  ip-with-nodes:
    endpoints:
      - $CP_IP
    nodes:
      - $CP_IP
      - $WORKER1_IP
      - $WORKER2_IP
    ca: $CA
    crt: $CRT
    key: $KEY

  # Context 4: Multiple endpoints (all real nodes)
  # Tests that we don't break normal multi-endpoint configs
  multi-endpoint:
    endpoints:
      - $CP_IP
      - $WORKER1_IP
      - $WORKER2_IP
    ca: $CA
    crt: $CRT
    key: $KEY
EOF

echo "  Created contexts:"
echo "    - normal: Single endpoint, no nodes (baseline)"
echo "    - vip-with-nodes: vIP hostname in both endpoints and nodes (BUG SCENARIO)"
echo "    - ip-with-nodes: IP in both endpoints and nodes (similar bug)"
echo "    - multi-endpoint: Multiple real endpoints (should work normally)"

echo ""
echo "=========================================="
echo "Setup Complete!"
echo "=========================================="
echo ""
echo "Current context: vip-with-nodes (the bug scenario)"
echo ""
echo "Test commands:"
echo "  # Run talos-pilot with the vIP bug scenario"
echo "  cargo run --bin talos-pilot"
echo ""
echo "  # Check etcd members directly (should show 1/1 if fix works)"
echo "  talosctl etcd members"
echo ""
echo "  # Switch to normal context for comparison"
echo "  talosctl config context normal"
echo ""
echo "  # List all contexts"
echo "  talosctl config contexts"
echo ""
echo "What to verify:"
echo "  1. In 'vip-with-nodes' context, etcd should show 1/1 members (not 0/0)"
echo "  2. In 'ip-with-nodes' context, etcd should show 1/1 members (not 0/0)"
echo "  3. Cluster view should show all 3 nodes correctly"
echo ""
