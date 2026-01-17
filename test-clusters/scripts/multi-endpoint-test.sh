#!/usr/bin/env bash
#
# Multi-Endpoint Test Script
#
# Creates a talosconfig context with multiple endpoints pointing to the same cluster
# to reproduce the duplicate node issue.
#
# Problem: When multiple endpoints are configured, queries through different endpoints
# return the same nodes, leading to duplicate entries in the cluster view.
#
# Example user config causing the issue:
# ```yaml
# context: prod
# contexts:
#     prod:
#         endpoints:
#             - cluster.example.com  # vIP
#             - kubec01.example.com
#             - kubec02.example.com
#             - kubec03.example.com
#         nodes:
#             - kubec01.example.com
#             - kubec02.example.com
#             - kubec03.example.com
#             - kubew01.example.com
#             - kubew02.example.com
# ```
#
# Usage:
#   ./multi-endpoint-test.sh setup     Create test context with multiple endpoints
#   ./multi-endpoint-test.sh cleanup   Remove test context
#   ./multi-endpoint-test.sh status    Show current talosconfig contexts
#   ./multi-endpoint-test.sh help      Show help
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_CONTEXT="multi-endpoint-test"

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

show_help() {
    cat << 'EOF'
Multi-Endpoint Test Script

Reproduces the duplicate node issue that occurs when a talosconfig has multiple
endpoints pointing to the same cluster.

USAGE:
    ./multi-endpoint-test.sh <command>

COMMANDS:
    setup       Create a test context with multiple endpoints using cluster-alpha
    cleanup     Remove the test context
    status      Show current talosconfig contexts
    help        Show this help

WHAT THIS TESTS:

The duplicate node bug occurs when:
1. A talosconfig context has multiple endpoints (e.g., VIP + individual node IPs)
2. talos-pilot queries through each endpoint
3. Each endpoint returns the same nodes
4. Without deduplication, nodes appear multiple times in the UI

After running 'setup', you'll have a 'multi-endpoint-test' context with:
- 3 endpoints (control plane + 2 workers from cluster-alpha)
- 3 nodes (the same 3 machines)

This mimics a production setup where users configure:
- VIP endpoint (for HA)
- Individual control plane endpoints (for direct access)

Run talos-pilot and switch to this context to verify the deduplication fix.

PREREQUISITES:
    - cluster-alpha must be running (use multi-cluster-test.sh create first)

EOF
}

check_prerequisites() {
    # Check if cluster-alpha exists
    if ! docker ps --format '{{.Names}}' | grep -q "^cluster-alpha-"; then
        log_error "cluster-alpha is not running"
        echo "Run './multi-cluster-test.sh create' first to create test clusters"
        exit 1
    fi

    # Check talosctl is available
    if ! command -v talosctl &>/dev/null; then
        log_error "talosctl is not installed"
        exit 1
    fi
}

setup_multi_endpoint_context() {
    check_prerequisites

    log_info "Creating multi-endpoint test context..."

    # The main talosconfig is at ~/.talos/config
    local talosconfig="${HOME}/.talos/config"
    if [[ ! -f "${talosconfig}" ]]; then
        log_error "talosconfig not found at ${talosconfig}"
        exit 1
    fi

    # The cluster-alpha nodes are (internal Docker network IPs):
    # - 10.5.0.2 (controlplane)
    # - 10.5.0.3 (worker-1)
    # - 10.5.0.4 (worker-2)

    # First, remove old context if it exists
    talosctl config remove "${TEST_CONTEXT}" -y 2>/dev/null || true

    # Create output directory
    mkdir -p "${SCRIPT_DIR}/../output"

    # Use Python to properly extract and create the config (more reliable YAML handling)
    log_info "Creating ${TEST_CONTEXT} context with multiple endpoints..."

    python3 << 'PYEOF'
import yaml
import os

talosconfig_path = os.path.expanduser("~/.talos/config")
output_path = os.path.join(os.path.dirname(os.path.abspath(".")), "test-clusters/output/multi-endpoint-temp.yaml")

with open(talosconfig_path, 'r') as f:
    config = yaml.safe_load(f)

# Get cluster-alpha credentials
alpha_ctx = config['contexts']['cluster-alpha']

# Create new context with multiple endpoints
new_config = {
    'context': 'multi-endpoint-test',
    'contexts': {
        'multi-endpoint-test': {
            'endpoints': ['10.5.0.2', '10.5.0.3', '10.5.0.4'],
            'nodes': ['10.5.0.2', '10.5.0.3', '10.5.0.4'],
            'ca': alpha_ctx['ca'],
            'crt': alpha_ctx['crt'],
            'key': alpha_ctx['key']
        }
    }
}

# Write to temp file
output_file = os.path.expanduser("~/.talos/multi-endpoint-temp.yaml")
with open(output_file, 'w') as f:
    yaml.dump(new_config, f, default_flow_style=False)

print(f"Created temp config at {output_file}")
PYEOF

    local temp_config="${HOME}/.talos/multi-endpoint-temp.yaml"

    if [[ ! -f "${temp_config}" ]]; then
        log_error "Failed to create temp config"
        exit 1
    fi

    # Merge this config
    log_info "Merging ${TEST_CONTEXT} context..."
    talosctl config merge "${temp_config}"

    # Switch to the new context
    talosctl config context "${TEST_CONTEXT}"

    # Clean up temp file
    rm -f "${temp_config}"

    echo ""
    log_success "Multi-endpoint test context created!"
    echo ""
    echo -e "${CYAN}=== Context Configuration ===${NC}"
    echo ""
    echo "Context: ${TEST_CONTEXT}"
    echo ""
    echo "Endpoints (3 - simulates VIP + individual nodes):"
    echo "  - 10.5.0.2 (controlplane)"
    echo "  - 10.5.0.3 (worker-1)"
    echo "  - 10.5.0.4 (worker-2)"
    echo ""
    echo "Nodes (3 - same machines as endpoints):"
    echo "  - 10.5.0.2"
    echo "  - 10.5.0.3"
    echo "  - 10.5.0.4"
    echo ""
    echo -e "${CYAN}=== Testing ===${NC}"
    echo ""
    echo "1. Verify the context is active:"
    echo "   talosctl config contexts"
    echo ""
    echo "2. Run talos-pilot:"
    echo "   cargo run --bin talos-pilot"
    echo ""
    echo "3. Expected behavior (with deduplication fix):"
    echo "   - Should see 3 nodes total (not 9 or more)"
    echo "   - Each node appears exactly once"
    echo ""
    echo "4. Bug behavior (without deduplication):"
    echo "   - Nodes would appear 3x each (once per endpoint)"
    echo "   - Total of 9 node entries for 3 actual nodes"
    echo ""
}

cleanup_context() {
    log_info "Removing ${TEST_CONTEXT} context..."

    if talosctl config remove "${TEST_CONTEXT}" -y 2>/dev/null; then
        log_success "Context ${TEST_CONTEXT} removed"
    else
        log_warn "Context ${TEST_CONTEXT} was not found"
    fi

    # Switch back to cluster-alpha if available
    if talosctl config contexts 2>/dev/null | grep -q "cluster-alpha"; then
        talosctl config context cluster-alpha
        log_info "Switched back to cluster-alpha context"
    fi
}

show_status() {
    echo ""
    echo -e "${CYAN}=== Talos Contexts ===${NC}"
    echo ""
    talosctl config contexts
    echo ""

    # Check if our test context exists
    if talosctl config contexts 2>/dev/null | grep -q "${TEST_CONTEXT}"; then
        echo -e "${GREEN}Test context '${TEST_CONTEXT}' is configured${NC}"
        echo ""
        echo "To test:"
        echo "  1. talosctl config context ${TEST_CONTEXT}"
        echo "  2. cargo run --bin talos-pilot"
    else
        echo -e "${YELLOW}Test context '${TEST_CONTEXT}' is not configured${NC}"
        echo ""
        echo "Run './multi-endpoint-test.sh setup' to create it"
    fi
}

main() {
    local command="${1:-help}"

    case "${command}" in
        setup)
            setup_multi_endpoint_context
            ;;
        cleanup)
            cleanup_context
            ;;
        status)
            show_status
            ;;
        help|--help|-h)
            show_help
            ;;
        *)
            log_error "Unknown command: ${command}"
            echo "Run './multi-endpoint-test.sh help' for usage"
            exit 1
            ;;
    esac
}

main "$@"
