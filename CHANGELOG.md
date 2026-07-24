# Changelog

## 0.1.11

Reliability release: node enumeration no longer depends on the Talos discovery service, and Kubernetes views always target the launched cluster.

### Bug Fixes

**Worker nodes missing when cluster discovery is disabled ([#25](https://github.com/Handfish/talos-pilot/issues/25), [#22](https://github.com/Handfish/talos-pilot/issues/22))**

0.1.10 fixed workers vanishing when `--config` broke context resolution, but they still disappeared whenever Talos cluster discovery returns no members. This is increasingly common: Sidero has disabled the Kubernetes discovery registry by default, leaving only the external `discovery.talos.dev` service registry — which hardened/airgapped clusters routinely turn off — so `talosctl get members` comes back empty as the norm, not the exception.

talos-pilot built its node roster from `talosctl get members`, and when that's empty it fell back to etcd membership — which only ever contains control-plane nodes — so workers dropped out (leaving the "cluster discovery returned no members" warning). It now falls back to the **Kubernetes API**, which knows every node (control plane and worker) independently of Talos discovery. The kubeconfig for that lookup is fetched directly from a control-plane node over the Talos API (certificate auth), so it always targets the correct cluster and never an unrelated one an ambient `KUBECONFIG` might point at. Displayed nodes still come from successful Talos queries, so unreachable/bogus IPs self-filter.

The same fallback is applied everywhere the app enumerates nodes independently of the main list:

- **Cluster node list** — workers appear again (and everything derived from it: rolling operations, per-node logs/processes/network/storage/operations).
- **Lifecycle (versions / config drift)** — the version-skew and config-drift table now covers all nodes, not just the endpoint the base client targets.

Reproduced on a discovery-disabled Docker cluster (1 control plane + 2 workers): before, both views showed the control plane only; after, all three nodes appear. Verified end-to-end + regression tests added.

**Lifecycle "Config" and "Time Sync" columns were always blank**

The Lifecycle node table showed `-` for every node's config version and time-sync status. Two causes: `talosctl get machineconfig` returns multiple YAML documents (the `persistent` and `v1alpha1` configs), but the parser tried to decode the whole blob as a single document and always errored; and per-node time-sync info was only fetched for the endpoint. Both are fixed — machineconfig parsing now splits documents and prefers the canonical `v1alpha1` config (tolerating a numeric `version`), and version + time-sync are queried per node — so the table is fully populated for control-plane and worker nodes alike. Regression tests added.

**Kubernetes views could target the wrong cluster when `KUBECONFIG` was set**

The workloads view and the drain/cordon operations build a Kubernetes client that preferred an ambient `KUBECONFIG` over the launched Talos context. If your shell's `KUBECONFIG` pointed at a *different* cluster than `--context`, read views silently showed the wrong cluster's data and — more seriously — drain/cordon could act on the wrong nodes.

These clients now pin to the launched cluster: the kubeconfig is fetched from a control-plane node over the Talos API, so it always identifies the cluster you asked for. An ambient `KUBECONFIG` is still honored when it names the **same** cluster (matched by cluster CA, so a VIP/load-balancer endpoint isn't mistaken for a different cluster) — preserving your configured, possibly more reachable endpoint — but is ignored when it names a different one. When it's ignored, a wrapped warning on the cluster view explains why, so nothing is silently overridden. Verified end-to-end against a cluster with a mismatched `KUBECONFIG`; regression tests added.

## 0.1.10

Reliability release focused on connectivity, node discovery, and node operations.

### Bug Fixes

**Multi-endpoint failover — app no longer hangs silently ([#27](https://github.com/Handfish/talos-pilot/issues/27))**

talos-pilot now fails over across endpoints like `talosctl`: it dials all of a context's endpoints concurrently and uses the first that actually connects (bounded by a 10s timeout), so a dead first endpoint is skipped instead of hanging the session. The old "Successfully connected" log was just tonic's lazy channel, not a real handshake — hence the ~21s wait before the first RPC failed. Startup no longer blocks on a blank screen: there's now a cancellable "Connecting…" screen (q/Esc/Ctrl-C), and an all-unreachable cluster shows a clear Connection Error panel. Verified on a multi-endpoint cluster with the dead endpoint first — connects in ~2ms where it used to hang, so the reorder-endpoints workaround is no longer needed. (commit 9cab34f)

**Lifecycle/Versions view now fetches data ([#26](https://github.com/Handfish/talos-pilot/issues/26))**

The Lifecycle view fetched the Talos version first and treated it as a hard gate: on failure it cleared its own error and returned early, skipping the etcd/K8s/pod checks entirely (hence zero follow-up requests and the silent "unknown" fields). Now a version-fetch failure just logs a warning and continues; the other checks fire regardless, and you only get an error banner if the whole refresh comes back empty. Regression tests added. (commit 1ce1e01)

**Worker nodes no longer missing ([#25](https://github.com/Handfish/talos-pilot/issues/25), [#22](https://github.com/Handfish/talos-pilot/issues/22))**

talos-pilot fetches the full node list (workers included) via `talosctl get members`, but it wasn't passing `--talosconfig`, so if you ran with `--config <path>` talosctl fell back to the default `~/.talos/config`, couldn't find your context, and the list silently collapsed to just the control-plane nodes. It now passes your config through correctly. Node roles are now determined from the discovery `machineType` (not inferred from the etcd service), and if worker discovery genuinely can't run (discovery service disabled, or `talosctl` not on PATH) you'll now see a visible warning instead of a silently short list. Verified end-to-end + tests added. (commit fd5080f)

**"No K8s client available" when rebooting a node ([#23](https://github.com/Handfish/talos-pilot/issues/23))**

When you open node operations, talos-pilot builds a Kubernetes client to run the pre-drain safety checks. It was only trying your environment kubeconfig and then a VIP fallback — and since we build kube-rs without the exec/OIDC auth plugins, a kubeconfig that `kubectl` accepts (e.g. EKS/OIDC) could fail to load, leaving no client, so the reboot bailed out with the generic "No K8s client available". It now fetches the kubeconfig directly from a control-plane node over the Talos API (certificate auth, always available), so it no longer depends on your local kubeconfig's auth method. And if a client genuinely can't be created, you'll now see the specific reason instead of the generic message. Verified that drain/reboot safety checks load even with no usable local kubeconfig. (commit 49814c7)

## 0.1.9

See the git history for changes prior to 0.1.10.
