//! Pick a reachable address for each discovery member
//!
//! A Talos discovery member lists every address the node has (public, private,
//! KubeSpan, ...) in no meaningful order. Callers target a node through
//! `addresses.first()`, proxied via the connected endpoint's apid, so if the
//! first address is one the endpoint cannot reach (e.g. a firewalled public
//! IP) every query to that node fails and it silently drops out of the views.
//!
//! [`prefer_reachable_addresses`] probes each member's addresses and moves the
//! first one that answers to the front, so every `addresses.first()` consumer
//! targets a working address.

use std::time::Duration;
use talos_rs::{DiscoveryMember, TalosClient};

/// Upper bound for a single probe. A dead address usually fails fast through
/// the apid proxy; this only guards against a hung dial.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Order the addresses to try: known-good `hints` first (e.g. etcd member IPs
/// or the address that worked on the previous refresh), then the rest in their
/// original order.
fn candidate_order(addresses: &[String], hints: &[String]) -> Vec<String> {
    let (mut hinted, rest): (Vec<String>, Vec<String>) =
        addresses.iter().cloned().partition(|a| hints.contains(a));
    hinted.extend(rest);
    hinted
}

/// Move each member's first reachable address to the front of its list.
///
/// Members are probed concurrently. A member with no reachable address is left
/// unchanged, so callers degrade exactly as before.
pub async fn prefer_reachable_addresses(
    client: &TalosClient,
    members: &mut [DiscoveryMember],
    hints: &[String],
) {
    let probes = members.iter().map(|member| {
        let candidates = candidate_order(&member.addresses, hints);
        async move {
            for ip in candidates {
                let probe = client.with_node(&ip);
                if let Ok(Ok(_)) = tokio::time::timeout(PROBE_TIMEOUT, probe.version()).await {
                    return Some(ip);
                }
                tracing::debug!("Node {} not reachable via {}", member.hostname, ip);
            }
            None
        }
    });
    let reachable = futures::future::join_all(probes).await;

    for (member, ip) in members.iter_mut().zip(reachable) {
        match ip {
            Some(ip) => {
                if let Some(pos) = member.addresses.iter().position(|a| *a == ip) {
                    member.addresses[..=pos].rotate_right(1);
                }
            }
            None => tracing::warn!(
                "Node {} not reachable via any of its addresses: {:?}",
                member.hostname,
                member.addresses
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn hinted_addresses_are_tried_first() {
        // kpc2-style member: firewalled public IP listed first, etcd IP last
        let addresses = strings(&["5.9.183.236", "10.22.1.2", "100.100.0.7"]);
        let hints = strings(&["100.100.0.8", "100.100.0.7", "100.100.0.6"]);
        assert_eq!(
            candidate_order(&addresses, &hints),
            strings(&["100.100.0.7", "5.9.183.236", "10.22.1.2"])
        );
    }

    #[test]
    fn original_order_kept_without_hints() {
        let addresses = strings(&["10.22.1.4", "88.198.66.196", "100.100.0.3"]);
        assert_eq!(candidate_order(&addresses, &[]), addresses);
    }
}
