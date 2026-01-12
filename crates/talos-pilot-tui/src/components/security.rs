//! Security component - displays certificate status, RBAC, and encryption info
//!
//! Provides a consolidated view of security-related cluster status.

use crate::action::Action;
use crate::components::diagnostics::pki::{
    self, CertStatus, CertificateInfo, EncryptionProvider, EncryptionStatus, PkiStatus,
    VolumeEncryption,
};
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::time::Duration;
use talos_pilot_core::AsyncState;
use talos_rs::TalosClient;

/// Auto-refresh interval in seconds
const AUTO_REFRESH_INTERVAL_SECS: u64 = 30;

/// Loaded security data (wrapped by AsyncState)
#[derive(Debug, Clone, Default)]
pub struct SecurityData {
    /// Context name (cluster identifier)
    pub context_name: String,
    /// PKI status (certificates)
    pub pki_status: PkiStatus,
    /// Encryption status
    pub encryption_status: EncryptionStatus,
    /// All displayable items (for selection)
    pub items: Vec<SecurityItem>,
}

/// Security component for viewing PKI, RBAC, and encryption status
pub struct SecurityComponent {
    /// Async state wrapping all security data
    state: AsyncState<SecurityData>,

    /// Currently selected item index
    selected: usize,

    /// Auto-refresh enabled
    auto_refresh: bool,

    /// Client for API calls
    client: Option<TalosClient>,
}

/// A selectable item in the security view
#[derive(Debug, Clone)]
pub struct SecurityItem {
    /// Item type
    pub kind: SecurityItemKind,
    /// Display name
    pub name: String,
    /// Status indicator
    pub status: ItemStatus,
    /// Status message
    pub message: String,
    /// Detailed info (shown when selected)
    pub details: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityItemKind {
    SectionHeader,
    Certificate,
    Rbac,
    Encryption,
}

#[derive(Debug, Clone, Copy)]
pub enum ItemStatus {
    Good,
    Warning,
    Critical,
    Info,
    Header,
}

impl ItemStatus {
    fn indicator(&self) -> (&'static str, Color) {
        match self {
            ItemStatus::Good => ("●", Color::Green),
            ItemStatus::Warning => ("⚠", Color::Yellow),
            ItemStatus::Critical => ("!", Color::Red),
            ItemStatus::Info => ("○", Color::Cyan),
            ItemStatus::Header => ("", Color::Cyan),
        }
    }
}

impl Default for SecurityComponent {
    fn default() -> Self {
        Self::new("".to_string())
    }
}

impl SecurityComponent {
    pub fn new(context_name: String) -> Self {
        // Initialize with context name in the data
        let initial_data = SecurityData {
            context_name,
            ..Default::default()
        };
        let mut state = AsyncState::new();
        state.set_data(initial_data);

        Self {
            state,
            selected: 0,
            auto_refresh: true,
            client: None,
        }
    }

    /// Set the client for API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Helper to get data reference
    fn data(&self) -> Option<&SecurityData> {
        self.state.data()
    }

    /// Helper to get mutable data reference
    fn data_mut(&mut self) -> Option<&mut SecurityData> {
        self.state.data_mut()
    }

    /// Refresh security data
    pub async fn refresh(&mut self) -> Result<()> {
        self.state.start_loading();

        // Get or create data
        let mut data = self.state.take_data().unwrap_or_default();

        // Load PKI status from talosconfig
        Self::load_pki_status_into(&mut data).await;

        // Load kubeconfig certs and encryption status (if client available)
        if let Some(client) = &self.client {
            Self::load_kubeconfig_certs(&mut data, client).await;
            Self::load_encryption_status_into(&mut data, client).await;
        }

        // Build display items from loaded data
        Self::build_items_into(&mut data);

        // Set initial selection to first non-header item
        if !data.items.is_empty() {
            self.selected = data
                .items
                .iter()
                .position(|i| i.kind != SecurityItemKind::SectionHeader)
                .unwrap_or(0);
        }

        // Store the data
        self.state.set_data(data);
        Ok(())
    }

    /// Load PKI status from talosconfig (static method)
    async fn load_pki_status_into(data: &mut SecurityData) {
        let mut pki = PkiStatus::default();

        // Load talosconfig
        match talos_rs::TalosConfig::load_default() {
            Ok(config) => {
                // Get context name
                data.context_name = config.context.clone();

                if let Some(context) = config.current_context() {
                    // Parse client certificate
                    if let Ok(pem_data) = context.client_cert_pem() {
                        if let Ok(cert_info) = pki::parse_certificate("talosconfig", &pem_data) {
                            // Extract RBAC role from subject
                            // Subject format is like "O=os:admin" - extract just the role part
                            let role = cert_info.subject
                                .strip_prefix("O=")
                                .or_else(|| cert_info.subject.strip_prefix("CN="))
                                .unwrap_or(&cert_info.subject)
                                .to_string();
                            pki.rbac_role = Some(role);
                            pki.rbac_enabled = true;
                            pki.client_certs.push(cert_info);
                        }
                    }

                    // Parse CA certificate
                    if let Ok(pem_data) = context.ca_pem() {
                        if let Ok(cert_info) = pki::parse_certificate("Talos CA", &pem_data) {
                            pki.cas.push(cert_info);
                        }
                    }
                }
            }
            Err(e) => {
                pki.error = Some(format!("Failed to load talosconfig: {}", e));
            }
        }

        data.pki_status = pki;
    }

    /// Load kubeconfig certificates via client (static method)
    async fn load_kubeconfig_certs(data: &mut SecurityData, client: &TalosClient) {
        if let Ok(kubeconfig_yaml) = client.kubeconfig().await {
            if let Ok(kc) = serde_yaml::from_str::<serde_yaml::Value>(&kubeconfig_yaml) {
                // Parse kubeconfig CA
                if let Some(clusters) = kc.get("clusters").and_then(|c| c.as_sequence()) {
                    for cluster in clusters {
                        if let Some(cluster_data) = cluster.get("cluster") {
                            if let Some(ca_data) = cluster_data
                                .get("certificate-authority-data")
                                .and_then(|c| c.as_str())
                            {
                                if let Ok(cert_info) =
                                    pki::parse_base64_certificate("Kubernetes CA", ca_data)
                                {
                                    data.pki_status.cas.push(cert_info);
                                }
                            }
                        }
                    }
                }

                // Parse kubeconfig client cert
                if let Some(users) = kc.get("users").and_then(|u| u.as_sequence()) {
                    for user in users {
                        if let Some(user_data) = user.get("user") {
                            if let Some(cert_data) = user_data
                                .get("client-certificate-data")
                                .and_then(|c| c.as_str())
                            {
                                if let Ok(cert_info) =
                                    pki::parse_base64_certificate("kubeconfig", cert_data)
                                {
                                    data.pki_status.client_certs.push(cert_info);
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Load encryption status from node via talosctl (static method)
    async fn load_encryption_status_into(data: &mut SecurityData, _client: &TalosClient) {
        // Get the first node from talosconfig to query
        let node = match talos_rs::TalosConfig::load_default() {
            Ok(config) => {
                config.current_context()
                    .and_then(|ctx| {
                        // Prefer nodes if set, otherwise use first endpoint
                        if !ctx.nodes.is_empty() {
                            ctx.nodes.first().map(|n| n.split(':').next().unwrap_or(n).to_string())
                        } else {
                            ctx.endpoints.first().map(|e| e.split(':').next().unwrap_or(e).to_string())
                        }
                    })
            }
            Err(_) => None,
        };

        let Some(node) = node else {
            data.encryption_status = EncryptionStatus {
                volumes: vec![
                    VolumeEncryption {
                        name: "STATE".to_string(),
                        provider: EncryptionProvider::Unknown("no node configured".to_string()),
                    },
                    VolumeEncryption {
                        name: "EPHEMERAL".to_string(),
                        provider: EncryptionProvider::Unknown("no node configured".to_string()),
                    },
                ],
            };
            return;
        };

        // Execute talosctl get volumestatus
        match talos_rs::get_volume_status(&node) {
            Ok(statuses) => {
                let mut volumes = Vec::new();

                for status in statuses {
                    // Only include main volumes
                    if !status.id.contains("STATE") && !status.id.contains("EPHEMERAL") {
                        continue;
                    }

                    let provider = match status.encryption_provider.as_deref() {
                        Some("luks2") | Some("LUKS2") => EncryptionProvider::Static,
                        Some(p) if p.to_lowercase().contains("tpm") => EncryptionProvider::Tpm,
                        Some(p) if p.to_lowercase().contains("kms") => EncryptionProvider::Kms,
                        Some(_) => EncryptionProvider::Static,
                        None => EncryptionProvider::None,
                    };

                    volumes.push(VolumeEncryption {
                        name: status.id,
                        provider,
                    });
                }

                // If no main volumes found, add defaults
                if volumes.is_empty() {
                    volumes.push(VolumeEncryption {
                        name: "STATE".to_string(),
                        provider: EncryptionProvider::Unknown("not found".to_string()),
                    });
                    volumes.push(VolumeEncryption {
                        name: "EPHEMERAL".to_string(),
                        provider: EncryptionProvider::Unknown("not found".to_string()),
                    });
                }

                data.encryption_status = EncryptionStatus { volumes };
            }
            Err(e) => {
                tracing::warn!("Failed to get volume status: {}", e);
                data.encryption_status = EncryptionStatus {
                    volumes: vec![
                        VolumeEncryption {
                            name: "STATE".to_string(),
                            provider: EncryptionProvider::Unknown(format!("{}", e)),
                        },
                        VolumeEncryption {
                            name: "EPHEMERAL".to_string(),
                            provider: EncryptionProvider::Unknown(format!("{}", e)),
                        },
                    ],
                };
            }
        }
    }

    /// Build display items from loaded data (static method)
    fn build_items_into(data: &mut SecurityData) {
        data.items.clear();

        // Certificate Authorities section
        data.items.push(SecurityItem {
            kind: SecurityItemKind::SectionHeader,
            name: "Certificate Authorities".to_string(),
            status: ItemStatus::Header,
            message: String::new(),
            details: None,
        });

        for ca in &data.pki_status.cas {
            data.items.push(cert_to_item(ca));
        }

        if data.pki_status.cas.is_empty() {
            data.items.push(SecurityItem {
                kind: SecurityItemKind::Certificate,
                name: "No CAs found".to_string(),
                status: ItemStatus::Info,
                message: "talosconfig may not be loaded".to_string(),
                details: None,
            });
        }

        // Client Certificates section
        data.items.push(SecurityItem {
            kind: SecurityItemKind::SectionHeader,
            name: "Client Certificates".to_string(),
            status: ItemStatus::Header,
            message: String::new(),
            details: None,
        });

        for cert in &data.pki_status.client_certs {
            data.items.push(cert_to_item(cert));
        }

        if data.pki_status.client_certs.is_empty() {
            data.items.push(SecurityItem {
                kind: SecurityItemKind::Certificate,
                name: "No client certs".to_string(),
                status: ItemStatus::Info,
                message: "unavailable".to_string(),
                details: None,
            });
        }

        // RBAC section
        data.items.push(SecurityItem {
            kind: SecurityItemKind::SectionHeader,
            name: "RBAC".to_string(),
            status: ItemStatus::Header,
            message: String::new(),
            details: None,
        });

        let rbac_role = data
            .pki_status
            .rbac_role
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        data.items.push(SecurityItem {
            kind: SecurityItemKind::Rbac,
            name: "Current Role".to_string(),
            status: ItemStatus::Info,
            message: rbac_role.clone(),
            details: Some(format!(
                "Role: {}\nRBAC Enabled: {}\n\nThis role is derived from your talosconfig certificate subject.",
                rbac_role,
                if data.pki_status.rbac_enabled { "Yes" } else { "No" }
            )),
        });

        // Encryption section
        data.items.push(SecurityItem {
            kind: SecurityItemKind::SectionHeader,
            name: "Encryption".to_string(),
            status: ItemStatus::Header,
            message: String::new(),
            details: None,
        });

        for vol in &data.encryption_status.volumes {
            let status = match &vol.provider {
                EncryptionProvider::Tpm | EncryptionProvider::Kms => ItemStatus::Good,
                EncryptionProvider::None => ItemStatus::Warning,
                EncryptionProvider::Static | EncryptionProvider::NodeID => ItemStatus::Warning,
                EncryptionProvider::Unknown(_) => ItemStatus::Info,
            };

            data.items.push(SecurityItem {
                kind: SecurityItemKind::Encryption,
                name: format!("{} partition", vol.name),
                status,
                message: format!("{} ({})", vol.provider.name(), vol.provider.strength()),
                details: Some(format!(
                    "Volume: {}\nProvider: {}\nStrength: {}\n\n{}",
                    vol.name,
                    vol.provider.name(),
                    vol.provider.strength(),
                    match &vol.provider {
                        EncryptionProvider::None => "No encryption configured for this volume.",
                        EncryptionProvider::Static => "Static key stored in config. Consider using TPM for better security.",
                        EncryptionProvider::NodeID => "Key derived from node UUID. Provides minimal protection.",
                        EncryptionProvider::Tpm => "Key sealed to TPM. Provides strong hardware-backed encryption.",
                        EncryptionProvider::Kms => "Key managed by external KMS. Provides strong enterprise encryption.",
                        EncryptionProvider::Unknown(_) => "Encryption status requires querying the node.\nRun: talosctl get volumestatus -n <node-ip>\n\nThis will be automated in a future release.",
                    }
                )),
            });
        }
    }

    /// Move selection up
    fn select_prev(&mut self) {
        let Some(data) = self.data() else {
            return;
        };
        if data.items.is_empty() {
            return;
        }

        let items_len = data.items.len();
        let mut new_sel = self.selected;
        loop {
            new_sel = if new_sel == 0 {
                items_len - 1
            } else {
                new_sel - 1
            };

            // Skip headers (need to re-borrow data)
            if let Some(d) = self.data() {
                if d.items[new_sel].kind != SecurityItemKind::SectionHeader {
                    break;
                }
            }

            // Prevent infinite loop
            if new_sel == self.selected {
                break;
            }
        }
        self.selected = new_sel;
    }

    /// Move selection down
    fn select_next(&mut self) {
        let Some(data) = self.data() else {
            return;
        };
        if data.items.is_empty() {
            return;
        }

        let items_len = data.items.len();
        let mut new_sel = self.selected;
        loop {
            new_sel = (new_sel + 1) % items_len;

            // Skip headers (need to re-borrow data)
            if let Some(d) = self.data() {
                if d.items[new_sel].kind != SecurityItemKind::SectionHeader {
                    break;
                }
            }

            // Prevent infinite loop
            if new_sel == self.selected {
                break;
            }
        }
        self.selected = new_sel;
    }

    /// Get currently selected item
    fn selected_item(&self) -> Option<&SecurityItem> {
        self.data().and_then(|d| d.items.get(self.selected))
    }
}

/// Convert a CertificateInfo to a SecurityItem
fn cert_to_item(cert: &CertificateInfo) -> SecurityItem {
    let status = match cert.status {
        CertStatus::Valid => ItemStatus::Good,
        CertStatus::Warning => ItemStatus::Warning,
        CertStatus::Critical | CertStatus::Expired => ItemStatus::Critical,
    };

    let message = if cert.days_remaining <= 0 {
        format!(
            "EXPIRED {} ago",
            cert.time_remaining.replace("expired ", "")
        )
    } else {
        format!("Valid for {}", cert.time_remaining)
    };

    SecurityItem {
        kind: SecurityItemKind::Certificate,
        name: cert.name.clone(),
        status,
        message,
        details: Some(format!(
            "Subject: {}\nIssuer: {}\nNot Before: {}\nNot After: {}\nDays Remaining: {}\nIs CA: {}",
            cert.subject,
            cert.issuer,
            cert.not_before.format("%Y-%m-%d %H:%M:%S UTC"),
            cert.not_after.format("%Y-%m-%d %H:%M:%S UTC"),
            cert.days_remaining,
            if cert.is_ca { "Yes" } else { "No" }
        )),
    }
}

impl Component for SecurityComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Back)),
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_prev();
                Ok(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_next();
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Check for auto-refresh using AsyncState helper
            let interval = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
            if self.state.should_auto_refresh(self.auto_refresh, interval) {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        let chunks = Layout::vertical([
            Constraint::Length(3), // Header
            Constraint::Fill(1),   // Content
            Constraint::Length(5), // Details
            Constraint::Length(2), // Footer
        ])
        .split(area);

        // Get data for rendering (default values if not yet loaded)
        let (context_display, cert_summary) = if let Some(data) = self.data() {
            let context = if data.context_name.is_empty() {
                "default".to_string()
            } else {
                data.context_name.clone()
            };
            let (valid, warning, _critical, expired) = data.pki_status.summary();
            let summary = format!("{} valid, {} warning, {} expired", valid, warning, expired);
            (context, summary)
        } else {
            ("default".to_string(), "loading...".to_string())
        };

        // Header
        let header = Paragraph::new(Line::from(vec![
            Span::styled(" Security ", Style::default().bold().fg(Color::Cyan)),
            Span::raw("─ "),
            Span::styled(&context_display, Style::default().fg(Color::Yellow)),
            Span::raw(" ─ "),
            Span::styled(cert_summary, Style::default().dim()),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(header, chunks[0]);

        // Content - render items or error/loading state
        if self.state.is_loading() && !self.state.has_data() {
            let loading = Paragraph::new("Loading...")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, chunks[1]);
        } else if let Some(error) = self.state.error() {
            if !self.state.has_data() {
                let error_msg = Paragraph::new(format!("Error: {}", error))
                    .style(Style::default().fg(Color::Red));
                frame.render_widget(error_msg, chunks[1]);
            } else {
                self.render_items(frame, chunks[1]);
            }
        } else {
            self.render_items(frame, chunks[1]);
        }

        // Details panel
        self.render_details(frame, chunks[2]);

        // Footer
        let footer = Paragraph::new(Line::from(vec![
            Span::styled("[↑↓]", Style::default().fg(Color::Yellow)),
            Span::raw(" Navigate").dim(),
            Span::raw("  "),
            Span::styled("[r]", Style::default().fg(Color::Yellow)),
            Span::raw(" Refresh").dim(),
            Span::raw("  "),
            Span::styled("[q]", Style::default().fg(Color::Yellow)),
            Span::raw(" Back").dim(),
        ]))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(footer, chunks[3]);

        Ok(())
    }
}

impl SecurityComponent {
    fn render_items(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Certificate Status ")
            .title_style(Style::default().fg(Color::Cyan).bold())
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(data) = self.data() else {
            return;
        };

        let mut lines: Vec<Line> = Vec::new();

        for (i, item) in data.items.iter().enumerate() {
            let is_selected = i == self.selected;

            let line = match item.kind {
                SecurityItemKind::SectionHeader => {
                    // Section header - blank line before (except first)
                    if !lines.is_empty() {
                        lines.push(Line::from(""));
                    }
                    Line::from(Span::styled(
                        &item.name,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ))
                }
                _ => {
                    let (indicator, color) = item.status.indicator();
                    let style = if is_selected {
                        Style::default().bg(Color::DarkGray)
                    } else {
                        Style::default()
                    };

                    // Format: "├─ Name .......... Message"
                    let prefix = if is_selected { "►" } else { "├" };
                    let name_width = 18;
                    let padded_name = format!("{:.<width$}", item.name, width = name_width);

                    Line::from(vec![
                        Span::styled(format!("{} ", prefix), Style::default().fg(Color::DarkGray)),
                        Span::styled(indicator, Style::default().fg(color)),
                        Span::raw(" "),
                        Span::styled(padded_name, style),
                        Span::raw(" "),
                        Span::styled(&item.message, Style::default().fg(color)),
                    ])
                }
            };

            lines.push(line);
        }

        let content = Paragraph::new(lines);
        frame.render_widget(content, inner);
    }

    fn render_details(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Details ")
            .title_style(Style::default().fg(Color::Cyan).bold())
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if let Some(item) = self.selected_item() {
            if let Some(details) = &item.details {
                let lines: Vec<Line> = details
                    .lines()
                    .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::White))))
                    .collect();
                let content = Paragraph::new(lines);
                frame.render_widget(content, inner);
            }
        }
    }
}
