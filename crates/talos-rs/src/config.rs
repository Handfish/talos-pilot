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
        tracing::debug!("Loading talosconfig from: {:?}", path);
        Self::load_from(&path)
    }

    /// Load configuration from a specific path
    pub fn load_from(path: &PathBuf) -> Result<Self, TalosError> {
        tracing::debug!("load_from called with path: {:?}", path);
        if !path.exists() {
            return Err(TalosError::ConfigNotFound(path.display().to_string()));
        }
        let content = std::fs::read_to_string(path)?;
        let config: TalosConfig = serde_yaml::from_str(&content)?;
        tracing::debug!(
            "Loaded config with {} contexts: {:?}",
            config.contexts.len(),
            config.contexts.keys().collect::<Vec<_>>()
        );
        Ok(config)
    }

    /// Get the default config path (from $TALOSCONFIG env var or ~/.talos/config)
    pub fn default_path() -> Result<PathBuf, TalosError> {
        // Check if TALOSCONFIG environment variable is set
        if let Ok(talosconfig) = std::env::var("TALOSCONFIG") {
            tracing::debug!("TALOSCONFIG env var found: {}", talosconfig);
            return Ok(PathBuf::from(talosconfig));
        }

        tracing::debug!("TALOSCONFIG env var not set, using default path");
        // Fallback to default location
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
            } else if e.starts_with('[') {
                // IPv6 address with brackets - check if port is specified
                if e.contains("]:") {
                    // Has port specified: [::1]:50000
                    format!("https://{}", e)
                } else {
                    // No port: [::1] -> add default port
                    format!("https://{}:50000", e)
                }
            } else if e.contains("::") || e.matches(':').count() > 1 {
                // Raw IPv6 address without brackets - add brackets and default port
                format!("https://[{}]:50000", e)
            } else if e.contains(':') {
                // IPv4 or hostname with port specified
                format!("https://{}", e)
            } else {
                // IPv4 or hostname without port - add default Talos API port
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
        // IPv6 with brackets and port
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

        // Raw IPv6 without brackets - should add brackets and default port
        let ctx2 = Context {
            endpoints: vec!["2a01:e0a:e4b:aa30::1".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(
            ctx2.endpoint_url(),
            Some("https://[2a01:e0a:e4b:aa30::1]:50000".to_string())
        );

        // IPv6 with brackets and port
        let ctx3 = Context {
            endpoints: vec!["[::1]:50000".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(ctx3.endpoint_url(), Some("https://[::1]:50000".to_string()));

        // IPv6 with brackets but no port - should add default port
        let ctx4 = Context {
            endpoints: vec!["[::1]".to_string()],
            nodes: vec![],
            ca: "YQ==".to_string(),
            crt: "Yg==".to_string(),
            key: "Yw==".to_string(),
        };
        assert_eq!(ctx4.endpoint_url(), Some("https://[::1]:50000".to_string()));
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

    // Note: These tests manipulate environment variables and must run sequentially.
    // They are combined into a single test to avoid race conditions in parallel execution.
    #[test]
    fn test_default_path_env_handling() {
        use std::sync::Mutex;
        static ENV_MUTEX: Mutex<()> = Mutex::new(());

        let _guard = ENV_MUTEX.lock().unwrap();

        // Test 1: With env var set
        unsafe {
            std::env::set_var("TALOSCONFIG", "/custom/path/to/talosconfig");
        }
        let path = TalosConfig::default_path().unwrap();
        assert_eq!(path, PathBuf::from("/custom/path/to/talosconfig"));

        // Test 2: Env var takes precedence (different path)
        let custom_path = "/tmp/my-talos-config.yaml";
        unsafe {
            std::env::set_var("TALOSCONFIG", custom_path);
        }
        let path = TalosConfig::default_path().unwrap();
        assert_eq!(path, PathBuf::from(custom_path));
        assert!(!path.to_string_lossy().contains(".talos"));

        // Test 3: Without env var - falls back to default
        unsafe {
            std::env::remove_var("TALOSCONFIG");
        }
        let path = TalosConfig::default_path().unwrap();
        let home = dirs_next::home_dir().unwrap();
        let expected = home.join(".talos").join("config");
        assert_eq!(path, expected);
    }
}
