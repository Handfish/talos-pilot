//! Home component - main dashboard view

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

/// Home component showing the main dashboard
#[allow(dead_code)]
pub struct HomeComponent {
    /// Whether the component is active
    active: bool,
}

impl Default for HomeComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl HomeComponent {
    pub fn new() -> Self {
        Self { active: true }
    }
}

impl Component for HomeComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Quit)),
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Handle tick for animations
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        let layout = Layout::vertical([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Content
            Constraint::Length(3), // Footer
        ])
        .split(area);

        // Header
        let header = Paragraph::new(Line::from(vec![
            Span::raw(" talos-pilot ").bold().fg(Color::Cyan),
            Span::raw("v0.1.0").dim(),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(header, layout[0]);

        // Content - placeholder for now
        let content = Paragraph::new(vec![
            Line::from(""),
            Line::from(
                Span::raw("Welcome to talos-pilot")
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from(""),
            Line::from(Span::raw("A terminal UI for managing Talos Linux clusters").dim()),
            Line::from(""),
            Line::from(""),
            Line::from(vec![
                Span::raw("  Press ").dim(),
                Span::raw("r").fg(Color::Yellow),
                Span::raw(" to refresh").dim(),
            ]),
            Line::from(vec![
                Span::raw("  Press ").dim(),
                Span::raw("q").fg(Color::Yellow),
                Span::raw(" to quit").dim(),
            ]),
        ])
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::NONE));
        frame.render_widget(content, layout[1]);

        // Footer
        let footer = Paragraph::new(Line::from(vec![
            Span::raw(" [q]").fg(Color::Yellow),
            Span::raw(" quit").dim(),
            Span::raw("  "),
            Span::raw("[r]").fg(Color::Yellow),
            Span::raw(" refresh").dim(),
        ]))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(footer, layout[2]);

        Ok(())
    }
}
