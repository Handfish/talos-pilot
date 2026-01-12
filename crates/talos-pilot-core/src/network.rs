//! Network analysis utilities
//!
//! Provides port-to-service mapping and network connection analysis
//! for Talos Linux and Kubernetes clusters.

/// Well-known service information
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServicePort {
    /// Port number
    pub port: u16,
    /// Service name
    pub name: &'static str,
    /// Short description
    pub description: &'static str,
    /// Whether this is a Talos-specific service
    pub is_talos: bool,
    /// Whether this is a Kubernetes control plane service
    pub is_controlplane: bool,
}

impl ServicePort {
    const fn new(
        port: u16,
        name: &'static str,
        description: &'static str,
        is_talos: bool,
        is_controlplane: bool,
    ) -> Self {
        Self {
            port,
            name,
            description,
            is_talos,
            is_controlplane,
        }
    }
}

/// Well-known Talos and Kubernetes service ports
pub const KNOWN_PORTS: &[ServicePort] = &[
    // Talos services
    ServicePort::new(50000, "apid", "Talos API daemon", true, false),
    ServicePort::new(50001, "trustd", "Talos trust daemon", true, false),
    ServicePort::new(51821, "kubernetesd", "Kubernetes daemon", true, true),

    // etcd
    ServicePort::new(2379, "etcd-client", "etcd client API", false, true),
    ServicePort::new(2380, "etcd-peer", "etcd peer communication", false, true),

    // Kubernetes control plane
    ServicePort::new(6443, "kube-apiserver", "Kubernetes API server", false, true),
    ServicePort::new(10250, "kubelet", "Kubelet API", false, false),
    ServicePort::new(10259, "kube-scheduler", "Kubernetes scheduler", false, true),
    ServicePort::new(10257, "kube-controller-manager", "Controller manager", false, true),

    // Kubernetes networking
    ServicePort::new(10256, "kube-proxy", "Kubernetes proxy", false, false),
    ServicePort::new(8472, "flannel-vxlan", "Flannel VXLAN overlay", false, false),
    ServicePort::new(4240, "cilium-health", "Cilium health check", false, false),
    ServicePort::new(4244, "cilium-hubble", "Cilium Hubble relay", false, false),

    // Common services
    ServicePort::new(53, "dns", "DNS", false, false),
    ServicePort::new(443, "https", "HTTPS", false, false),
    ServicePort::new(80, "http", "HTTP", false, false),
];

/// Get service name for a port
///
/// Returns the service name if the port is a well-known Talos/K8s port.
///
/// # Examples
///
/// ```
/// use talos_pilot_core::network::port_to_service;
///
/// assert_eq!(port_to_service(6443), Some("kube-apiserver"));
/// assert_eq!(port_to_service(50000), Some("apid"));
/// assert_eq!(port_to_service(12345), None);
/// ```
pub fn port_to_service(port: u16) -> Option<&'static str> {
    KNOWN_PORTS.iter()
        .find(|sp| sp.port == port)
        .map(|sp| sp.name)
}

/// Get service name for a u32 port (for compatibility)
pub fn port_to_service_u32(port: u32) -> Option<&'static str> {
    if port > u16::MAX as u32 {
        return None;
    }
    port_to_service(port as u16)
}

/// Get full service info for a port
pub fn get_service_info(port: u16) -> Option<&'static ServicePort> {
    KNOWN_PORTS.iter().find(|sp| sp.port == port)
}

/// Check if a port is a Talos-specific service
pub fn is_talos_port(port: u16) -> bool {
    get_service_info(port).map_or(false, |sp| sp.is_talos)
}

/// Check if a port is a control plane service
pub fn is_controlplane_port(port: u16) -> bool {
    get_service_info(port).map_or(false, |sp| sp.is_controlplane)
}

/// Connection direction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionDirection {
    /// Incoming connection (we're listening)
    Inbound,
    /// Outgoing connection (we initiated)
    Outbound,
    /// Unknown direction
    Unknown,
}

/// Classify a connection by its ports
pub fn classify_connection(local_port: u16, remote_port: u16) -> ConnectionDirection {
    let local_known = port_to_service(local_port).is_some();
    let remote_known = port_to_service(remote_port).is_some();

    if local_known && !remote_known {
        ConnectionDirection::Inbound
    } else if !local_known && remote_known {
        ConnectionDirection::Outbound
    } else {
        ConnectionDirection::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_to_service() {
        assert_eq!(port_to_service(6443), Some("kube-apiserver"));
        assert_eq!(port_to_service(50000), Some("apid"));
        assert_eq!(port_to_service(2379), Some("etcd-client"));
        assert_eq!(port_to_service(12345), None);
    }

    #[test]
    fn test_port_to_service_u32() {
        assert_eq!(port_to_service_u32(6443), Some("kube-apiserver"));
        assert_eq!(port_to_service_u32(70000), None); // Out of u16 range
    }

    #[test]
    fn test_is_talos_port() {
        assert!(is_talos_port(50000)); // apid
        assert!(is_talos_port(50001)); // trustd
        assert!(!is_talos_port(6443)); // kube-apiserver is not Talos-specific
    }

    #[test]
    fn test_is_controlplane_port() {
        assert!(is_controlplane_port(6443)); // kube-apiserver
        assert!(is_controlplane_port(2379)); // etcd-client
        assert!(!is_controlplane_port(10250)); // kubelet runs on all nodes
    }

    #[test]
    fn test_get_service_info() {
        let info = get_service_info(6443).unwrap();
        assert_eq!(info.name, "kube-apiserver");
        assert!(info.is_controlplane);
        assert!(!info.is_talos);
    }

    #[test]
    fn test_classify_connection() {
        // Local is kube-apiserver, remote is ephemeral -> inbound
        assert_eq!(
            classify_connection(6443, 54321),
            ConnectionDirection::Inbound
        );

        // Local is ephemeral, remote is kube-apiserver -> outbound
        assert_eq!(
            classify_connection(54321, 6443),
            ConnectionDirection::Outbound
        );

        // Both unknown -> unknown
        assert_eq!(
            classify_connection(54321, 54322),
            ConnectionDirection::Unknown
        );
    }
}
