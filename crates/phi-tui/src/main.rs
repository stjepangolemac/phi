use std::{env, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures_util::{FutureExt, StreamExt, future::pending};
use phi_runtime::{Handle, RunOptions, RuntimeCommand, RuntimeEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Text},
    widgets::{Block, Clear, Padding, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    allow_shell: bool,
    #[arg(long)]
    allow_write: bool,
    prompt: Option<String>,
}

#[derive(Default)]
struct Composer {
    text: String,
}

impl Composer {
    fn lines(&self) -> u16 {
        self.text.lines().count().clamp(1, 5) as u16
    }

    fn take(&mut self) -> String {
        std::mem::take(&mut self.text)
    }
}

struct App {
    options: RunOptions,
    session_id: Option<String>,
    transcript: Vec<(String, String)>,
    current_model: String,
    composer: Composer,
    handle: Option<Handle>,
    approval: Option<String>,
    status: String,
    estimated_tokens: Option<u64>,
    token_budget: Option<u64>,
    compactions: u64,
    scroll: u16,
    follow: bool,
    quit: bool,
}

impl App {
    fn new(options: RunOptions) -> Self {
        Self {
            session_id: options.session_id.clone(),
            options,
            transcript: Vec::new(),
            current_model: String::new(),
            composer: Composer::default(),
            handle: None,
            approval: None,
            status: "ready".into(),
            estimated_tokens: None,
            token_budget: None,
            compactions: 0,
            scroll: 0,
            follow: true,
            quit: false,
        }
    }

    fn submit(&mut self) {
        let prompt = self.composer.take();
        if prompt.trim().is_empty() || self.handle.is_some() {
            return;
        }
        self.transcript.push(("you".into(), prompt.clone()));
        self.options.session_id = self.session_id.clone();
        self.handle = Some(phi_runtime::start(self.options.clone(), prompt));
        self.status = "working".into();
        self.follow = true;
    }

    fn on_runtime(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Session { id } => self.session_id = Some(id),
            RuntimeEvent::UserMessage { .. } => {}
            RuntimeEvent::ContextUpdated {
                estimated_tokens,
                token_budget,
                compactions,
            } => {
                self.estimated_tokens = Some(estimated_tokens);
                self.token_budget = Some(token_budget);
                self.compactions = compactions;
            }
            RuntimeEvent::ModelDelta { content } => self.current_model.push_str(&content),
            RuntimeEvent::ToolStarted { name } => {
                self.flush_model();
                self.transcript
                    .push(("tool".into(), format!("{name} · running")));
            }
            RuntimeEvent::ToolCompleted { name, result } => {
                if let Some((_, line)) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|(role, line)| role == "tool" && line.starts_with(&name))
                {
                    *line = format!("{name} · {}", compact_result(&result));
                }
            }
            RuntimeEvent::ApprovalRequested { name } => self.approval = Some(name),
            RuntimeEvent::Finished { content } => {
                if self.current_model.is_empty() && !content.is_empty() {
                    self.current_model = content;
                }
                self.flush_model();
                self.handle = None;
                self.status = "ready".into();
            }
            RuntimeEvent::Error { message } => {
                self.flush_model();
                self.transcript.push(("error".into(), message));
                self.handle = None;
                self.approval = None;
                self.status = "ready".into();
            }
        }
        self.follow = true;
    }

    fn flush_model(&mut self) {
        if !self.current_model.is_empty() {
            self.transcript
                .push(("phi".into(), std::mem::take(&mut self.current_model)));
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if self.approval.is_some() {
            let command = match key.code {
                KeyCode::Char('y') => Some(RuntimeCommand::ApproveOnce),
                KeyCode::Char('n') | KeyCode::Esc => Some(RuntimeCommand::Deny),
                _ => None,
            };
            if let Some(command) = command {
                if let Some(handle) = &self.handle {
                    let _ = handle.commands.send(command);
                }
                self.approval = None;
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if let Some(handle) = &self.handle {
                handle.cancel();
                self.status = "cancelling".into();
            } else {
                self.quit = true;
            }
            return;
        }
        match key.code {
            KeyCode::Esc if self.handle.is_some() => {
                if let Some(handle) = &self.handle {
                    handle.cancel();
                }
                self.status = "cancelling".into();
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.text.push('\n');
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace if self.handle.is_none() => {
                self.composer.text.pop();
            }
            KeyCode::Char(character) if self.handle.is_none() => {
                self.composer.text.push(character);
            }
            KeyCode::PageUp | KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                self.follow = false;
            }
            KeyCode::PageDown | KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                self.follow = false;
            }
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tokio::task::LocalSet::new().run_until(run()).await
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let workspace = cli
        .workspace
        .canonicalize()
        .context("workspace does not exist")?;
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../phi.json");
    let mut app = App::new(RunOptions {
        workspace,
        config_path,
        session_id: cli.session,
        allow_shell: cli.allow_shell,
        allow_write: cli.allow_write,
        interactive_approvals: true,
    });
    if let Some(prompt) = cli.prompt {
        app.composer.text = prompt;
        app.submit();
    }

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut input = EventStream::new();
    while !app.quit {
        terminal.draw(|frame| draw(frame, app))?;
        tokio::select! {
            event = input.next().fuse() => {
                match event.transpose()? {
                    Some(Event::Key(key)) if key.is_press() => app.on_key(key),
                    Some(Event::Resize(_, _)) => {},
                    Some(_) => {},
                    None => app.quit = true,
                }
            }
            event = next_runtime(&mut app.handle).fuse() => {
                if let Some(event) = event {
                    app.on_runtime(event);
                } else {
                    app.handle = None;
                }
            }
        }
    }
    Ok(())
}

async fn next_runtime(handle: &mut Option<Handle>) -> Option<RuntimeEvent> {
    match handle {
        Some(handle) => handle.events.recv().await,
        None => pending().await,
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let composer_height = app.composer.lines() + 2;
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let transcript = transcript_text(app, areas[0].width as usize);
    let height = areas[0].height;
    let line_count = transcript.lines.len() as u16;
    let max_scroll = line_count.saturating_sub(height);
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
    }
    frame.render_widget(Paragraph::new(transcript).scroll((app.scroll, 0)), areas[0]);
    frame.render_widget(
        Paragraph::new(app.composer.text.as_str())
            .block(
                Block::default()
                    .padding(Padding::new(2, 2, 1, 1))
                    .style(Style::default().bg(Color::Rgb(30, 30, 34))),
            )
            .wrap(Wrap { trim: false }),
        areas[1],
    );
    if app.handle.is_none() && app.approval.is_none() {
        let last_line = app.composer.text.rsplit('\n').next().unwrap_or("");
        let x = areas[1].x
            + 2
            + (last_line.chars().count() as u16).min(areas[1].width.saturating_sub(5));
        let y = areas[1].y
            + 1
            + (app.composer.text.lines().count().saturating_sub(1) as u16)
                .min(areas[1].height.saturating_sub(3));
        frame.set_cursor_position((x, y));
    }
    let context = match (app.estimated_tokens, app.token_budget) {
        (Some(used), Some(budget)) => format!("context {used}/{budget} tokens"),
        _ => "context —".into(),
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{} · {context} · {} compactions",
            app.status, app.compactions
        ))
        .style(Style::default().fg(Color::DarkGray)),
        areas[2],
    );

    if let Some(name) = &app.approval {
        let area = centered(frame.area(), 50, 5);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(format!("Allow {name} once?\n\n[y] yes   [n] no"))
                .style(Style::default().bg(Color::Rgb(45, 40, 25))),
            area,
        );
    }
}

fn transcript_text(app: &App, width: usize) -> Text<'static> {
    let mut lines = Vec::new();
    for (role, content) in &app.transcript {
        push_message(&mut lines, role, content, width);
    }
    if !app.current_model.is_empty() {
        push_message(&mut lines, "phi", &app.current_model, width);
    }
    Text::from(lines)
}

fn push_message(lines: &mut Vec<Line<'static>>, role: &str, content: &str, width: usize) {
    let style = match role {
        "you" => Style::default().bg(Color::Rgb(38, 40, 45)),
        "error" => Style::default().fg(Color::Red),
        "tool" => Style::default().fg(Color::Yellow),
        _ => Style::default(),
    };
    lines.push(Line::styled(" ".repeat(width), style));
    let content_width = width.saturating_sub(4).max(1);
    for content_line in content.split('\n') {
        for wrapped in wrap_line(content_line, content_width) {
            let used = UnicodeWidthStr::width(wrapped.as_str()).min(content_width);
            lines.push(Line::styled(
                format!("  {wrapped}{}", " ".repeat(width.saturating_sub(2 + used))),
                style,
            ));
        }
    }
    lines.push(Line::styled(" ".repeat(width), style));
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for character in line.chars() {
        let character_width = character.width().unwrap_or_default();
        if current_width + character_width > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(character);
        current_width += character_width;
    }
    lines.push(current);
    lines
}

fn compact_result(value: &serde_json::Value) -> String {
    let text = value.to_string();
    let mut characters = text.chars();
    let compact: String = characters.by_ref().take(120).collect();
    if characters.next().is_some() {
        format!("{compact}…")
    } else {
        compact
    }
}

fn centered(area: Rect, percent: u16, height: u16) -> Rect {
    let width = area.width.saturating_mul(percent) / 100;
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height: height.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn app() -> App {
        App::new(RunOptions {
            workspace: ".".into(),
            config_path: "phi.json".into(),
            session_id: None,
            allow_shell: false,
            allow_write: false,
            interactive_approvals: true,
        })
    }

    #[test]
    fn reduces_stream_and_tool_events() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ModelDelta {
            content: "hi".into(),
        });
        app.on_runtime(RuntimeEvent::ToolStarted {
            name: "read_file".into(),
        });
        assert_eq!(app.transcript[0], ("phi".into(), "hi".into()));
        assert!(app.transcript[1].1.contains("running"));
    }

    #[test]
    fn renders_small_terminal() {
        let mut app = app();
        app.transcript.push(("you".into(), "hello".into()));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let content = terminal.backend().to_string();
        assert!(content.contains("hello"));
        assert!(!content.contains("conversation"));
    }

    #[test]
    fn preserves_model_line_breaks() {
        let mut app = app();
        app.current_model = "Question?\n\nAnswer.".into();
        let text = transcript_text(&app, 20);
        assert_eq!(text.lines.len(), 5);
        assert_eq!(text.lines[1].to_string().trim(), "Question?");
        assert_eq!(text.lines[2].to_string().trim(), "");
        assert_eq!(text.lines[3].to_string().trim(), "Answer.");
    }

    #[test]
    fn user_background_spans_the_full_block() {
        let mut app = app();
        app.transcript.push(("you".into(), "hello".into()));
        let text = transcript_text(&app, 20);
        assert_eq!(text.lines.len(), 3);
        assert!(text.lines.iter().all(|line| line.to_string().len() == 20));
        assert!(
            text.lines
                .iter()
                .all(|line| line.style.bg == Some(Color::Rgb(38, 40, 45)))
        );
    }

    #[test]
    fn scroll_stops_at_end_of_history() {
        let mut app = app();
        app.transcript.push(("phi".into(), "hello".into()));
        app.follow = false;
        app.scroll = u16::MAX;
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.scroll, 0);
    }

    #[tokio::test]
    async fn echoes_submitted_message_immediately() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = app();
                app.composer.text = "hello".into();
                app.submit();
                assert_eq!(app.transcript, vec![("you".into(), "hello".into())]);
            })
            .await;
    }
}
