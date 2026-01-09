#!/usr/bin/env bash
# Fetch Talos proto files from the official repository
#
# Usage: ./scripts/fetch-protos.sh [VERSION]
#   VERSION defaults to v1.12.1

set -euo pipefail

VERSION="${1:-v1.12.1}"
PROTO_DIR="crates/talos-rs/proto"
BASE_URL="https://raw.githubusercontent.com/siderolabs/talos/${VERSION}/api"

echo "Fetching Talos proto files (${VERSION})..."

# Create directories
mkdir -p "${PROTO_DIR}"/{common,machine,storage,time,inspect,google/rpc}

# Fetch proto files
fetch_proto() {
    local path="$1"
    local url="${BASE_URL}/${path}"
    local dest="${PROTO_DIR}/${path}"

    echo "  Downloading ${path}..."
    if curl -sfL "${url}" -o "${dest}"; then
        echo "    ✓ ${path}"
    else
        echo "    ✗ Failed to download ${path}" >&2
        return 1
    fi
}

fetch_proto "common/common.proto"
fetch_proto "machine/machine.proto"
fetch_proto "storage/storage.proto"
fetch_proto "time/time.proto"
fetch_proto "inspect/inspect.proto"

# Fetch Google RPC protos (required dependency)
echo "  Downloading google/rpc/status.proto..."
curl -sfL "https://raw.githubusercontent.com/googleapis/googleapis/master/google/rpc/status.proto" \
    -o "${PROTO_DIR}/google/rpc/status.proto" && echo "    ✓ google/rpc/status.proto"

echo ""
echo "Proto files updated to ${VERSION}"
echo "Don't forget to rebuild: cargo build -p talos-rs"
