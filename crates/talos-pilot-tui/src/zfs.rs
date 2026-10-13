//! ZFS pool capacity per node
//!
//! Talos can't run `zpool list`, and a pool's root dataset usually isn't
//! mounted, so the Mounts API only shows quota-limited datasets (PVCs) whose
//! sizes say nothing about the pool. OpenEBS ZFS LocalPV publishes each node's
//! pools with free/used space as a `ZFSNode` resource; we read those.
//! Clusters without OpenEBS ZFS simply report no pools.

use k8s_openapi::serde_json::Value;
use kube::Client;
use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind, ListParams};
use std::collections::HashMap;

/// Capacity of one ZFS pool (bytes)
#[derive(Debug, Clone, PartialEq)]
pub struct ZfsPool {
    pub name: String,
    pub free: u64,
    pub used: u64,
}

impl ZfsPool {
    pub fn size(&self) -> u64 {
        self.free + self.used
    }

    /// Percentage of the pool in use, `None` for an empty pool
    pub fn used_pct(&self) -> Option<f32> {
        let size = self.size();
        (size > 0).then(|| self.used as f32 / size as f32 * 100.0)
    }
}

/// ZFS pools keyed by Kubernetes node name.
///
/// Returns an empty map when the `ZFSNode` CRD isn't installed.
pub async fn fetch_zfs_pools(
    client: &Client,
) -> Result<HashMap<String, Vec<ZfsPool>>, kube::Error> {
    let gvk = GroupVersionKind::gvk("zfs.openebs.io", "v1", "ZFSNode");
    let resource = ApiResource::from_gvk_with_plural(&gvk, "zfsnodes");
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &resource);

    let list = match api.list(&ListParams::default()).await {
        Ok(list) => list,
        Err(kube::Error::Api(e)) if e.code == 404 => return Ok(HashMap::new()),
        Err(e) => return Err(e),
    };

    Ok(list
        .items
        .into_iter()
        .filter_map(|obj| {
            let node = obj.metadata.name?;
            let pools = pools_from(obj.data.get("pools")?);
            Some((node, pools))
        })
        .collect())
}

/// Parse the `pools` array of a ZFSNode, skipping entries without sizes
fn pools_from(pools: &Value) -> Vec<ZfsPool> {
    pools
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some(ZfsPool {
                name: p.get("name")?.as_str()?.to_string(),
                free: parse_quantity(p.get("free")?.as_str()?)?,
                used: parse_quantity(p.get("used")?.as_str()?)?,
            })
        })
        .collect()
}

/// Parse a Kubernetes quantity such as `838259748Ki` or `1G` into bytes
fn parse_quantity(q: &str) -> Option<u64> {
    let q = q.trim();
    let split = q
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(q.len());
    let (number, suffix) = q.split_at(split);
    let multiplier: u64 = match suffix {
        "" => 1,
        "Ki" => 1 << 10,
        "Mi" => 1 << 20,
        "Gi" => 1 << 30,
        "Ti" => 1 << 40,
        "Pi" => 1 << 50,
        "k" => 1_000,
        "M" => 1_000_000,
        "G" => 1_000_000_000,
        "T" => 1_000_000_000_000,
        "P" => 1_000_000_000_000_000,
        _ => return None,
    };
    let value: f64 = number.parse().ok()?;
    Some((value * multiplier as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::serde_json::json;

    #[test]
    fn quantities() {
        assert_eq!(parse_quantity("838259748Ki"), Some(838_259_748 * 1024));
        assert_eq!(parse_quantity("1.5Gi"), Some(1_610_612_736));
        assert_eq!(parse_quantity("2G"), Some(2_000_000_000));
        assert_eq!(parse_quantity("512"), Some(512));
        assert_eq!(parse_quantity("12Xi"), None);
        assert_eq!(parse_quantity(""), None);
    }

    #[test]
    fn zfsnode_pools() {
        let pools = json!([
            {"name": "datapool", "free": "3Gi", "used": "1Gi", "uuid": "7"},
            {"name": "broken", "uuid": "8"}
        ]);
        let parsed = pools_from(&pools);
        assert_eq!(
            parsed,
            [ZfsPool {
                name: "datapool".to_string(),
                free: 3 << 30,
                used: 1 << 30,
            }]
        );
        assert_eq!(parsed[0].used_pct(), Some(25.0));
    }
}
