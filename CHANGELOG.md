# Changelog

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
