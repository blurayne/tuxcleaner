use std::io::stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Clean,
    Analyze,
    Purge,
    Status,
    History,
    Update,
}

const MENU: &[(MenuAction, &str, &str)] = &[
    (
        MenuAction::Clean,
        "Clean",
        "Review known package, application, and developer caches",
    ),
    (
        MenuAction::Analyze,
        "Analyze",
        "Inspect disk usage and list large files safely",
    ),
    (
        MenuAction::Purge,
        "Purge",
        "Find old project build artifacts",
    ),
    (
        MenuAction::Status,
        "Status",
        "Show CPU, memory, disk, and uptime information",
    ),
    (
        MenuAction::History,
        "History",
        "Review previous cleanup operations",
    ),
    (
        MenuAction::Update,
        "Update",
        "Install a checksum-verified GitHub release",
    ),
];

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
}

pub fn interactive_menu() -> Result<Option<MenuAction>> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = ListState::default().with_selected(Some(0));

    loop {
        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(4),
                    Constraint::Min(8),
                    Constraint::Length(3),
                ])
                .split(frame.area());
            let title = Paragraph::new(vec![
                Line::from(Span::styled(
                    "TuxCleaner",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("Safety-first Linux maintenance"),
            ])
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::BOTTOM));
            frame.render_widget(title, chunks[0]);

            let items: Vec<_> = MENU
                .iter()
                .map(|(_, title, detail)| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{title:<10}"),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(*detail),
                    ]))
                })
                .collect();
            let list = List::new(items)
                .block(Block::default().title(" Actions ").borders(Borders::ALL))
                .highlight_symbol("▶ ")
                .highlight_style(Style::default().fg(Color::Yellow));
            frame.render_stateful_widget(list, chunks[1], &mut state);

            let help = Paragraph::new("↑/↓ or j/k: move   Enter: select   q/Esc: quit")
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::TOP));
            frame.render_widget(help, chunks[2]);
        })?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let current = state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                state.select(Some(current.saturating_sub(1)));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.select(Some((current + 1).min(MENU.len() - 1)));
            }
            KeyCode::Enter => {
                terminal.show_cursor()?;
                return Ok(Some(MENU[current].0));
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                terminal.show_cursor()?;
                return Ok(None);
            }
            _ => {}
        }
    }
}
