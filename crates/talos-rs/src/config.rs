//! Talos configuration parsing
//!
//! Parses the talosconfig file format used by talosctl.

use crate::error::TalosError;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Talos client configuration (matches talosconfig format)
#[derive(Debug, Clone, Deserialize)]
pub struct TalosConfig {
    /// Current context name
    pub context: String,
    /// Available contexts
    pub contexts: HashMap<String, Context>,
}

/// A single context in the talosconfig
#[derive(Debug, Clone, Deserialize)]
pub struct Context {
    /// API endpoints (e.g., "127.0.0.1:50000")
    pub endpoints: Vec<String>,
    /// Target nodes (optional, defaults to endpoints)
    #[serde(default)]
    pub nodes: Vec<String>,
    /// CA certificate (base64 encoded PEM)
    pub ca: String,
    /// Client certificate (base64 encoded PEM)
    pub crt: String,
    /// Client private key (base64 encoded PEM)
    pub key: String,
}

impl TalosConfig {
    /// Load configuration from the default location (~/.talos/config)
    pub fn load_default() -> Result<Self, TalosError> {
        let path = Self::default_path()?;
        Self::load_from(&path)
    }

    /// Load configuration from a specific path
    pub fn load_from(path: &PathBuf) -> Result<Self, TalosError> {
        if !path.exists() {
            return Err(TalosError::ConfigNotFound(path.display().to_string()));
        }
        let content = std::fs::read_to_string(path)?;
        let config: TalosConfig = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    /// Get the default config path (~/.talos/config)
    pub fn default_path() -> Result<PathBuf, TalosError> {
        let home = dirs_next::home_dir().ok_or(TalosError::NoHomeDirectory)?;
        Ok(home.join(".talos").join("config"))
    }

    /// Get the current context
    pub fn current_context(&self) -> Option<&Context> {
        self.contexts.get(&self.context)
    }

    /// List all context names
    pub fn context_names(&self) -> Vec<&str> {
        self.contexts.keys().map(|s| s.as_str()).collect()
    }

    /// Get a specific context by name
    pub fn get_context(&self, name: &str) -> Result<&Context, TalosError> {
        self.contexts
            .get(name)
            .ok_or_else(|| TalosError::ContextNotFound(name.to_string()))
    }
}

impl Context {
    /// Decode the CA certificate from base64
    pub fn ca_pem(&self) -> Result<Vec<u8>, TalosError> {
        use base64::Engine;
        Ok(base64::engine::general_purpose::STANDARD.decode(&self.ca)?)
    }

    /// Decode the client certificate from base64
    pub fn client_cert_pem(&self) -> Result<Vec<u8>, TalosError> {
        use base64::Engine;
        Ok(base64::engine::general_purpose::STANDARD.decode(&self.crt)?)
    }

    /// Decode the client key from base64
    pub fn client_key_pem(&self) -> Result<Vec<u8>, TalosError> {
        use base64::Engine;
        Ok(base64::engine::general_purpose::STANDARD.decode(&self.key)?)
    }

    /// Get the first endpoint URL
    pub fn endpoint_url(&self) -> Option<String> {
        self.endpoints.first().map(|e| {
            if e.starts_with("https://") || e.starts_with("http://") {
                e.clone()
            } else if e.starts_with('[') || e.contains("::") || e.matches(':').count() > 1 {
                // IPv6 address (with brackets or raw)
                format!("https://{}", e)
            } else if e.contains(':') {
                // Has port specified
                format!("https://{}", e)
            } else {
                // Add default Talos API port
                format!("https://{}:50000", e)
            }
        })
    }

    /// Get target nodes, falling back to endpoints if not specified
    pub fn target_nodes(&self) -> &[String] {
        if self.nodes.is_empty() {
            &self.endpoints
        } else {
            &self.nodes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config() {
        let yaml = r#"
context: default
contexts:
  default:
    endpoints:
      - 192.168.1.100:50000
    ca: Y2EtY2VydA==
    crt: Y2xpZW50LWNlcnQ=
    key: Y2xpZW50LWtleQ==
"#;
        let config: TalosConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.context, "default");
        assert_eq!(config.contexts.len(), 1);

        let ctx = config.current_context().unwrap();
        assert_eq!(ctx.endpoints, vec!["192.168.1.100:50000"]);
    }

    #[test]
    fn test_context_names() {
        let yaml = r#"
context: prod
contexts:
  dev:
    endpoints: ["dev.example.com:50000"]
    ca: YQ==
    crt: Yg==
    key: Yw==
  prod:
    endpoints: ["prod.example.com:50000"]
    ca: ZA==
    crt: ZQ==
    key: Zg==
"#;
        let config: TalosConfig = serde_yaml::from_str(yaml).unwrap();
        let names = config.context_names();
        assert!(names.contains(&"dev"));
        assert!(names.contains(&"prod"));
    }

    #[test]
    fn test_endpoint_url() {
        // Test endpoint with explicit port
        let ctx = Context {
            endpoints: vec!["192.168.1.100:50000".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(
            ctx.endpoint_url(),
            Some("https://192.168.1.100:50000".to_string())
        );

        let ctx2 = Context {
            endpoints: vec!["kharkiv".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(
            ctx2.endpoint_url(),
            Some("https://kharkiv:50000".to_string())
        );

        let ctx3 = Context {
            endpoints: vec!["https://192.168.1.100:50000".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(
            ctx3.endpoint_url(),
            Some("https://192.168.1.100:50000".to_string())
        );
    }

    #[test]
    fn test_endpoint_url_with_ipv6() {
        let ctx = Context {
            endpoints: vec!["[2a01:e0a:e4b:aa30::1]:50000".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(
            ctx.endpoint_url(),
            Some("https://[2a01:e0a:e4b:aa30::1]:50000".to_string())
        );

        let ctx2 = Context {
            endpoints: vec!["2a01:e0a:e4b:aa30::1".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(
            ctx2.endpoint_url(),
            Some("https://2a01:e0a:e4b:aa30::1".to_string())
        );

        let ctx3 = Context {
            endpoints: vec!["[::1]:50000".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(ctx3.endpoint_url(), Some("https://[::1]:50000".to_string()));
    }

    #[test]
    fn test_endpoint_url_with_custom_port() {
        let ctx = Context {
            endpoints: vec!["192.168.1.100:443".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(
            ctx.endpoint_url(),
            Some("https://192.168.1.100:443".to_string())
        );
    }

    #[test]
    fn test_endpoint_url_with_hostname() {
        let ctx = Context {
            endpoints: vec!["talos.example.com".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(
            ctx.endpoint_url(),
            Some("https://talos.example.com:50000".to_string())
        );

        let ctx2 = Context {
            endpoints: vec!["talos.example.com:8443".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(
            ctx2.endpoint_url(),
            Some("https://talos.example.com:8443".to_string())
        );
    }

    #[test]
    fn test_target_nodes_fallback() {
        let ctx = Context {
            endpoints: vec!["192.168.1.100:50000".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(ctx.target_nodes(), &["192.168.1.100:50000"]);

        let ctx2 = Context {
            endpoints: vec!["127.0.0.1:50000".to_string()],
            nodes: vec!["192.168.1.101".to_string(), "192.168.1.102".to_string()],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(ctx2.target_nodes(), &["192.168.1.101", "192.168.1.102"]);
    }

    #[test]
    fn test_empty_endpoints() {
        let ctx = Context {
            endpoints: vec![],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(ctx.endpoint_url(), None);
    }
}
