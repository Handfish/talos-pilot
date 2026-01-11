#!/usr/bin/env bash
#
# Talos Test Cluster Management Script
#
# Creates Docker-based Talos clusters with various CNI and networking
# configurations for testing talos-pilot features.
#
# Usage:
#   ./cluster.sh create <profile>    Create cluster with specified profile
#   ./cluster.sh destroy             Destroy the test cluster
#   ./cluster.sh status              Show cluster status
#   ./cluster.sh kubeconfig          Export kubeconfig
#   ./cluster.sh workloads <type>    Create test workloads
#   ./cluster.sh help                Show this help
#
# Profiles:
#   flannel          Default Flannel CNI (simplest)
#   cilium           Cilium CNI with kube-proxy (legacy mode)
#   cilium-ebpf      Cilium CNI in eBPF mode (kube-proxy replacement)
#   kubespan         Flannel + KubeSpan mesh networking
#   cilium-kubespan  Cilium eBPF + KubeSpan (problematic combo for testing)
#   cilium-hubble    Cilium eBPF + Hubble observability
#

set -euo pipefail

# Configuration
CLUSTER_NAME="${TALOS_CLUSTER_NAME:-talos-pilot}"
WORKERS="${TALOS_WORKERS:-0}"
PROVISIONER="${TALOS_PROVISIONER:-docker}"
# Note: Docker provisioner always creates 1 control plane, this is only used for display
CONTROLPLANES=1

# Paths
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PATCHES_DIR="${SCRIPT_DIR}/../patches"
OUTPUT_DIR="${SCRIPT_DIR}/../output"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() { echo -e "${BLUE}[INFO]${NC} $*"; }
log_success() { echo -e "${GREEN}[OK]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

# Ensure output directory exists
mkdir -p "${OUTPUT_DIR}"

#
# Check prerequisites
#
check_prerequisites() {
    local missing=()

    command -v talosctl &>/dev/null || missing+=("talosctl")
    command -v kubectl &>/dev/null || missing+=("kubectl")
    command -v helm &>/dev/null || missing+=("helm")
    command -v docker &>/dev/null || missing+=("docker")

    if [[ ${#missing[@]} -gt 0 ]]; then
        log_error "Missing required tools: ${missing[*]}"
        echo ""
        echo "Install instructions:"
        echo "  talosctl: https://www.talos.dev/latest/introduction/getting-started/#talosctl"
        echo "  kubectl:  https://kubernetes.io/docs/tasks/tools/"
        echo "  helm:     https://helm.sh/docs/intro/install/"
        echo "  docker:   https://docs.docker.com/get-docker/"
        exit 1
    fi

    # Check Docker is running
    if ! docker info &>/dev/null; then
        log_error "Docker is not running"
        exit 1
    fi
}

#
# Get the control plane IP
# For Docker provisioner, this is always 10.5.0.2 (internal network IP)
#
get_cp_ip() {
    # Docker provisioner always uses 10.5.0.2 for the control plane
    echo "10.5.0.2"
}

#
# Wait for Talos API to be ready
#
wait_for_talos_api() {
    local timeout="${1:-120}"
    local start_time=$(date +%s)

    while true; do
        if talosctl version --nodes 10.5.0.2 &>/dev/null; then
            log_success "Talos API is ready"
            return 0
        fi

        local elapsed=$(($(date +%s) - start_time))
        if [[ "${elapsed}" -ge "${timeout}" ]]; then
            log_error "Timeout waiting for Talos API"
            return 1
        fi

        sleep 3
    done
}

#
# Wait for Kubernetes API to be accessible
#
wait_for_k8s_api() {
    local timeout="${1:-180}"
    local start_time=$(date +%s)

    # Ensure KUBECONFIG is set
    export KUBECONFIG="${OUTPUT_DIR}/kubeconfig"

    while true; do
        # Try to get API health, suppress all errors
        if kubectl get --raw /healthz &>/dev/null; then
            log_success "Kubernetes API is ready"
            return 0
        fi

        local elapsed=$(($(date +%s) - start_time))
        if [[ "${elapsed}" -ge "${timeout}" ]]; then
            log_error "Timeout waiting for Kubernetes API"
            return 1
        fi

        echo -ne "\r  Waiting for K8s API... (${elapsed}s elapsed)"
        sleep 3
    done
}

#
# Wait for nodes to be ready
#
wait_for_nodes() {
    local timeout="${1:-300}"
    local expected_nodes=$((CONTROLPLANES + WORKERS))

    log_info "Waiting for ${expected_nodes} nodes to be Ready (timeout: ${timeout}s)..."

    local start_time=$(date +%s)
    while true; do
        # Count ready nodes, handling empty output gracefully
        local ready_nodes
        ready_nodes=$(kubectl get nodes --no-headers 2>/dev/null | grep -c " Ready " || true)
        ready_nodes=${ready_nodes:-0}
        # Ensure it's a clean integer
        ready_nodes=$(echo "${ready_nodes}" | tr -d '[:space:]')

        if [[ "${ready_nodes}" -ge "${expected_nodes}" ]]; then
            log_success "All ${expected_nodes} nodes are Ready"
            return 0
        fi

        local elapsed=$(($(date +%s) - start_time))
        if [[ "${elapsed}" -ge "${timeout}" ]]; then
            log_error "Timeout waiting for nodes (${ready_nodes}/${expected_nodes} ready)"
            kubectl get nodes
            return 1
        fi

        echo -ne "\r  ${ready_nodes}/${expected_nodes} nodes ready (${elapsed}s elapsed)..."
        sleep 5
    done
}

#
# Wait for pods in namespace to be ready
#
# Note: Ignores Pending pods that can't be scheduled (e.g., due to anti-affinity
# on single-node clusters). Only waits for pods that are actively failing.
#
wait_for_pods() {
    local namespace="$1"
    local timeout="${2:-300}"

    log_info "Waiting for pods in ${namespace} to be Ready..."

    local start_time=$(date +%s)
    while true; do
        # Get pods that are actually failing (not just Pending due to scheduling)
        # Running/Completed = OK
        # Pending = might be OK (scheduling constraints) - check if it's stuck
        # Other states = problem
        local pods_output
        pods_output=$(kubectl get pods -n "${namespace}" --no-headers 2>/dev/null)

        local problem_pods=0
        if [[ -n "${pods_output}" ]]; then
            # Count pods that are failing (CrashLoop, Error, ImagePull, etc.)
            # Exclude: Running, Completed, Pending (scheduling issues are OK)
            problem_pods=$(echo "${pods_output}" | grep -v "Running\|Completed\|Pending" | wc -l)
            problem_pods=$(echo "${problem_pods}" | tr -d '[:space:]')
            problem_pods=$((10#${problem_pods:-0}))

            # Also check if any Running pods are not ready (0/1)
            local not_ready_running
            not_ready_running=$(echo "${pods_output}" | grep "Running" | grep -E "0/[0-9]+" | wc -l)
            not_ready_running=$(echo "${not_ready_running}" | tr -d '[:space:]')
            not_ready_running=$((10#${not_ready_running:-0}))

            problem_pods=$((problem_pods + not_ready_running))
        fi

        if [[ "${problem_pods}" -eq 0 ]]; then
            # Check if we have at least some Running pods (not just all Pending)
            local running_pods
            running_pods=$(echo "${pods_output}" | grep -c "Running" || echo "0")
            running_pods=$(echo "${running_pods}" | tr -d '[:space:]')
            running_pods=$((10#${running_pods:-0}))

            if [[ "${running_pods}" -gt 0 ]]; then
                log_success "Pods in ${namespace} are healthy (${running_pods} running)"
                return 0
            fi
        fi

        local elapsed=$(($(date +%s) - start_time))
        if [[ "${elapsed}" -ge "${timeout}" ]]; then
            log_warn "Timeout waiting for pods in ${namespace}"
            kubectl get pods -n "${namespace}"
            return 1
        fi

        echo -ne "\r  Waiting for pods... (${elapsed}s elapsed, ${problem_pods} issues)"
        sleep 3
    done
}

#
# Install Cilium CNI
#
install_cilium() {
    local mode="${1:-ebpf}"  # ebpf or legacy
    local with_hubble="${2:-false}"

    log_info "Adding Cilium Helm repository..."
    helm repo add cilium https://helm.cilium.io/ 2>/dev/null || true
    helm repo update cilium

    log_info "Installing Cilium (mode: ${mode}, hubble: ${with_hubble})..."

    local helm_args=(
        "cilium"
        "cilium/cilium"
        "--namespace" "kube-system"
        "--set" "ipam.mode=kubernetes"
        "--set" "securityContext.capabilities.ciliumAgent={CHOWN,KILL,NET_ADMIN,NET_RAW,IPC_LOCK,SYS_ADMIN,SYS_RESOURCE,DAC_OVERRIDE,FOWNER,SETGID,SETUID}"
        "--set" "securityContext.capabilities.cleanCiliumState={NET_ADMIN,SYS_ADMIN,SYS_RESOURCE}"
        "--set" "cgroup.autoMount.enabled=false"
        "--set" "cgroup.hostRoot=/sys/fs/cgroup"
    )

    if [[ "${mode}" == "ebpf" ]]; then
        helm_args+=("--set" "kubeProxyReplacement=true")
        # For eBPF mode, we need to specify the k8s service host/port
        local cp_ip=$(get_cp_ip)
        helm_args+=("--set" "k8sServiceHost=${cp_ip}")
        helm_args+=("--set" "k8sServicePort=6443")
    else
        helm_args+=("--set" "kubeProxyReplacement=false")
    fi

    if [[ "${with_hubble}" == "true" ]]; then
        helm_args+=(
            "--set" "hubble.enabled=true"
            "--set" "hubble.relay.enabled=true"
            "--set" "hubble.ui.enabled=true"
            "--set" "hubble.metrics.enabled={dns,drop,tcp,flow,icmp,http}"
        )
    fi

    helm install "${helm_args[@]}"

    log_info "Waiting for Cilium to be ready..."
    wait_for_pods "kube-system" 300
}

#
# Create cluster with Flannel (default)
#
# Uses the same robust pattern as Cilium: run talosctl in background,
# wait for APIs, then wait for nodes.
#
create_flannel() {
    log_info "Creating cluster with Flannel CNI..."

    # Run talosctl in background for consistent behavior
    talosctl cluster create "${PROVISIONER}" \
        --name "${CLUSTER_NAME}" \
        --workers "${WORKERS}" &
    local cluster_pid=$!

    log_info "Waiting for Talos API..."
    wait_for_talos_api
    export_kubeconfig

    log_info "Waiting for Kubernetes API..."
    wait_for_k8s_api

    log_info "Waiting for cluster bootstrap to complete..."
    wait $cluster_pid || true
    wait_for_nodes
}

#
# Create cluster with Cilium (legacy mode - with kube-proxy)
#
create_cilium_legacy() {
    log_info "Creating cluster with Cilium CNI (legacy mode)..."

    # Run talosctl in background (same pattern as create_cilium_ebpf)
    talosctl cluster create "${PROVISIONER}" \
        --name "${CLUSTER_NAME}" \
        --workers "${WORKERS}" \
        --config-patch "@${PATCHES_DIR}/cilium-cni.yaml" &
    local cluster_pid=$!

    log_info "Waiting for Talos API..."
    wait_for_talos_api
    export_kubeconfig

    log_info "Waiting for Kubernetes API..."
    wait_for_k8s_api

    install_cilium "legacy" "false"

    log_info "Waiting for cluster bootstrap to complete..."
    wait $cluster_pid || true
    wait_for_nodes
}

#
# Create cluster with Cilium eBPF mode (kube-proxy replacement)
#
# Note: For CNI=none clusters, we need to install Cilium WHILE talosctl
# is waiting for nodes to be Ready (chicken-and-egg problem).
#
create_cilium_ebpf() {
    log_info "Creating cluster with Cilium CNI (eBPF mode)..."

    # Run talosctl in background since it will wait for node Ready,
    # but node won't be Ready until Cilium is installed
    talosctl cluster create "${PROVISIONER}" \
        --name "${CLUSTER_NAME}" \
        --workers "${WORKERS}" \
        --config-patch "@${PATCHES_DIR}/cilium-ebpf.yaml" &
    local cluster_pid=$!

    # Wait for Talos API to be ready (so we can get kubeconfig)
    log_info "Waiting for Talos API..."
    wait_for_talos_api

    # Export kubeconfig
    export_kubeconfig

    # Wait for K8s API to be accessible
    log_info "Waiting for Kubernetes API..."
    wait_for_k8s_api

    # Now install Cilium - this will make the node Ready
    install_cilium "ebpf" "false"

    # Wait for talosctl cluster create to finish
    log_info "Waiting for cluster bootstrap to complete..."
    wait $cluster_pid || true

    # Final node check
    wait_for_nodes
}

#
# Create cluster with Cilium eBPF + Hubble
#
create_cilium_hubble() {
    log_info "Creating cluster with Cilium CNI (eBPF mode) + Hubble..."

    # Run talosctl in background (same pattern as create_cilium_ebpf)
    talosctl cluster create "${PROVISIONER}" \
        --name "${CLUSTER_NAME}" \
        --workers "${WORKERS}" \
        --config-patch "@${PATCHES_DIR}/cilium-ebpf.yaml" &
    local cluster_pid=$!

    log_info "Waiting for Talos API..."
    wait_for_talos_api
    export_kubeconfig

    log_info "Waiting for Kubernetes API..."
    wait_for_k8s_api

    install_cilium "ebpf" "true"

    log_info "Waiting for cluster bootstrap to complete..."
    wait $cluster_pid || true
    wait_for_nodes

    log_info "Hubble UI available via: kubectl port-forward -n kube-system svc/hubble-ui 12000:80"
}

#
# Create cluster with KubeSpan (Flannel + WireGuard mesh)
#
# Uses the same robust pattern as Cilium: run talosctl in background,
# wait for APIs, then wait for nodes.
#
create_kubespan() {
    log_info "Creating cluster with Flannel + KubeSpan..."

    # Run talosctl in background for consistent behavior
    talosctl cluster create "${PROVISIONER}" \
        --name "${CLUSTER_NAME}" \
        --workers "${WORKERS}" \
        --config-patch "@${PATCHES_DIR}/kubespan.yaml" &
    local cluster_pid=$!

    log_info "Waiting for Talos API..."
    wait_for_talos_api
    export_kubeconfig

    log_info "Waiting for Kubernetes API..."
    wait_for_k8s_api

    log_info "Waiting for cluster bootstrap to complete..."
    wait $cluster_pid || true
    wait_for_nodes

    log_info "Checking KubeSpan status..."
    local cp_ip=$(get_cp_ip)
    talosctl get kubespanpeerstatus -n "${cp_ip}" || log_warn "Could not get KubeSpan status"
}

#
# Create cluster with Cilium eBPF + KubeSpan (the problematic combo)
#
create_cilium_kubespan() {
    log_warn "Creating cluster with Cilium eBPF + KubeSpan..."
    log_warn "This combination may have networking issues due to asymmetric routing!"
    echo ""

    # Combine patches
    local combined_patch="${OUTPUT_DIR}/cilium-kubespan-combined.yaml"
    cat > "${combined_patch}" << 'EOF'
# Combined: Cilium eBPF + KubeSpan
# WARNING: This combination may cause networking issues
machine:
  network:
    kubespan:
      enabled: true
cluster:
  network:
    cni:
      name: none
  proxy:
    disabled: true
EOF

    # Run talosctl in background (same pattern as create_cilium_ebpf)
    talosctl cluster create "${PROVISIONER}" \
        --name "${CLUSTER_NAME}" \
        --workers "${WORKERS}" \
        --config-patch "@${combined_patch}" &
    local cluster_pid=$!

    log_info "Waiting for Talos API..."
    wait_for_talos_api
    export_kubeconfig

    log_info "Waiting for Kubernetes API..."
    wait_for_k8s_api

    install_cilium "ebpf" "false"

    log_info "Waiting for cluster bootstrap to complete..."
    wait $cluster_pid || true
    wait_for_nodes

    log_warn "Cluster created with Cilium eBPF + KubeSpan"
    log_warn "Use this to test the compatibility warning feature!"
}

#
# Export kubeconfig (with retry for early-stage clusters)
#
export_kubeconfig() {
    local kubeconfig_path="${OUTPUT_DIR}/kubeconfig"
    local timeout="${1:-120}"
    local start_time=$(date +%s)
    local cp_ip="10.5.0.2"  # Docker provisioner always uses this IP

    log_info "Exporting kubeconfig to ${kubeconfig_path}..."

    while true; do
        # Need to specify --nodes since Docker provisioner doesn't set it in config
        if talosctl kubeconfig "${kubeconfig_path}" --nodes "${cp_ip}" --force 2>/dev/null; then
            export KUBECONFIG="${kubeconfig_path}"
            log_success "Kubeconfig exported"
            echo ""
            echo "To use this cluster:"
            echo "  export KUBECONFIG=${kubeconfig_path}"
            echo ""
            return 0
        fi

        local elapsed=$(($(date +%s) - start_time))
        if [[ "${elapsed}" -ge "${timeout}" ]]; then
            log_warn "Could not export kubeconfig within timeout"
            export KUBECONFIG="${kubeconfig_path}"
            return 1
        fi

        sleep 3
    done
}

#
# Destroy cluster
#
destroy_cluster() {
    log_info "Destroying cluster ${CLUSTER_NAME}..."

    talosctl cluster destroy --name "${CLUSTER_NAME}" 2>/dev/null || true

    # Clean up output files
    rm -f "${OUTPUT_DIR}/kubeconfig"
    rm -f "${OUTPUT_DIR}"/*.yaml

    log_success "Cluster destroyed"
}

#
# Show cluster status
#
show_status() {
    echo ""
    echo "=== Cluster Status ==="
    echo ""

    # Check if cluster exists
    if ! talosctl config info &>/dev/null; then
        log_warn "No cluster configured"
        return 1
    fi

    local cp_ip=$(get_cp_ip)

    echo "Control Plane IP: ${cp_ip}"
    echo ""

    echo "--- Talos Nodes ---"
    talosctl get members -n "${cp_ip}" 2>/dev/null || log_warn "Could not get Talos members"
    echo ""

    echo "--- Kubernetes Nodes ---"
    kubectl get nodes -o wide 2>/dev/null || log_warn "Could not get K8s nodes"
    echo ""

    echo "--- CNI Detection ---"
    local cni="unknown"
    if kubectl get pods -n kube-system -l k8s-app=cilium 2>/dev/null | grep -q cilium; then
        cni="Cilium"
        # Check if kube-proxy exists
        if kubectl get pods -n kube-system -l k8s-app=kube-proxy 2>/dev/null | grep -q kube-proxy; then
            cni="Cilium (legacy mode with kube-proxy)"
        else
            cni="Cilium (eBPF mode, kube-proxy replacement)"
        fi
        # Check Hubble
        if kubectl get pods -n kube-system -l k8s-app=hubble-relay 2>/dev/null | grep -q hubble; then
            cni="${cni} + Hubble"
        fi
    elif kubectl get pods -n kube-system -l app=flannel 2>/dev/null | grep -q flannel; then
        cni="Flannel"
    elif kubectl get pods -n kube-system -l k8s-app=calico-node 2>/dev/null | grep -q calico; then
        cni="Calico"
    fi
    echo "CNI: ${cni}"
    echo ""

    echo "--- KubeSpan Status ---"
    if talosctl get kubespanpeerstatus -n "${cp_ip}" 2>/dev/null; then
        echo "KubeSpan: Enabled"
    else
        echo "KubeSpan: Disabled or not available"
    fi
    echo ""

    echo "--- Pod Summary ---"
    kubectl get pods -A --no-headers 2>/dev/null | awk '
        {
            status[$4]++
            total++
        }
        END {
            for (s in status) printf "  %s: %d\n", s, status[s]
            printf "  Total: %d\n", total
        }
    '
}

#
# Create test workloads - comprehensive set for testing workload health screen
#
create_workloads() {
    local workload_type="${1:-all}"

    case "${workload_type}" in
        healthy)
            log_info "Creating healthy workloads..."
            kubectl create namespace test-healthy 2>/dev/null || true
            # Deployments
            kubectl create deployment nginx --image=nginx:alpine --replicas=3 -n test-healthy
            kubectl create deployment redis --image=redis:alpine --replicas=2 -n test-healthy
            # StatefulSet
            cat <<EOF | kubectl apply -f -
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: web
  namespace: test-healthy
spec:
  serviceName: "web"
  replicas: 2
  selector:
    matchLabels:
      app: web
  template:
    metadata:
      labels:
        app: web
    spec:
      containers:
      - name: nginx
        image: nginx:alpine
        ports:
        - containerPort: 80
EOF
            log_success "Created healthy workloads (3 Deployments, 1 StatefulSet) in test-healthy namespace"
            ;;

        crashloop)
            log_info "Creating CrashLoopBackOff workload..."
            kubectl create namespace test-failing 2>/dev/null || true
            kubectl create deployment crasher --image=busybox -n test-failing -- /bin/sh -c "exit 1"
            log_success "Created crasher deployment (will enter CrashLoopBackOff)"
            ;;

        imagepull)
            log_info "Creating ImagePullBackOff workload..."
            kubectl create namespace test-failing 2>/dev/null || true
            kubectl create deployment bad-image --image=nonexistent-registry.invalid/fake:v999 -n test-failing
            log_success "Created bad-image deployment (will enter ImagePullBackOff)"
            ;;

        pending)
            log_info "Creating Pending workload..."
            kubectl create namespace test-failing 2>/dev/null || true
            cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: pending-pod
  namespace: test-failing
spec:
  containers:
  - name: nginx
    image: nginx:alpine
    resources:
      requests:
        cpu: "100"
        memory: "1000Gi"
EOF
            log_success "Created pending-pod (will stay Pending due to resource requests)"
            ;;

        oomkill)
            log_info "Creating OOMKilled workload..."
            kubectl create namespace test-failing 2>/dev/null || true
            cat <<EOF | kubectl apply -f -
apiVersion: apps/v1
kind: Deployment
metadata:
  name: oom-killer
  namespace: test-failing
spec:
  replicas: 1
  selector:
    matchLabels:
      app: oom-killer
  template:
    metadata:
      labels:
        app: oom-killer
    spec:
      containers:
      - name: oom
        image: polinux/stress
        resources:
          limits:
            memory: "50Mi"
        command: ["stress"]
        args: ["--vm", "1", "--vm-bytes", "200M", "--vm-hang", "1"]
EOF
            log_success "Created oom-killer deployment (will get OOMKilled)"
            ;;

        highrestarts)
            log_info "Creating high-restart workload..."
            kubectl create namespace test-degraded 2>/dev/null || true
            cat <<EOF | kubectl apply -f -
apiVersion: apps/v1
kind: Deployment
metadata:
  name: flaky-app
  namespace: test-degraded
spec:
  replicas: 1
  selector:
    matchLabels:
      app: flaky-app
  template:
    metadata:
      labels:
        app: flaky-app
    spec:
      containers:
      - name: flaky
        image: busybox
        command: ["/bin/sh", "-c", "sleep 5 && exit 0"]
EOF
            log_success "Created flaky-app deployment (will accumulate restarts)"
            ;;

        degraded)
            log_info "Creating degraded workload (partial replicas)..."
            kubectl create namespace test-degraded 2>/dev/null || true
            # Create deployment with 3 replicas but one will fail
            cat <<EOF | kubectl apply -f -
apiVersion: apps/v1
kind: Deployment
metadata:
  name: partial-ready
  namespace: test-degraded
spec:
  replicas: 3
  selector:
    matchLabels:
      app: partial-ready
  template:
    metadata:
      labels:
        app: partial-ready
    spec:
      containers:
      - name: main
        image: nginx:alpine
      initContainers:
      - name: init
        image: busybox
        command: ["/bin/sh", "-c", "if [ \$(hostname | grep -o '[0-9]*\$') = '2' ]; then exit 1; fi; exit 0"]
EOF
            log_success "Created partial-ready deployment (will have some pods failing init)"
            ;;

        statefulset)
            log_info "Creating StatefulSet workloads..."
            kubectl create namespace test-stateful 2>/dev/null || true
            # Healthy StatefulSet
            cat <<EOF | kubectl apply -f -
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: db
  namespace: test-stateful
spec:
  serviceName: "db"
  replicas: 3
  selector:
    matchLabels:
      app: db
  template:
    metadata:
      labels:
        app: db
    spec:
      containers:
      - name: postgres
        image: postgres:alpine
        env:
        - name: POSTGRES_PASSWORD
          value: "testpassword"
        ports:
        - containerPort: 5432
EOF
            # Failing StatefulSet
            cat <<EOF | kubectl apply -f -
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: db-broken
  namespace: test-stateful
spec:
  serviceName: "db-broken"
  replicas: 2
  selector:
    matchLabels:
      app: db-broken
  template:
    metadata:
      labels:
        app: db-broken
    spec:
      containers:
      - name: postgres
        image: postgres:alpine
        # Missing POSTGRES_PASSWORD will cause crash
        ports:
        - containerPort: 5432
EOF
            log_success "Created StatefulSet workloads (1 healthy, 1 failing) in test-stateful namespace"
            ;;

        daemonset)
            log_info "Creating DaemonSet workloads..."
            kubectl create namespace test-daemonset 2>/dev/null || true
            # Healthy DaemonSet
            cat <<EOF | kubectl apply -f -
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: node-exporter
  namespace: test-daemonset
spec:
  selector:
    matchLabels:
      app: node-exporter
  template:
    metadata:
      labels:
        app: node-exporter
    spec:
      containers:
      - name: exporter
        image: nginx:alpine
        ports:
        - containerPort: 9100
EOF
            log_success "Created DaemonSet workload in test-daemonset namespace"
            ;;

        pdb)
            log_info "Creating workload with PodDisruptionBudget..."
            kubectl create namespace test-pdb 2>/dev/null || true
            kubectl create deployment pdb-test --image=nginx:alpine --replicas=3 -n test-pdb
            cat <<EOF | kubectl apply -f -
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: pdb-test
  namespace: test-pdb
spec:
  minAvailable: 3
  selector:
    matchLabels:
      app: pdb-test
EOF
            log_success "Created pdb-test with PDB (minAvailable: 3, will block drains)"
            ;;

        mixed)
            log_info "Creating mixed namespace with healthy and failing workloads..."
            kubectl create namespace test-mixed 2>/dev/null || true
            # Healthy deployment
            kubectl create deployment app-frontend --image=nginx:alpine --replicas=2 -n test-mixed
            kubectl create deployment app-backend --image=redis:alpine --replicas=1 -n test-mixed
            # Failing deployment
            kubectl create deployment app-worker --image=busybox -n test-mixed -- /bin/sh -c "exit 1"
            log_success "Created mixed workloads (2 healthy, 1 failing) in test-mixed namespace"
            ;;

        kitchen-sink)
            log_info "Setting up kitchen-sink workloads for comprehensive testing..."
            echo ""

            # Remove control plane taint for single-node clusters
            log_info "Removing control plane taint for single-node cluster..."
            kubectl taint nodes --all node-role.kubernetes.io/control-plane:NoSchedule- 2>/dev/null || true
            log_success "Control plane taint removed (workloads can schedule on control plane)"

            # Clean up any existing test workloads first
            log_info "Cleaning up existing test workloads..."
            kubectl delete namespace test-healthy test-failing test-degraded test-stateful test-daemonset test-pdb test-mixed 2>/dev/null || true
            sleep 2

            # Create all workloads
            create_workloads healthy
            create_workloads crashloop
            create_workloads imagepull
            create_workloads pending
            create_workloads oomkill
            create_workloads highrestarts
            create_workloads statefulset
            create_workloads daemonset
            create_workloads pdb
            create_workloads mixed

            echo ""
            log_success "Kitchen-sink workloads created!"
            echo ""
            echo "This creates every scenario for testing the Workload Health screen:"
            echo ""
            echo "NAMESPACES:"
            echo "  test-healthy   - Healthy Deployments (nginx, redis) + StatefulSet"
            echo "  test-failing   - CrashLoopBackOff, ImagePullBackOff, Pending, OOMKilled"
            echo "  test-degraded  - High restarts, partial ready"
            echo "  test-stateful  - StatefulSets (1 healthy, 1 failing)"
            echo "  test-daemonset - DaemonSet"
            echo "  test-pdb       - Deployment with PodDisruptionBudget"
            echo "  test-mixed     - Mix of healthy and failing in same namespace"
            echo ""
            echo "POD STATES YOU'LL SEE:"
            echo "  ● Running          - Healthy pods"
            echo "  ✗ CrashLoopBackOff - Pods that keep crashing"
            echo "  ✗ ImagePullBackOff - Invalid image reference"
            echo "  ○ Pending          - Can't be scheduled (resource constraints)"
            echo "  ✗ OOMKilled        - Out of memory kills"
            echo "  ◐ High Restarts    - Pods with >5 restarts"
            echo ""
            echo "Wait ~30 seconds for pods to enter their error states, then run:"
            echo "  cargo run --bin talos-pilot"
            echo "  Press 'w' to view workload health"
            ;;

        all)
            create_workloads healthy
            create_workloads crashloop
            create_workloads imagepull
            create_workloads pending
            create_workloads oomkill
            create_workloads highrestarts
            create_workloads statefulset
            create_workloads daemonset
            create_workloads pdb
            create_workloads mixed
            echo ""
            log_success "Created all test workloads!"
            echo ""
            echo "Namespaces created:"
            echo "  test-healthy   - Healthy Deployments and StatefulSet"
            echo "  test-failing   - CrashLoopBackOff, ImagePullBackOff, Pending, OOMKilled"
            echo "  test-degraded  - High restarts, partial ready"
            echo "  test-stateful  - StatefulSets (healthy and failing)"
            echo "  test-daemonset - DaemonSet"
            echo "  test-pdb       - Deployment with PDB"
            echo "  test-mixed     - Mix of healthy and failing"
            echo ""
            echo "Wait ~30 seconds for pods to enter their error states, then run:"
            echo "  cargo run --bin talos-pilot"
            echo "  Press 'w' to view workload health"
            ;;

        drainable)
            log_info "Creating drainable workloads for testing node operations..."
            kubectl create namespace test-drainable 2>/dev/null || true

            # Uncordon all nodes (in case they were cordoned from previous drain tests)
            log_info "Uncordoning all nodes..."
            kubectl uncordon --all 2>/dev/null || true

            # Remove control plane taint so pods can schedule
            log_info "Removing control plane taint..."
            kubectl taint nodes --all node-role.kubernetes.io/control-plane:NoSchedule- 2>/dev/null || true

            # Give scheduler a moment to pick up the changes
            sleep 2

            # Simple deployments without PDB (PDBs don't work on single-node clusters)
            kubectl create deployment web --image=nginx:alpine --replicas=3 -n test-drainable
            kubectl create deployment api --image=nginx:alpine --replicas=2 -n test-drainable

            log_success "Created drainable workloads in test-drainable namespace"
            echo ""
            echo "These workloads CAN be drained:"
            echo "  - web (3 replicas, no PDB)"
            echo "  - api (2 replicas, no PDB)"
            echo ""
            echo "NOTE: PDBs are not used because they don't work on single-node clusters"
            echo "(evicted pods can't reschedule when the only node is cordoned)"
            echo ""
            echo "Test drain with: press 'o' on the node, then 'd' for drain"
            echo ""
            echo "NOTE: Docker-based Talos clusters support reboot, but it's a"
            echo "container restart rather than a true node reboot. The operation"
            echo "will complete quickly (~5-10 seconds) vs a real node reboot."
            ;;

        clean)
            log_info "Cleaning up test workloads..."
            kubectl delete namespace test-healthy test-failing test-degraded test-stateful test-daemonset test-pdb test-mixed test-drainable 2>/dev/null || true
            log_success "Cleaned up all test namespaces"
            ;;

        *)
            log_error "Unknown workload type: ${workload_type}"
            echo "Available types:"
            echo "  kitchen-sink - [RECOMMENDED] All scenarios + removes control plane taint"
            echo "  drainable    - Workloads for testing drain/reboot operations"
            echo "  healthy      - Healthy Deployments and StatefulSet"
            echo "  crashloop    - CrashLoopBackOff deployment"
            echo "  imagepull    - ImagePullBackOff deployment"
            echo "  pending      - Pending pod (impossible resources)"
            echo "  oomkill      - OOMKilled deployment"
            echo "  highrestarts - High restart count deployment"
            echo "  degraded     - Partially ready deployment"
            echo "  statefulset  - StatefulSet examples"
            echo "  daemonset    - DaemonSet example"
            echo "  pdb          - Deployment with PodDisruptionBudget (blocks drain)"
            echo "  mixed        - Mixed healthy/failing namespace"
            echo "  all          - Create all test workloads (no taint removal)"
            echo "  clean        - Delete all test workloads"
            exit 1
            ;;
    esac
}

#
# Show help
#
show_help() {
    cat << 'EOF'
Talos Test Cluster Management Script

USAGE:
    ./cluster.sh <command> [arguments]

COMMANDS:
    create <profile> [--force]  Create a new test cluster with the specified profile
    destroy                     Destroy the test cluster
    status                      Show cluster status (CNI, KubeSpan, nodes, pods)
    kubeconfig                  Export kubeconfig to output/kubeconfig
    workloads <type>            Create test workloads
    help                        Show this help

OPTIONS:
    --force, -f         Destroy existing cluster before creating new one

PROFILES:
    flannel             Default Flannel CNI (simplest setup)
    cilium              Cilium CNI with kube-proxy (legacy mode)
    cilium-ebpf         Cilium CNI in eBPF mode (replaces kube-proxy)
    cilium-hubble       Cilium eBPF + Hubble observability
    kubespan            Flannel + KubeSpan WireGuard mesh
    cilium-kubespan     Cilium eBPF + KubeSpan (problematic combo for testing)

WORKLOAD TYPES:
    kitchen-sink        [RECOMMENDED] All scenarios + removes control plane taint
    drainable           Workloads for testing drain/reboot operations
    healthy             Healthy Deployments and StatefulSet
    crashloop           CrashLoopBackOff deployment
    imagepull           ImagePullBackOff deployment
    pending             Pending pod (impossible resources)
    oomkill             OOMKilled deployment
    highrestarts        High restart count deployment
    degraded            Partially ready deployment
    statefulset         StatefulSet examples (healthy + failing)
    daemonset           DaemonSet example
    pdb                 Deployment with PodDisruptionBudget (blocks drain)
    mixed               Mixed healthy/failing namespace
    all                 Create all test workloads (no taint removal)
    clean               Delete all test workloads

ENVIRONMENT VARIABLES:
    TALOS_CLUSTER_NAME  Cluster name (default: talos-pilot)
    TALOS_CONTROLPLANES Number of control plane nodes (default: 1)
    TALOS_WORKERS       Number of worker nodes (default: 0)
    TALOS_VERSION       Talos version (default: v1.9.0)
    TALOS_PROVISIONER   Provisioner: docker or qemu (default: docker)

EXAMPLES:
    # Create a simple Flannel cluster
    ./cluster.sh create flannel

    # Create Cilium eBPF cluster with Hubble
    ./cluster.sh create cilium-hubble

    # Replace existing cluster with a new profile
    ./cluster.sh create cilium-ebpf --force

    # Create the problematic Cilium + KubeSpan combo for testing warnings
    ./cluster.sh create cilium-kubespan

    # Add comprehensive test workloads (recommended for development)
    ./cluster.sh workloads kitchen-sink

    # Check cluster status
    ./cluster.sh status

    # Clean up
    ./cluster.sh destroy

OUTPUT:
    Kubeconfig and generated files are saved to:
    test-clusters/output/

EOF
}

#
# Main
#
main() {
    local command="${1:-help}"
    shift || true

    check_prerequisites

    case "${command}" in
        create)
            local profile=""
            local force=false

            # Parse arguments
            while [[ $# -gt 0 ]]; do
                case "$1" in
                    --force|-f)
                        force=true
                        shift
                        ;;
                    -*)
                        log_error "Unknown option: $1"
                        exit 1
                        ;;
                    *)
                        if [[ -z "${profile}" ]]; then
                            profile="$1"
                        else
                            log_error "Unexpected argument: $1"
                            exit 1
                        fi
                        shift
                        ;;
                esac
            done

            if [[ -z "${profile}" ]]; then
                log_error "Profile required. Available: flannel, cilium, cilium-ebpf, cilium-hubble, kubespan, cilium-kubespan"
                exit 1
            fi

            # Check if cluster already exists (Docker containers or state directory)
            local cluster_exists=false
            if docker ps -a --filter "name=${CLUSTER_NAME}" --format "{{.Names}}" 2>/dev/null | grep -q "${CLUSTER_NAME}"; then
                cluster_exists=true
            elif [[ -d "${HOME}/.talos/clusters/${CLUSTER_NAME}" ]]; then
                cluster_exists=true
            fi

            if [[ "${cluster_exists}" == "true" ]]; then
                if [[ "${force}" == "true" ]]; then
                    log_warn "Cluster ${CLUSTER_NAME} exists, destroying first (--force)"
                    destroy_cluster
                else
                    log_error "Cluster ${CLUSTER_NAME} already exists. Run './cluster.sh destroy' or use --force"
                    exit 1
                fi
            fi

            case "${profile}" in
                flannel)        create_flannel ;;
                cilium)         create_cilium_legacy ;;
                cilium-ebpf)    create_cilium_ebpf ;;
                cilium-hubble)  create_cilium_hubble ;;
                kubespan)       create_kubespan ;;
                cilium-kubespan) create_cilium_kubespan ;;
                *)
                    log_error "Unknown profile: ${profile}"
                    echo "Available: flannel, cilium, cilium-ebpf, cilium-hubble, kubespan, cilium-kubespan"
                    exit 1
                    ;;
            esac

            echo ""
            log_success "Cluster created successfully!"
            echo ""
            echo "Next steps:"
            echo "  export KUBECONFIG=${OUTPUT_DIR}/kubeconfig"
            echo "  ./cluster.sh status"
            echo "  ./cluster.sh workloads all"
            ;;

        destroy)
            destroy_cluster
            ;;

        status)
            show_status
            ;;

        kubeconfig)
            export_kubeconfig
            ;;

        workloads)
            local workload_type="${1:-all}"
            export KUBECONFIG="${OUTPUT_DIR}/kubeconfig"
            create_workloads "${workload_type}"
            ;;

        help|--help|-h)
            show_help
            ;;

        *)
            log_error "Unknown command: ${command}"
            echo "Run './cluster.sh help' for usage"
            exit 1
            ;;
    esac
}

main "$@"
