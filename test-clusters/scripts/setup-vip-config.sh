#!/bin/bash
# setup-vip-config.sh - Configure talosconfig with vIP test scenarios
#
# Run this AFTER creating a cluster with:
#   sudo -E $(which talosctl) cluster create --name test-cluster --cidr 10.5.0.0/24 --controlplanes 1 --workers 2
#
# This script sets up multiple talosconfig contexts to test the vIP filtering fix.
#
# The key scenario we're testing (user's real config):
#   endpoints: [vip, cp1, cp2, cp3]  <- vIP is only in endpoints
#   nodes: [cp1, cp2, cp3, w1, w2]   <- CP nodes in both, workers only in nodes
#
# The fix should:
#   - Keep CP nodes (they're in both endpoints AND nodes = real nodes)
#   - Filter vIP only if it accidentally appears in nodes (it's NOT a real node)

set -e

CLUSTER_NAME="test-cluster"
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
context: user-real-config
contexts:
  # =======================================================================
  # USER'S REAL CONFIG PATTERN (the main test case)
  # =======================================================================
  # This matches the user's production config:
  #   endpoints: [vip, cp1, cp2, cp3]  <- vIP + CP nodes
  #   nodes: [cp1, cp2, cp3, w1, w2]   <- CP + worker nodes (NO vIP)
  #
  # Expected behavior:
  #   - All 3 nodes should be targeted (CP and workers)
  #   - vIP is NOT in nodes, so nothing to filter
  #   - etcd should show 1/1 members
  #
  # NOTE: In test environment, $VIP_HOSTNAME (cluster.local) has TLS issues
  # because the cert doesn't include that name. Put working endpoint first.
  # In real production, the vIP would have a proper cert.
  user-real-config:
    endpoints:
      - $CP_IP
      - $VIP_HOSTNAME
    nodes:
      - $CP_IP
      - $WORKER1_IP
      - $WORKER2_IP
    ca: $CA
    crt: $CRT
    key: $KEY

  # =======================================================================
  # MISCONFIGURED: vIP accidentally in nodes list
  # =======================================================================
  # This is a misconfiguration where the vIP ended up in nodes.
  # With the new fix, we can't detect this - the vIP will be kept
  # because it appears in BOTH endpoints AND nodes.
  # (We assume if it's in nodes, the user wants to target it)
  vip-in-nodes-misconfigured:
    endpoints:
      - $VIP_HOSTNAME
    nodes:
      - $VIP_HOSTNAME
      - $CP_IP
      - $WORKER1_IP
      - $WORKER2_IP
    ca: $CA
    crt: $CRT
    key: $KEY

  # =======================================================================
  # BASELINE: Normal single endpoint, no nodes
  # =======================================================================
  normal:
    endpoints:
      - $CP_IP
    ca: $CA
    crt: $CRT
    key: $KEY

  # =======================================================================
  # ALL NODES AS ENDPOINTS (tests that real nodes aren't filtered)
  # =======================================================================
  # When all endpoints are also nodes, they should ALL be kept
  all-nodes-as-endpoints:
    endpoints:
      - $CP_IP
      - $WORKER1_IP
      - $WORKER2_IP
    nodes:
      - $CP_IP
      - $WORKER1_IP
      - $WORKER2_IP
    ca: $CA
    crt: $CRT
    key: $KEY

  # =======================================================================
  # ORIGINAL CLUSTER CONTEXT (preserved)
  # =======================================================================
  $CLUSTER_NAME:
    endpoints:
      - $CP_IP
    ca: $CA
    crt: $CRT
    key: $KEY
EOF

echo "  Created contexts:"
echo "    - user-real-config: User's real config pattern (vIP in endpoints only)"
echo "    - vip-in-nodes-misconfigured: vIP accidentally in nodes (misconfiguration)"
echo "    - normal: Single endpoint, no nodes (baseline)"
echo "    - all-nodes-as-endpoints: All nodes also as endpoints"
echo "    - $CLUSTER_NAME: Original cluster context"

echo ""
echo "=========================================="
echo "Setup Complete!"
echo "=========================================="
echo ""
echo "Current context: user-real-config"
echo ""
echo "Test Scenarios:"
echo ""
echo "  1. user-real-config (MAIN TEST - user's real config pattern)"
echo "     endpoints: [$CP_IP, $VIP_HOSTNAME]"
echo "     nodes: [$CP_IP, $WORKER1_IP, $WORKER2_IP]"
echo "     Expected: All 3 nodes targeted, etcd shows 1/1"
echo "     (Note: CP first due to TLS cert limitations in test env)"
echo ""
echo "  2. all-nodes-as-endpoints (verify real nodes aren't filtered)"
echo "     endpoints: [$CP_IP, $WORKER1_IP, $WORKER2_IP]"
echo "     nodes: [$CP_IP, $WORKER1_IP, $WORKER2_IP]"
echo "     Expected: All 3 nodes targeted"
echo ""
echo "Test commands:"
echo "  # Run talos-pilot"
echo "  cargo run --bin talos-pilot"
echo ""
echo "  # Check etcd members directly"
echo "  talosctl etcd members"
echo ""
echo "  # Switch contexts"
echo "  talosctl config context user-real-config"
echo "  talosctl config context all-nodes-as-endpoints"
echo "  talosctl config context normal"
echo ""
