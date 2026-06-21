//! Minimal ratatui-based REPL UI.
//!
//! Layout:
//!   ┌──────────────────────────────┐
//!   │ transcript (scrollback)       │
//!   ├──────────────────────────────┤
//!   │ > prompt input                │
//!   └──────────────────────────────┘
//!
//! Keys: Enter submits, Esc/Ctrl-C quits.

use crate::app::App;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use std::io::Stdout;

struct UiState {
    input: String,
    scrollback: Vec<Line<'static>>,
    busy: bool,
}

pub async fn run(app: &mut App) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    let mut state = UiState {
        input: String::new(),
        scrollback: vec![Line::from(vec![Span::styled(
            "llmvm REPL — type a prompt and press Enter. Esc/Ctrl-C to quit.",
            Style::default().add_modifier(Modifier::DIM),
        )])],
        busy: false,
    };

    loop {
        terminal.draw(|f| draw(f, &state))?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if should_quit(key) {
                    break;
                }
                if state.busy {
                    continue;
                }
                match key.code {
                    KeyCode::Enter => {
                        let prompt = state.input.trim().to_string();
                        if prompt.is_empty() {
                            continue;
                        }
                        state.busy = true;
                        state.scrollback.push(Line::from(vec![Span::styled(
                            format!("> {prompt}"),
                            Style::default().fg(Color::Cyan),
                        )]));
                        state.input.clear();

                        let result = app.run_prompt(&prompt).await;
                        state.busy = false;
                        match result {
                            Ok(r) => {
                                let body = serde_json::to_string_pretty(&r.value).unwrap_or_default();
                                for line in body.lines() {
                                    state.scrollback.push(Line::from(line.to_string()));
                                }
                            }
                            Err(e) => {
                                state.scrollback.push(Line::from(vec![Span::styled(
                                    format!("error: {e}"),
                                    Style::default().fg(Color::Red),
                                )]));
                            }
                        }
                    }
                    KeyCode::Char(c) => state.input.push(c),
                    KeyCode::Backspace => {
                        state.input.pop();
                    }
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

fn should_quit(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
        || key.code == KeyCode::Esc
}

fn draw(f: &mut ratatui::Frame, state: &UiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(f.size());

    let transcript = Paragraph::new(state.scrollback.clone())
        .block(Block::default().borders(Borders::ALL).title("transcript"))
        .wrap(ratatui::widgets::Wrap { trim: false });
    f.render_widget(transcript, chunks[0]);

    let prompt_label = if state.busy { "busy" } else { "prompt" };
    let input = Paragraph::new(state.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(prompt_label));
    f.render_widget(input, chunks[1]);
}
