#!/bin/bash
# setup-vip-config.sh - Configure talosconfig with vIP test scenarios
#
# Run this AFTER creating a cluster with:
#   sudo -E talosctl cluster create --name test-cluster --cidr 10.5.0.0/24 --controlplanes 1 --workers 2
#
# This script sets up multiple talosconfig contexts to test the vIP filtering fix.

set -e

CLUSTER_NAME="talos-pilot"
CP_IP="10.5.0.2"
WORKER1_IP="10.5.0.3"
WORKER2_IP="10.5.0.4"
VIP_HOSTNAME="cluster.local"

echo "=========================================="
echo "Setting up vIP Test Contexts"
echo "=========================================="
echo ""

# Check if talosconfig exists
if [ ! -f ~/.talos/config ]; then
    echo "ERROR: ~/.talos/config not found!"
    echo ""
    echo "Please create a cluster first:"
    echo "  sudo -E talosctl cluster create --name test-cluster --cidr 10.5.0.0/24 --controlplanes 1 --workers 2"
    exit 1
fi

# Verify we can connect to the cluster
echo "[1/4] Verifying cluster connectivity..."
if ! talosctl version -n "$CP_IP" > /dev/null 2>&1; then
    echo "ERROR: Cannot connect to cluster at $CP_IP"
    echo "Make sure the cluster is running."
    exit 1
fi
echo "  Connected to cluster"

# Get node hostnames
echo ""
echo "[2/4] Getting node information..."
CP_HOSTNAME=$(talosctl get hostname -n "$CP_IP" -o json 2>/dev/null | grep -o '"hostname":"[^"]*"' | cut -d'"' -f4 || echo "")
WORKER1_HOSTNAME=$(talosctl get hostname -n "$WORKER1_IP" -o json 2>/dev/null | grep -o '"hostname":"[^"]*"' | cut -d'"' -f4 || echo "")
WORKER2_HOSTNAME=$(talosctl get hostname -n "$WORKER2_IP" -o json 2>/dev/null | grep -o '"hostname":"[^"]*"' | cut -d'"' -f4 || echo "")

# Fallback to IPs if hostnames not available
CP_HOSTNAME=${CP_HOSTNAME:-$CP_IP}
WORKER1_HOSTNAME=${WORKER1_HOSTNAME:-$WORKER1_IP}
WORKER2_HOSTNAME=${WORKER2_HOSTNAME:-$WORKER2_IP}

echo "  Control Plane: $CP_HOSTNAME ($CP_IP)"
echo "  Worker 1: $WORKER1_HOSTNAME ($WORKER1_IP)"
echo "  Worker 2: $WORKER2_HOSTNAME ($WORKER2_IP)"

# Add /etc/hosts entry for vIP hostname
echo ""
echo "[3/4] Setting up vIP hostname in /etc/hosts..."
if grep -q "$VIP_HOSTNAME" /etc/hosts 2>/dev/null; then
    echo "  Entry for $VIP_HOSTNAME already exists, updating..."
    sudo sed -i "/$VIP_HOSTNAME/d" /etc/hosts 2>/dev/null || true
fi
echo "$CP_IP $VIP_HOSTNAME" | sudo tee -a /etc/hosts > /dev/null
echo "  Added: $CP_IP $VIP_HOSTNAME"

# Extract credentials from existing config
echo ""
echo "[4/4] Creating talosconfig with test contexts..."

ORIGINAL_CONFIG=~/.talos/config
CA=$(grep "ca:" "$ORIGINAL_CONFIG" | head -1 | awk '{print $2}')
CRT=$(grep "crt:" "$ORIGINAL_CONFIG" | head -1 | awk '{print $2}')
KEY=$(grep "key:" "$ORIGINAL_CONFIG" | head -1 | awk '{print $2}')

if [ -z "$CA" ] || [ -z "$CRT" ] || [ -z "$KEY" ]; then
    echo "ERROR: Could not extract credentials from talosconfig"
    exit 1
fi

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

  # Context 5: Original cluster context (preserved)
  $CLUSTER_NAME:
    endpoints:
      - $CP_IP
    ca: $CA
    crt: $CRT
    key: $KEY
EOF

echo "  Created contexts:"
echo "    - normal: Single endpoint, no nodes (baseline)"
echo "    - vip-with-nodes: vIP hostname in both endpoints and nodes (BUG SCENARIO)"
echo "    - ip-with-nodes: IP in both endpoints and nodes (similar bug)"
echo "    - multi-endpoint: Multiple real endpoints"
echo "    - $CLUSTER_NAME: Original cluster context"

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
echo "  # Check etcd members directly"
echo "  talosctl etcd members"
echo ""
echo "  # Switch contexts"
echo "  talosctl config context normal"
echo "  talosctl config context vip-with-nodes"
echo "  talosctl config context ip-with-nodes"
echo ""
echo "What to verify:"
echo "  1. In 'vip-with-nodes' context: etcd should show 1/1 (not 0/0)"
echo "  2. In 'ip-with-nodes' context: etcd should show 1/1 (not 0/0)"
echo "  3. Cluster view should show all 3 nodes correctly"
echo ""
