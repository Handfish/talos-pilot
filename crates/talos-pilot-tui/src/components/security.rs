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
use std::time::Instant;
use talos_rs::TalosClient;

/// Auto-refresh interval in seconds
const AUTO_REFRESH_INTERVAL_SECS: u64 = 30;

/// Security component for viewing PKI, RBAC, and encryption status
pub struct SecurityComponent {
    /// Context name (cluster identifier)
    context_name: String,

    /// PKI status (certificates)
    pki_status: PkiStatus,

    /// Encryption status
    encryption_status: EncryptionStatus,

    /// Currently selected item index
    selected: usize,

    /// All displayable items (for selection)
    items: Vec<SecurityItem>,

    /// Loading state
    loading: bool,

    /// Error message
    error: Option<String>,

    /// Last refresh time
    last_refresh: Option<Instant>,

    /// Auto-refresh enabled
    auto_refresh: bool,

    /// Client for API calls
    client: Option<TalosClient>,
}

/// A selectable item in the security view
#[derive(Debug, Clone)]
struct SecurityItem {
    /// Item type
    kind: SecurityItemKind,
    /// Display name
    name: String,
    /// Status indicator
    status: ItemStatus,
    /// Status message
    message: String,
    /// Detailed info (shown when selected)
    details: Option<String>,
}

#[derive(Debug, Clone)]
enum SecurityItemKind {
    SectionHeader,
    Certificate,
    Rbac,
    Encryption,
}

#[derive(Debug, Clone, Copy)]
enum ItemStatus {
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
        Self {
            context_name,
            pki_status: PkiStatus::default(),
            encryption_status: EncryptionStatus::default(),
            selected: 0,
            items: Vec::new(),
            loading: true,
            error: None,
            last_refresh: None,
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
        self.error = Some(error);
        self.loading = false;
    }

    /// Refresh security data
    pub async fn refresh(&mut self) -> Result<()> {
        self.loading = true;
        self.error = None;

        // Load PKI status from talosconfig
        self.load_pki_status().await;

        // Load encryption status (if client available)
        if self.client.is_some() {
            self.load_encryption_status().await;
        }

        // Build display items
        self.build_items();

        self.loading = false;
        self.last_refresh = Some(Instant::now());
        Ok(())
    }

    /// Load PKI status from talosconfig and kubeconfig
    async fn load_pki_status(&mut self) {
        let mut pki = PkiStatus::default();

        // Load talosconfig
        match talos_rs::TalosConfig::load_default() {
            Ok(config) => {
                // Get context name
                self.context_name = config.context.clone();

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

        // Load kubeconfig certificate (from API if available)
        if let Some(client) = &self.client {
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
                                        pki.cas.push(cert_info);
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
                                        pki.client_certs.push(cert_info);
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        self.pki_status = pki;
    }

    /// Load encryption status from node via talosctl
    ///
    /// Executes `talosctl get volumestatus` to get encryption info.
    async fn load_encryption_status(&mut self) {
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
            self.encryption_status = EncryptionStatus {
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

                self.encryption_status = EncryptionStatus { volumes };
            }
            Err(e) => {
                tracing::warn!("Failed to get volume status: {}", e);
                self.encryption_status = EncryptionStatus {
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

    /// Build display items from loaded data
    fn build_items(&mut self) {
        self.items.clear();

        // Certificate Authorities section
        self.items.push(SecurityItem {
            kind: SecurityItemKind::SectionHeader,
            name: "Certificate Authorities".to_string(),
            status: ItemStatus::Header,
            message: String::new(),
            details: None,
        });

        for ca in &self.pki_status.cas {
            self.items.push(cert_to_item(ca));
        }

        if self.pki_status.cas.is_empty() {
            self.items.push(SecurityItem {
                kind: SecurityItemKind::Certificate,
                name: "No CAs found".to_string(),
                status: ItemStatus::Info,
                message: "talosconfig may not be loaded".to_string(),
                details: None,
            });
        }

        // Client Certificates section
        self.items.push(SecurityItem {
            kind: SecurityItemKind::SectionHeader,
            name: "Client Certificates".to_string(),
            status: ItemStatus::Header,
            message: String::new(),
            details: None,
        });

        for cert in &self.pki_status.client_certs {
            self.items.push(cert_to_item(cert));
        }

        if self.pki_status.client_certs.is_empty() {
            self.items.push(SecurityItem {
                kind: SecurityItemKind::Certificate,
                name: "No client certs".to_string(),
                status: ItemStatus::Info,
                message: "unavailable".to_string(),
                details: None,
            });
        }

        // RBAC section
        self.items.push(SecurityItem {
            kind: SecurityItemKind::SectionHeader,
            name: "RBAC".to_string(),
            status: ItemStatus::Header,
            message: String::new(),
            details: None,
        });

        let rbac_role = self
            .pki_status
            .rbac_role
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        self.items.push(SecurityItem {
            kind: SecurityItemKind::Rbac,
            name: "Current Role".to_string(),
            status: ItemStatus::Info,
            message: rbac_role.clone(),
            details: Some(format!(
                "Role: {}\nRBAC Enabled: {}\n\nThis role is derived from your talosconfig certificate subject.",
                rbac_role,
                if self.pki_status.rbac_enabled { "Yes" } else { "No" }
            )),
        });

        // Encryption section
        self.items.push(SecurityItem {
            kind: SecurityItemKind::SectionHeader,
            name: "Encryption".to_string(),
            status: ItemStatus::Header,
            message: String::new(),
            details: None,
        });

        for vol in &self.encryption_status.volumes {
            let status = match &vol.provider {
                EncryptionProvider::Tpm | EncryptionProvider::Kms => ItemStatus::Good,
                EncryptionProvider::None => ItemStatus::Warning,
                EncryptionProvider::Static | EncryptionProvider::NodeID => ItemStatus::Warning,
                EncryptionProvider::Unknown(_) => ItemStatus::Info,
            };

            self.items.push(SecurityItem {
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

        // Skip headers when selecting
        if !self.items.is_empty() {
            self.selected = self
                .items
                .iter()
                .position(|i| !matches!(i.kind, SecurityItemKind::SectionHeader))
                .unwrap_or(0);
        }
    }

    /// Move selection up
    fn select_prev(&mut self) {
        if self.items.is_empty() {
            return;
        }

        let mut new_sel = self.selected;
        loop {
            new_sel = if new_sel == 0 {
                self.items.len() - 1
            } else {
                new_sel - 1
            };

            // Skip headers
            if !matches!(self.items[new_sel].kind, SecurityItemKind::SectionHeader) {
                break;
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
        if self.items.is_empty() {
            return;
        }

        let mut new_sel = self.selected;
        loop {
            new_sel = (new_sel + 1) % self.items.len();

            // Skip headers
            if !matches!(self.items[new_sel].kind, SecurityItemKind::SectionHeader) {
                break;
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
        self.items.get(self.selected)
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
            if self.auto_refresh && !self.loading {
                if let Some(last) = self.last_refresh {
                    if last.elapsed().as_secs() >= AUTO_REFRESH_INTERVAL_SECS {
                        return Ok(Some(Action::Refresh));
                    }
                }
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

        // Header
        let context_display = if self.context_name.is_empty() {
            "default".to_string()
        } else {
            self.context_name.clone()
        };

        let (valid, warning, _critical, expired) = self.pki_status.summary();
        let cert_summary = format!(
            "{} valid, {} warning, {} expired",
            valid, warning, expired
        );

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

        // Content - render items
        if let Some(error) = &self.error {
            let error_msg = Paragraph::new(format!("Error: {}", error))
                .style(Style::default().fg(Color::Red));
            frame.render_widget(error_msg, chunks[1]);
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

        let mut lines: Vec<Line> = Vec::new();

        for (i, item) in self.items.iter().enumerate() {
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
