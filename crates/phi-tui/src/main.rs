use std::{
    env,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures_util::{FutureExt, StreamExt, future::pending};
use phi_runtime::{
    CommandCatalog, CommandExecution, CommandInvocation, Handle, RunOptions, RuntimeCommand,
    RuntimeEvent,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
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

#[derive(Clone)]
enum Picker {
    Model {
        selected: usize,
    },
    Reasoning {
        model: String,
        selected: usize,
    },
    ServiceTier {
        model: String,
        reasoning: String,
        selected: usize,
    },
}

struct PickerItem {
    label: String,
    description: String,
    value: String,
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
    catalog: CommandCatalog,
    session_id: Option<String>,
    transcript: Vec<(String, String)>,
    current_model: String,
    composer: Composer,
    handle: Option<Handle>,
    command_task: Option<tokio::task::JoinHandle<Result<CommandExecution>>>,
    picker: Option<Picker>,
    approval: Option<String>,
    status: String,
    estimated_tokens: Option<u64>,
    token_budget: Option<u64>,
    input_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    output_tokens: Option<u64>,
    compactions: u64,
    turn_started: Option<Instant>,
    scroll: u16,
    follow: bool,
    quit: bool,
}

impl App {
    fn new(options: RunOptions, catalog: CommandCatalog) -> Self {
        Self {
            session_id: options.session_id.clone(),
            options,
            catalog,
            transcript: Vec::new(),
            current_model: String::new(),
            composer: Composer::default(),
            handle: None,
            command_task: None,
            picker: None,
            approval: None,
            status: "ready".into(),
            estimated_tokens: None,
            token_budget: None,
            input_tokens: None,
            cached_tokens: None,
            cache_write_tokens: None,
            output_tokens: None,
            compactions: 0,
            turn_started: None,
            scroll: 0,
            follow: true,
            quit: false,
        }
    }

    fn submit(&mut self) {
        let prompt = self.composer.take();
        if prompt.trim().is_empty() || self.busy() {
            return;
        }
        self.transcript.push(("you".into(), prompt.clone()));
        self.options.session_id = self.session_id.clone();
        if let Some(invocation) = CommandInvocation::parse(&prompt) {
            match invocation.name.as_str() {
                "help" if invocation.arguments.is_empty() => {
                    self.transcript.push(("phi".into(), self.help()));
                }
                "model" if invocation.arguments.is_empty() => self.open_model_picker(),
                _ => self.start_command(invocation),
            }
            self.follow = true;
            return;
        }
        self.handle = Some(phi_runtime::start(self.options.clone(), prompt));
        self.turn_started = Some(Instant::now());
        self.status = "working".into();
        self.follow = true;
    }

    fn busy(&self) -> bool {
        self.handle.is_some() || self.command_task.is_some() || self.picker.is_some()
    }

    fn help(&self) -> String {
        self.catalog
            .commands
            .iter()
            .map(|command| format!("{} — {}", command.usage, command.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn start_command(&mut self, invocation: CommandInvocation) {
        let options = self.options.clone();
        self.command_task = Some(tokio::task::spawn_blocking(move || {
            phi_runtime::execute_command(&options, &invocation)
        }));
        self.status = "command".into();
    }

    fn on_command(&mut self, result: Result<CommandExecution>) {
        match result {
            Ok(execution) => {
                self.session_id = Some(execution.session_id);
                self.options.session_id = self.session_id.clone();
                self.catalog = execution.catalog;
                self.transcript.push(("phi".into(), execution.content));
            }
            Err(error) => self.transcript.push(("error".into(), error.to_string())),
        }
        self.status = "ready".into();
        self.follow = true;
    }

    fn open_model_picker(&mut self) {
        let selected = self
            .catalog
            .selected_model
            .as_deref()
            .and_then(|current| {
                self.catalog
                    .models
                    .iter()
                    .position(|model| model.id == current)
            })
            .unwrap_or_default();
        self.picker = Some(Picker::Model { selected });
    }

    fn on_picker_key(&mut self, key: KeyEvent) {
        let Some(mut picker) = self.picker.take() else {
            return;
        };
        let options = picker_options(&picker, &self.catalog);
        let selected = match &mut picker {
            Picker::Model { selected }
            | Picker::Reasoning { selected, .. }
            | Picker::ServiceTier { selected, .. } => selected,
        };
        match key.code {
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => {
                *selected = (*selected + 1).min(options.len().saturating_sub(1));
            }
            KeyCode::Esc => {
                self.transcript
                    .push(("phi".into(), "Model selection cancelled.".into()));
                self.follow = true;
                return;
            }
            KeyCode::Enter => {
                let Some(value) = options.get(*selected).map(|option| option.value.clone()) else {
                    self.transcript
                        .push(("error".into(), "No models available.".into()));
                    return;
                };
                picker = match picker {
                    Picker::Model { .. } => {
                        let model = self
                            .catalog
                            .models
                            .iter()
                            .find(|model| model.id == value)
                            .cloned()
                            .expect("picker model comes from catalog");
                        if model.reasoning.is_empty() {
                            return self.finish_model_picker(
                                model.id,
                                model.default_reasoning,
                                model.default_service_tier,
                            );
                        }
                        let selected = model
                            .reasoning
                            .iter()
                            .position(|option| option.id() == model.default_reasoning)
                            .unwrap_or_default();
                        Picker::Reasoning {
                            model: model.id,
                            selected,
                        }
                    }
                    Picker::Reasoning { model, .. } => {
                        let spec = self
                            .catalog
                            .models
                            .iter()
                            .find(|candidate| candidate.id == model)
                            .cloned()
                            .expect("picker model comes from catalog");
                        if spec.service_tiers.is_empty() {
                            return self.finish_model_picker(
                                model,
                                value,
                                spec.default_service_tier,
                            );
                        }
                        let selected = spec
                            .service_tiers
                            .iter()
                            .position(|option| option.id() == spec.default_service_tier)
                            .unwrap_or_default();
                        Picker::ServiceTier {
                            model,
                            reasoning: value,
                            selected,
                        }
                    }
                    Picker::ServiceTier {
                        model, reasoning, ..
                    } => return self.finish_model_picker(model, reasoning, value),
                };
            }
            _ => {}
        }
        self.picker = Some(picker);
    }

    fn finish_model_picker(&mut self, model: String, reasoning: String, tier: String) {
        self.start_command(CommandInvocation {
            name: "model".into(),
            arguments: format!("{model} {reasoning} {tier}"),
        });
    }

    fn on_runtime(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Session { id } => self.session_id = Some(id),
            RuntimeEvent::UserMessage { .. } => {}
            RuntimeEvent::ContextUpdated {
                estimated_tokens,
                token_budget,
                compactions,
                input_tokens,
                cached_tokens,
                cache_write_tokens,
                output_tokens,
            } => {
                self.estimated_tokens = Some(estimated_tokens);
                self.token_budget = Some(token_budget);
                self.input_tokens = input_tokens;
                self.cached_tokens = cached_tokens;
                self.cache_write_tokens = cache_write_tokens;
                self.output_tokens = output_tokens;
                self.compactions = compactions;
            }
            RuntimeEvent::ModelDelta { content } => self.current_model.push_str(&content),
            RuntimeEvent::ToolStarted { name, arguments } => {
                self.flush_model();
                self.transcript.push((
                    "tool".into(),
                    if name == "shell" {
                        format!("Ran `{}`", display_command(&arguments))
                    } else {
                        format!("Ran `{name}`")
                    },
                ));
            }
            RuntimeEvent::ToolCompleted { result, .. } => {
                if let Some((_, content)) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|(role, _)| role == "tool")
                {
                    let result = tool_result(&result);
                    if !result.is_empty() {
                        content.push_str("\n\n");
                        content.push_str(&result);
                    }
                }
            }
            RuntimeEvent::ApprovalRequested { name } => self.approval = Some(name),
            RuntimeEvent::Finished { content } => {
                if self.current_model.is_empty() && !content.is_empty() {
                    self.current_model = content;
                }
                self.flush_model();
                let elapsed = self
                    .turn_started
                    .take()
                    .map_or(Duration::ZERO, |time| time.elapsed());
                self.transcript.push((
                    "turn_end".into(),
                    format!("Worked for {}", human_duration(elapsed)),
                ));
                self.handle = None;
                self.status = "ready".into();
            }
            RuntimeEvent::Error { message } => {
                self.flush_model();
                self.transcript.push(("error".into(), message));
                self.handle = None;
                self.approval = None;
                self.turn_started = None;
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

    fn command_suggestions(&self) -> Vec<&phi_runtime::CommandSpec> {
        let Some(prefix) = self.composer.text.strip_prefix('/') else {
            return Vec::new();
        };
        if prefix.chars().any(char::is_whitespace) {
            return Vec::new();
        }
        self.catalog
            .commands
            .iter()
            .filter(|command| command.name.starts_with(prefix))
            .take(5)
            .collect()
    }

    fn on_key(&mut self, key: KeyEvent) {
        if self.picker.is_some() {
            self.on_picker_key(key);
            return;
        }
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
            } else if self.command_task.is_none() {
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
            KeyCode::Tab if !self.busy() => {
                if let Some(command) = self.command_suggestions().first() {
                    self.composer.text = format!("/{} ", command.name);
                }
            }
            KeyCode::Backspace if !self.busy() => {
                self.composer.text.pop();
            }
            KeyCode::Char(character) if !self.busy() => {
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
    let options = RunOptions {
        workspace,
        config_path,
        session_id: cli.session,
        allow_shell: cli.allow_shell,
        allow_write: cli.allow_write,
        interactive_approvals: true,
    };
    let catalog = phi_runtime::command_catalog(&options)?;
    let mut app = App::new(options, catalog);
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
            result = next_command(&mut app.command_task).fuse() => {
                app.command_task = None;
                if let Some(result) = result {
                    app.on_command(result);
                }
            }
        }
    }
    Ok(())
}

async fn next_command(
    task: &mut Option<tokio::task::JoinHandle<Result<CommandExecution>>>,
) -> Option<Result<CommandExecution>> {
    match task {
        Some(task) => Some(
            task.await
                .map_err(anyhow::Error::from)
                .and_then(|result| result),
        ),
        None => pending().await,
    }
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
    let suggestions = if app.picker.is_none() {
        app.command_suggestions()
    } else {
        Vec::new()
    };
    if !suggestions.is_empty() {
        let height = suggestions.len() as u16;
        let area = Rect {
            x: areas[1].x,
            y: areas[1].y.saturating_sub(height),
            width: areas[1].width,
            height,
        };
        let content = suggestions
            .iter()
            .map(|command| format!("{}  {}", command.usage, command.description))
            .collect::<Vec<_>>()
            .join("\n");
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(content).style(Style::default().bg(Color::Rgb(30, 30, 34))),
            area,
        );
    }
    if !app.busy() && app.approval.is_none() {
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
    let context = match (app.input_tokens.or(app.estimated_tokens), app.token_budget) {
        (Some(used), Some(budget)) => format!("context {used}/{budget} tokens"),
        _ => "context —".into(),
    };
    let cache = match (app.cached_tokens, app.cache_write_tokens) {
        (Some(read), Some(written)) => format!("cache {read} read/{written} write"),
        _ => "cache —".into(),
    };
    let output = app
        .output_tokens
        .map_or_else(|| "output —".into(), |tokens| format!("output {tokens}"));
    let model = app.catalog.selected_model.as_deref().unwrap_or("model —");
    let reasoning = app.catalog.selected_reasoning.as_deref().unwrap_or("—");
    let tier = app.catalog.selected_service_tier.as_deref();
    let selection = tier.map_or_else(
        || format!("{model} · {reasoning}"),
        |tier| format!("{model} · {reasoning} · {tier}"),
    );
    frame.render_widget(
        Paragraph::new(format!(
            "{selection} · {} · {context} · {cache} · {output} · {} compactions",
            app.status, app.compactions,
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

    if let Some(picker) = &app.picker {
        draw_picker(frame, picker, &app.catalog, areas[1]);
    }
}

fn picker_options(picker: &Picker, catalog: &CommandCatalog) -> Vec<PickerItem> {
    match picker {
        Picker::Model { .. } => catalog
            .models
            .iter()
            .map(|model| PickerItem {
                label: model.id.clone(),
                description: model.description.clone(),
                value: model.id.clone(),
            })
            .collect(),
        Picker::Reasoning { model, .. } => catalog
            .models
            .iter()
            .find(|candidate| candidate.id == *model)
            .map(|model| {
                model
                    .reasoning
                    .iter()
                    .map(|option| PickerItem {
                        label: option.id().into(),
                        description: option.description().into(),
                        value: option.id().into(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Picker::ServiceTier { model, .. } => catalog
            .models
            .iter()
            .find(|candidate| candidate.id == *model)
            .map(|model| {
                model
                    .service_tiers
                    .iter()
                    .map(|option| PickerItem {
                        label: option.id().into(),
                        description: option.description().into(),
                        value: option.id().into(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn draw_picker(frame: &mut Frame, picker: &Picker, catalog: &CommandCatalog, composer: Rect) {
    let options = picker_options(picker, catalog);
    let selected = match picker {
        Picker::Model { selected }
        | Picker::Reasoning { selected, .. }
        | Picker::ServiceTier { selected, .. } => *selected,
    };
    let label_width = options
        .iter()
        .map(|option| UnicodeWidthStr::width(option.label.as_str()))
        .max()
        .unwrap_or_default();
    let lines = options.iter().enumerate().map(|(index, option)| {
        picker_line(
            index == selected,
            option,
            label_width,
            composer.width as usize,
        )
    });
    let lines = lines.collect::<Vec<_>>();
    let height = (lines.len() as u16).min(composer.y);
    let area = picker_area(composer, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(Color::Rgb(30, 30, 34))),
        area,
    );
}

fn picker_line(
    selected: bool,
    option: &PickerItem,
    label_width: usize,
    width: usize,
) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let actual_label_width = UnicodeWidthStr::width(option.label.as_str());
    let gap = label_width.saturating_sub(actual_label_width) + 2;
    let prefix_width = 2 + actual_label_width + gap;
    let description = truncate_width(&option.description, width.saturating_sub(prefix_width));
    Line::raw(format!(
        "{marker}{}{}{description}",
        option.label,
        " ".repeat(gap)
    ))
}

fn truncate_width(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = character.width().unwrap_or_default();
        if used + character_width + 1 > width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

fn picker_area(composer: Rect, height: u16) -> Rect {
    Rect {
        x: composer.x,
        y: composer.y.saturating_sub(height),
        width: composer.width,
        height,
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
    if lines
        .last()
        .is_some_and(|line| !line.to_string().trim().is_empty() || line.style.bg.is_some())
    {
        lines.push(Line::raw(" ".repeat(width)));
    }
    Text::from(lines)
}

fn push_message(lines: &mut Vec<Line<'static>>, role: &str, content: &str, width: usize) {
    if role == "turn_end" {
        lines.push(Line::raw(turn_divider(content, width)));
        lines.push(Line::raw(" ".repeat(width)));
        return;
    }
    if role == "tool" {
        push_tool(lines, content, width);
        return;
    }
    let style = match role {
        "you" => Style::default().bg(Color::Rgb(38, 40, 45)),
        "error" => Style::default().fg(Color::Red),
        _ => Style::default(),
    };
    lines.push(Line::styled(" ".repeat(width), style));
    let content_width = width.saturating_sub(2).max(1);
    let mut first = true;
    for content_line in content.split('\n') {
        for wrapped in wrap_line(content_line, content_width) {
            let used = UnicodeWidthStr::width(wrapped.as_str()).min(content_width);
            let marker = if first { "• " } else { "  " };
            lines.push(Line::styled(
                format!(
                    "{marker}{wrapped}{}",
                    " ".repeat(width.saturating_sub(2 + used))
                ),
                style,
            ));
            first = false;
        }
    }
    lines.push(Line::styled(" ".repeat(width), style));
}

fn push_tool(lines: &mut Vec<Line<'static>>, content: &str, width: usize) {
    let command_style = Style::default().fg(Color::Rgb(190, 190, 185));
    let output_style = Style::default().fg(Color::Rgb(125, 125, 122));
    lines.push(Line::styled(" ".repeat(width), command_style));
    let (command, output) = content
        .split_once("\n\n")
        .map_or((content, None), |(command, output)| (command, Some(output)));
    for (index, command) in wrap_line(command, width.saturating_sub(2).max(1))
        .iter()
        .enumerate()
    {
        if index == 0 {
            lines.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(Color::Green)),
                Span::styled(command.clone(), command_style),
            ]));
        } else {
            lines.push(Line::styled(format!("  {command}"), command_style));
        }
    }
    if let Some(output) = output {
        lines.push(Line::styled(String::new(), output_style));
        let wrapped = output
            .split('\n')
            .flat_map(|line| wrap_line(line, width.saturating_sub(4).max(1)))
            .collect::<Vec<_>>();
        for (index, output) in wrapped.iter().enumerate() {
            let prefix = if index == 0 { "  └ " } else { "    " };
            lines.push(Line::styled(format!("{prefix}{output}"), output_style));
        }
    }
    lines.push(Line::styled(" ".repeat(width), command_style));
}

fn turn_divider(label: &str, width: usize) -> String {
    let label_width = UnicodeWidthStr::width(label);
    if width <= label_width + 2 {
        return label.chars().take(width).collect();
    }
    format!("─ {label} {}", "─".repeat(width - label_width - 3))
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

fn display_command(arguments: &serde_json::Value) -> String {
    let mut command = vec![
        arguments
            .get("program")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("shell")
            .to_owned(),
    ];
    command.extend(
        arguments
            .get("args")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(quote_argument),
    );
    command.join(" ")
}

fn quote_argument(argument: &str) -> String {
    if argument
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_=./:".contains(character))
    {
        argument.into()
    } else {
        format!("{:?}", argument)
    }
}

fn tool_result(result: &serde_json::Value) -> String {
    if let Some(error) = result.get("error").and_then(serde_json::Value::as_str) {
        return truncate_display(error, 2_000);
    }
    let mut output = String::new();
    for field in ["stdout", "stderr"] {
        if let Some(value) = result.get(field).and_then(serde_json::Value::as_str)
            && !value.trim().is_empty()
        {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(value.trim_end());
        }
    }
    if output.is_empty()
        && let Some(code) = result.get("exit_code").and_then(serde_json::Value::as_i64)
    {
        output = format!("Exited with code {code}");
    }
    truncate_display(&output, 2_000)
}

fn truncate_display(value: &str, max_chars: usize) -> String {
    let mut characters = value.chars();
    let value: String = characters.by_ref().take(max_chars).collect();
    if characters.next().is_some() {
        format!("{value}\n… output truncated")
    } else {
        value
    }
}

fn human_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3_600 {
        format!(
            "{}h {}m {}s",
            seconds / 3_600,
            seconds % 3_600 / 60,
            seconds % 60
        )
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
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
        App::new(
            RunOptions {
                workspace: ".".into(),
                config_path: "phi.json".into(),
                session_id: None,
                allow_shell: false,
                allow_write: false,
                interactive_approvals: true,
            },
            CommandCatalog {
                commands: vec![phi_runtime::CommandSpec {
                    name: "model".into(),
                    usage: "/model [MODEL]".into(),
                    description: "Select model.".into(),
                    source: "core".into(),
                }],
                models: vec![phi_runtime::ModelSpec {
                    id: "test".into(),
                    label: "Test".into(),
                    description: "Test model.".into(),
                    default: true,
                    reasoning: vec![
                        phi_runtime::PickerOptionSpec::Detailed {
                            id: "low".into(),
                            description: "Fast.".into(),
                        },
                        phi_runtime::PickerOptionSpec::Detailed {
                            id: "high".into(),
                            description: "Thorough.".into(),
                        },
                    ],
                    default_reasoning: "low".into(),
                    service_tiers: vec![
                        phi_runtime::PickerOptionSpec::Detailed {
                            id: "default".into(),
                            description: "Standard.".into(),
                        },
                        phi_runtime::PickerOptionSpec::Detailed {
                            id: "fast".into(),
                            description: "Faster.".into(),
                        },
                    ],
                    default_service_tier: "default".into(),
                }],
                selected_model: Some("test".into()),
                selected_reasoning: Some("low".into()),
                selected_service_tier: Some("default".into()),
            },
        )
    }

    #[test]
    fn reduces_stream_and_tool_events() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ModelDelta {
            content: "hi".into(),
        });
        app.on_runtime(RuntimeEvent::ToolStarted {
            name: "read_file".into(),
            arguments: serde_json::json!({}),
        });
        assert_eq!(app.transcript[0], ("phi".into(), "hi".into()));
        assert_eq!(app.transcript[1].1, "Ran `read_file`");
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
    fn shows_provider_usage_and_cache_counts() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ContextUpdated {
            estimated_tokens: 400,
            token_budget: 6_000,
            compactions: 0,
            input_tokens: Some(1_500),
            cached_tokens: Some(1_024),
            cache_write_tokens: Some(128),
            output_tokens: Some(50),
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let content = terminal.backend().to_string();
        assert!(content.contains("context 1500/6000 tokens"));
        assert!(content.contains("cache 1024 read/128 write"));
        assert!(content.contains("output 50"));
    }

    #[test]
    fn preserves_model_line_breaks() {
        let mut app = app();
        app.current_model = "Question?\n\nAnswer.".into();
        let text = transcript_text(&app, 20);
        assert_eq!(text.lines.len(), 5);
        assert_eq!(text.lines[1].to_string().trim(), "• Question?");
        assert_eq!(text.lines[2].to_string().trim(), "");
        assert_eq!(text.lines[3].to_string().trim(), "Answer.");
    }

    #[test]
    fn user_background_spans_the_full_block() {
        let mut app = app();
        app.transcript.push(("you".into(), "hello".into()));
        let text = transcript_text(&app, 20);
        assert_eq!(text.lines.len(), 4);
        assert!(
            text.lines
                .iter()
                .all(|line| { UnicodeWidthStr::width(line.to_string().as_str()) == 20 })
        );
        assert!(
            text.lines[..3]
                .iter()
                .all(|line| line.style.bg == Some(Color::Rgb(38, 40, 45)))
        );
        assert_eq!(text.lines[3].style.bg, None);
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

    #[test]
    fn formats_shell_command_and_indented_output() {
        let command = display_command(&serde_json::json!({
            "program": "rg",
            "args": ["hello world", "src"]
        }));
        assert_eq!(command, "rg \"hello world\" src");
        let mut lines = Vec::new();
        push_tool(&mut lines, &format!("Ran `{command}`\n\nmatch\nsecond"), 40);
        assert!(lines[1].to_string().starts_with("• Ran `rg"));
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Green));
        assert_eq!(lines[2].to_string(), "");
        assert_eq!(lines[3].to_string(), "  └ match");
        assert_eq!(lines[4].to_string(), "    second");
        assert_eq!(lines[3].style.fg, Some(Color::Rgb(125, 125, 122)));
    }

    #[test]
    fn formats_turn_duration_and_divider() {
        assert_eq!(human_duration(Duration::from_secs(152)), "2m 32s");
        let divider = turn_divider("Worked for 2m 32s", 40);
        assert_eq!(UnicodeWidthStr::width(divider.as_str()), 40);
        assert!(divider.starts_with("─ Worked for 2m 32s "));
        assert!(divider.ends_with('─'));

        let mut lines = Vec::new();
        push_message(&mut lines, "turn_end", "Worked for 2m 32s", 40);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].to_string().trim().is_empty());
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

    #[test]
    fn model_picker_advances_through_provider_options() {
        let mut app = app();
        app.composer.text = "/model".into();
        app.submit();
        assert!(matches!(app.picker, Some(Picker::Model { .. })));

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.picker, Some(Picker::Reasoning { .. })));

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.picker, Some(Picker::ServiceTier { .. })));
    }

    #[test]
    fn help_uses_loaded_catalog_without_starting_command_work() {
        let mut app = app();
        app.composer.text = "/help".into();
        app.submit();
        assert!(app.command_task.is_none());
        assert_eq!(
            app.transcript.last().unwrap().1,
            "/model [MODEL] — Select model."
        );
    }

    #[test]
    fn picker_is_anchored_directly_above_composer() {
        let composer = Rect::new(0, 20, 80, 3);
        assert_eq!(picker_area(composer, 8), Rect::new(0, 12, 80, 8));
    }

    #[test]
    fn picker_descriptions_align_and_truncate() {
        let low = PickerItem {
            label: "low".into(),
            description: "Fast responses".into(),
            value: "low".into(),
        };
        let medium = PickerItem {
            label: "medium".into(),
            description: "Balanced".into(),
            value: "medium".into(),
        };
        let low = picker_line(true, &low, 6, 15).to_string();
        let medium = picker_line(false, &medium, 6, 15).to_string();
        let low_start = low.find("Fast").unwrap();
        let medium_start = medium.find("Bala").unwrap();
        assert_eq!(
            UnicodeWidthStr::width(&low[..low_start]),
            UnicodeWidthStr::width(&medium[..medium_start])
        );
        assert!(low.ends_with('…'));
        assert_eq!(truncate_width("abcdef", 4), "abc…");
    }
}
