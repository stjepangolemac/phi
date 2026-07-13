use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures_util::{FutureExt, StreamExt, future::pending};
use phi_runtime::{
    CommandCatalog, CommandExecution, CommandInvocation, Handle, RunOptions, RuntimeCommand,
    RuntimeEvent,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, Padding, Paragraph, Wrap},
};
use tui_markdown::StyleSheet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone)]
struct PhiMarkdown;

impl tui_markdown::StyleSheet for PhiMarkdown {
    fn heading(&self, _level: u8) -> Style {
        Style::default()
            .fg(Color::Rgb(225, 225, 220))
            .add_modifier(Modifier::BOLD)
    }

    fn code(&self) -> Style {
        Style::default()
            .fg(Color::Rgb(200, 200, 195))
            .bg(Color::Rgb(27, 28, 31))
    }

    fn link(&self) -> Style {
        Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::UNDERLINED)
    }

    fn blockquote(&self) -> Style {
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::ITALIC)
    }

    fn heading_meta(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }

    fn metadata_block(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }
}

#[derive(Default)]
struct Composer {
    text: String,
    cursor: usize,
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
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    fn insert(&mut self, character: char) {
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    fn backspace(&mut self) {
        if let Some(previous) = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
        {
            self.text.drain(previous..self.cursor);
            self.cursor = previous;
        }
    }

    fn delete(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.text
                .drain(self.cursor..self.cursor + character.len_utf8());
        }
    }

    fn move_left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn move_right(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    fn move_word_left(&mut self) {
        while self.previous_char().is_some_and(char::is_whitespace) {
            self.move_left();
        }
        while self
            .previous_char()
            .is_some_and(|character| !character.is_whitespace())
        {
            self.move_left();
        }
    }

    fn move_word_right(&mut self) {
        while self.next_char().is_some_and(char::is_whitespace) {
            self.move_right();
        }
        while self
            .next_char()
            .is_some_and(|character| !character.is_whitespace())
        {
            self.move_right();
        }
    }

    fn delete_word_left(&mut self) {
        let end = self.cursor;
        self.move_word_left();
        self.text.drain(self.cursor..end);
    }

    fn delete_to_line_start(&mut self) {
        let end = self.cursor;
        self.move_line_start();
        self.text.drain(self.cursor..end);
    }

    fn move_line_start(&mut self) {
        self.cursor = self.line_start();
    }

    fn move_line_end(&mut self) {
        self.cursor = self.line_end();
    }

    fn move_start(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    fn move_up(&mut self) {
        let start = self.line_start();
        if start == 0 {
            return;
        }
        let column = self.text[start..self.cursor].chars().count();
        let previous_end = start - 1;
        let previous_start = self.text[..previous_end]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.cursor = byte_at_column(&self.text, previous_start, previous_end, column);
    }

    fn move_down(&mut self) {
        let end = self.line_end();
        if end == self.text.len() {
            return;
        }
        let column = self.text[self.line_start()..self.cursor].chars().count();
        let next_start = end + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map_or(self.text.len(), |index| next_start + index);
        self.cursor = byte_at_column(&self.text, next_start, next_end, column);
    }

    fn on_first_line(&self) -> bool {
        self.line_start() == 0
    }

    fn on_last_line(&self) -> bool {
        self.line_end() == self.text.len()
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index)
    }

    fn previous_char(&self) -> Option<char> {
        self.text[..self.cursor].chars().next_back()
    }

    fn next_char(&self) -> Option<char> {
        self.text[self.cursor..].chars().next()
    }
}

fn byte_at_column(text: &str, start: usize, end: usize, column: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(column)
        .map_or(end, |(index, _)| start + index)
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
    compaction_started: Option<Instant>,
    tool_started: Option<(String, Instant)>,
    final_response_rendered: bool,
    command_filter: Option<String>,
    command_selected: usize,
    message_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    scroll: u16,
    follow: bool,
    quit: bool,
}

impl App {
    fn new(options: RunOptions, catalog: CommandCatalog) -> Self {
        let transcript = if options.session_id.is_none() {
            vec![(
                "note".into(),
                format!(
                    "Ready in {}\n\nType / for commands.",
                    compact_path(&options.workspace)
                ),
            )]
        } else {
            Vec::new()
        };
        Self {
            session_id: options.session_id.clone(),
            options,
            catalog,
            transcript,
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
            compaction_started: None,
            tool_started: None,
            final_response_rendered: false,
            command_filter: None,
            command_selected: 0,
            message_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            scroll: 0,
            follow: true,
            quit: false,
        }
    }

    fn submit(&mut self) {
        if self.busy() {
            return;
        }
        self.command_filter = None;
        self.command_selected = 0;
        let prompt = self.composer.take();
        if prompt.trim().is_empty() {
            return;
        }
        self.options.session_id = self.session_id.clone();
        if let Some(invocation) = CommandInvocation::parse(&prompt) {
            match invocation.name.as_str() {
                "help" if invocation.arguments.is_empty() => {
                    self.transcript.push(("note".into(), self.help()));
                }
                "model" if invocation.arguments.is_empty() => self.open_model_picker(),
                _ => self.start_command(invocation),
            }
            self.follow = true;
            return;
        }
        self.transcript.push(("you".into(), prompt.clone()));
        self.message_history.push(prompt.clone());
        self.history_index = None;
        self.history_draft.clear();
        self.handle = Some(phi_runtime::start(self.options.clone(), prompt));
        self.turn_started = Some(Instant::now());
        self.final_response_rendered = false;
        self.status = "working".into();
        self.follow = true;
    }

    fn busy(&self) -> bool {
        self.handle.is_some() || self.command_task.is_some() || self.picker.is_some()
    }

    fn composer_locked(&self) -> bool {
        self.command_task.is_some() || self.picker.is_some()
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
                self.transcript.push(("note".into(), execution.content));
            }
            Err(error) => self.transcript.push(("error".into(), error.to_string())),
        }
        self.status = "ready".into();
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
                    .push(("note".into(), "Model selection cancelled.".into()));
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
                if compactions > self.compactions {
                    let before = self.estimated_tokens.unwrap_or_default();
                    let elapsed = self
                        .compaction_started
                        .take()
                        .map_or(Duration::ZERO, |time| time.elapsed());
                    self.transcript.push((
                        "compaction_end".into(),
                        format!(
                            "Compacted in {} · context {} → {} tokens",
                            human_duration(elapsed),
                            before,
                            estimated_tokens
                        ),
                    ));
                }
                self.estimated_tokens = Some(estimated_tokens);
                self.token_budget = Some(token_budget);
                self.input_tokens = input_tokens;
                self.cached_tokens = cached_tokens;
                self.cache_write_tokens = cache_write_tokens;
                self.output_tokens = output_tokens;
                self.compactions = compactions;
            }
            RuntimeEvent::ActivityChanged { activity } => match activity.as_str() {
                "compacting" => {
                    self.final_response_rendered = !self.current_model.is_empty();
                    self.flush_model();
                    self.compaction_started = Some(Instant::now());
                    self.status = activity;
                }
                "searching" => {
                    self.flush_model();
                    self.status = activity;
                }
                "working" => self.status = activity,
                _ => {}
            },
            RuntimeEvent::ToolRouteSelected { .. } => {}
            RuntimeEvent::ModelDelta { content } => self.current_model.push_str(&content),
            RuntimeEvent::ToolStarted { name, arguments } => {
                self.flush_model();
                self.tool_started = Some((name.clone(), Instant::now()));
                if name == "web_search" {
                    self.status = "searching".into();
                }
                self.transcript.push((
                    "tool".into(),
                    match name.as_str() {
                        "shell" => format!("Ran `{}`", display_command(&arguments)),
                        "web_search" => "Searching the web".into(),
                        _ => format!("Ran `{name}`"),
                    },
                ));
            }
            RuntimeEvent::ToolCompleted { name, result } => {
                let elapsed = self
                    .tool_started
                    .take()
                    .filter(|(started_name, _)| started_name == &name)
                    .map_or(Duration::ZERO, |(_, started)| started.elapsed());
                if let Some((_, content)) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|(role, _)| role == "tool")
                {
                    if name == "web_search" {
                        let action = &result["action"];
                        let sources = result
                            .pointer("/action/sources")
                            .and_then(serde_json::Value::as_array)
                            .map(Vec::len)
                            .unwrap_or_default();
                        let completed = match action["type"].as_str() {
                            Some("open_page") => action["url"]
                                .as_str()
                                .map(url_host)
                                .filter(|host| !host.is_empty())
                                .map_or_else(
                                    || "Opened a page".into(),
                                    |host| format!("Opened {host}"),
                                ),
                            _ => "Searched the web".into(),
                        };
                        *content = format!("{completed} · {}", human_duration(elapsed));
                        if sources > 0 {
                            content.push_str(&format!("\n\n{sources} sources"));
                        }
                        self.status = "working".into();
                    } else {
                        let result = tool_result(&result);
                        if !result.is_empty() {
                            content.push_str("\n\n");
                            content.push_str(&result);
                        }
                    }
                }
            }
            RuntimeEvent::ApprovalRequested { name } => self.approval = Some(name),
            RuntimeEvent::Finished { content } => {
                if !self.final_response_rendered
                    && self.current_model.is_empty()
                    && !content.is_empty()
                {
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
                self.compaction_started = None;
                self.tool_started = None;
                self.final_response_rendered = false;
                self.status = "ready".into();
            }
            RuntimeEvent::Error { message } => {
                self.flush_model();
                self.transcript.push(("error".into(), message));
                self.handle = None;
                self.approval = None;
                self.turn_started = None;
                self.compaction_started = None;
                self.tool_started = None;
                self.final_response_rendered = false;
                self.status = "ready".into();
            }
        }
    }

    fn flush_model(&mut self) {
        if !self.current_model.is_empty() {
            self.transcript
                .push(("phi".into(), std::mem::take(&mut self.current_model)));
        }
    }

    fn command_suggestions(&self) -> Vec<&phi_runtime::CommandSpec> {
        let input = self
            .command_filter
            .as_deref()
            .unwrap_or(&self.composer.text);
        let Some(prefix) = input.strip_prefix('/') else {
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

    fn navigate_commands(&mut self, down: bool) -> bool {
        if self.busy() || self.command_suggestions().is_empty() {
            return false;
        }
        if self.command_filter.is_none() {
            self.command_filter = Some(self.composer.text.clone());
        }
        let count = self.command_suggestions().len();
        self.command_selected = if down {
            (self.command_selected + 1).min(count.saturating_sub(1))
        } else {
            self.command_selected.saturating_sub(1)
        };
        if let Some(name) = self
            .command_suggestions()
            .get(self.command_selected)
            .map(|command| command.name.clone())
        {
            self.composer.set(format!("/{name}"));
        }
        true
    }

    fn complete_command(&mut self) -> bool {
        let Some(name) = self
            .command_suggestions()
            .get(self.command_selected)
            .map(|command| command.name.clone())
        else {
            return false;
        };
        self.composer.set(format!("/{name} "));
        self.command_filter = None;
        self.command_selected = 0;
        true
    }

    fn previous_message(&mut self) -> bool {
        if self.message_history.is_empty() {
            return false;
        }
        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.history_draft.clone_from(&self.composer.text);
                self.message_history.len() - 1
            }
        };
        self.history_index = Some(index);
        self.composer.set(self.message_history[index].clone());
        true
    }

    fn next_message(&mut self) -> bool {
        let Some(index) = self.history_index else {
            return false;
        };
        if index + 1 < self.message_history.len() {
            self.history_index = Some(index + 1);
            self.composer.set(self.message_history[index + 1].clone());
        } else {
            self.history_index = None;
            self.composer.set(std::mem::take(&mut self.history_draft));
        }
        true
    }

    fn edit_composer(&mut self) {
        self.command_filter = None;
        self.command_selected = 0;
        self.history_index = None;
        self.history_draft.clear();
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
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::CONTROL) && !self.composer_locked() =>
            {
                self.edit_composer();
                self.composer.insert('\n');
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Tab if !self.composer_locked() => {
                self.complete_command();
            }
            KeyCode::Backspace
                if !self.composer_locked() && key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.edit_composer();
                self.composer.delete_word_left();
            }
            KeyCode::Backspace
                if !self.composer_locked() && key.modifiers.contains(KeyModifiers::SUPER) =>
            {
                self.edit_composer();
                self.composer.delete_to_line_start();
            }
            KeyCode::Backspace if !self.composer_locked() => {
                self.edit_composer();
                self.composer.backspace();
            }
            KeyCode::Delete if !self.composer_locked() => {
                self.edit_composer();
                self.composer.delete();
            }
            KeyCode::Char('b')
                if !self.composer_locked() && key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.composer.move_word_left();
            }
            KeyCode::Char('f')
                if !self.composer_locked() && key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.composer.move_word_right();
            }
            KeyCode::Char('a')
                if !self.composer_locked() && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.composer.move_line_start();
            }
            KeyCode::Char('e')
                if !self.composer_locked() && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.composer.move_line_end();
            }
            KeyCode::Char(character) if !self.composer_locked() => {
                self.edit_composer();
                self.composer.insert(character);
            }
            KeyCode::Up if self.navigate_commands(false) => {}
            KeyCode::Down if self.navigate_commands(true) => {}
            KeyCode::Left
                if !self.composer_locked() && key.modifiers.intersects(KeyModifiers::ALT) =>
            {
                self.composer.move_word_left();
            }
            KeyCode::Right
                if !self.composer_locked() && key.modifiers.intersects(KeyModifiers::ALT) =>
            {
                self.composer.move_word_right();
            }
            KeyCode::Left
                if !self.composer_locked() && key.modifiers.intersects(KeyModifiers::SUPER) =>
            {
                self.composer.move_line_start();
            }
            KeyCode::Right
                if !self.composer_locked() && key.modifiers.intersects(KeyModifiers::SUPER) =>
            {
                self.composer.move_line_end();
            }
            KeyCode::Up
                if !self.composer_locked() && key.modifiers.intersects(KeyModifiers::SUPER) =>
            {
                self.composer.move_start();
            }
            KeyCode::Down
                if !self.composer_locked() && key.modifiers.intersects(KeyModifiers::SUPER) =>
            {
                self.composer.move_end();
            }
            KeyCode::Home if !self.composer_locked() => self.composer.move_line_start(),
            KeyCode::End if !self.composer_locked() => self.composer.move_line_end(),
            KeyCode::Left if !self.composer_locked() => self.composer.move_left(),
            KeyCode::Right if !self.composer_locked() => self.composer.move_right(),
            KeyCode::Up if !self.composer_locked() && self.composer.on_first_line() => {
                self.previous_message();
            }
            KeyCode::Down if !self.composer_locked() && self.next_message() => {}
            KeyCode::Up if !self.composer_locked() => self.composer.move_up(),
            KeyCode::Down if !self.composer_locked() && !self.composer.on_last_line() => {
                self.composer.move_down();
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(1);
                self.follow = false;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(1);
                self.follow = false;
            }
            _ => {}
        }
    }
}

pub async fn launch(options: RunOptions, prompt: Option<String>) -> Result<()> {
    let catalog = phi_runtime::command_catalog(&options)?;
    let mut app = App::new(options, catalog);
    if let Some(prompt) = prompt {
        app.composer.set(prompt);
        app.submit();
    }

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    while !app.quit {
        terminal.draw(|frame| draw(frame, app))?;
        tokio::select! {
            _ = tick.tick(), if app.turn_started.is_some() => {}
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

    let mut transcript = transcript_text(app, areas[0].width as usize);
    let height = areas[0].height;
    let padding = height.saturating_sub(transcript.lines.len() as u16) as usize;
    if padding > 0 {
        let mut lines = vec![Line::raw(String::new()); padding];
        lines.extend(transcript.lines);
        transcript = Text::from(lines);
    }
    let line_count = transcript.lines.len() as u16;
    let max_scroll = line_count.saturating_sub(height);
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll == max_scroll {
            app.follow = true;
        }
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
            .enumerate()
            .map(|(index, command)| {
                let marker = if index == app.command_selected {
                    "› "
                } else {
                    "  "
                };
                Line::raw(truncate_width(
                    &format!("{marker}{}  {}", command.usage, command.description),
                    area.width as usize,
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(content).style(Style::default().bg(Color::Rgb(30, 30, 34))),
            area,
        );
    }
    if !app.composer_locked() && app.approval.is_none() {
        let before_cursor = &app.composer.text[..app.composer.cursor];
        let last_line = before_cursor.rsplit('\n').next().unwrap_or("");
        let x = areas[1].x
            + 2
            + (last_line.chars().count() as u16).min(areas[1].width.saturating_sub(5));
        let y = areas[1].y
            + 1
            + (before_cursor.lines().count().saturating_sub(1) as u16)
                .min(areas[1].height.saturating_sub(3));
        frame.set_cursor_position((x, y));
    }
    let context = match (app.estimated_tokens, app.token_budget) {
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
    let mut status = vec![selection];
    if app.status != "ready" {
        status.push(app.status.clone());
    }
    status.extend([
        context,
        cache,
        output,
        format!("{} compactions", app.compactions),
    ]);
    frame.render_widget(
        Paragraph::new(status.join(" · ")).style(Style::default().fg(Color::DarkGray)),
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

fn compact_path(path: &std::path::Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".into(),
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

fn url_host(url: &str) -> &str {
    url.split_once("://")
        .map_or(url, |(_, remainder)| remainder)
        .split('/')
        .next()
        .unwrap_or("")
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
    if let Some(turn_started) = app.turn_started {
        if lines.last().is_some_and(|line| line.style.bg.is_some()) {
            lines.push(Line::raw(" ".repeat(width)));
        }
        let (activity, started) = match app.status.as_str() {
            "compacting" => ("Compacting", app.compaction_started.unwrap_or(turn_started)),
            "searching" => (
                "Searching",
                app.tool_started
                    .as_ref()
                    .map(|(_, started)| *started)
                    .unwrap_or(turn_started),
            ),
            _ => ("Working", turn_started),
        };
        push_message(
            &mut lines,
            "turn_working",
            &format!("{activity} for {}", human_duration(started.elapsed())),
            width,
        );
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
    if role == "turn_end" || role == "turn_working" || role == "compaction_end" {
        lines.push(Line::raw(turn_divider(content, width)));
        lines.push(Line::raw(" ".repeat(width)));
        return;
    }
    if role == "tool" {
        push_tool(lines, content, width);
        return;
    }
    if role == "you" || role == "phi" {
        push_markdown(lines, role, content, width);
        return;
    }
    let style = match role {
        "note" => Style::default().fg(Color::DarkGray),
        "error" => Style::default().fg(Color::Red),
        _ => Style::default(),
    };
    lines.push(Line::styled(" ".repeat(width), style));
    let content_width = width.saturating_sub(2).max(1);
    let mut first = true;
    for content_line in content.split('\n') {
        for wrapped in wrap_line(content_line, content_width) {
            let used = UnicodeWidthStr::width(wrapped.as_str()).min(content_width);
            let marker = if first && role == "you" {
                "‣ "
            } else if first {
                "• "
            } else {
                "  "
            };
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

fn push_markdown(lines: &mut Vec<Line<'static>>, role: &str, content: &str, width: usize) {
    let normal = Color::Rgb(190, 190, 185);
    let strong = Color::Rgb(235, 235, 230);
    let block_style = if role == "you" {
        Style::default().bg(Color::Rgb(38, 40, 45))
    } else {
        Style::default()
    };
    let code_style = PhiMarkdown.code();
    let code_background = Style::default().bg(code_style.bg.unwrap());
    let marker = if role == "you" { "‣ " } else { "• " };
    lines.push(Line::styled(" ".repeat(width), block_style));

    let options = tui_markdown::Options::new(PhiMarkdown);
    let markdown = tui_markdown::from_str_with_options(content, &options);
    let content_width = width.saturating_sub(2).max(1);
    let mut marked = false;
    let mut in_code = false;
    for line in markdown.lines {
        let plain = line.to_string();
        if plain.trim_start().starts_with("```") {
            if in_code {
                lines.push(Line::styled(
                    " ".repeat(width),
                    block_style.patch(code_style),
                ));
                in_code = false;
            } else {
                in_code = true;
                lines.push(Line::styled(
                    " ".repeat(width),
                    block_style.patch(code_style),
                ));
            }
            continue;
        }
        for wrapped in wrap_styled_line(&line, content_width) {
            let has_content = !wrapped.to_string().trim().is_empty();
            let prefix = if !marked && has_content {
                marked = true;
                marker
            } else {
                "  "
            };
            let extra_style = if in_code {
                code_background
            } else {
                Style::default()
            };
            let inherited = wrapped.style.fg.unwrap_or(if in_code {
                code_style.fg.unwrap()
            } else {
                normal
            });
            let style = Style::default()
                .fg(inherited)
                .patch(block_style)
                .patch(wrapped.style)
                .patch(extra_style);
            let mut spans = vec![Span::styled(prefix.to_owned(), style)];
            spans.extend(wrapped.spans.into_iter().map(|span| {
                let mut span_style = Style::default()
                    .fg(inherited)
                    .patch(block_style)
                    .patch(span.style)
                    .patch(extra_style);
                if span_style.add_modifier.contains(Modifier::BOLD) {
                    span_style = span_style.fg(strong);
                }
                Span::styled(span.content.into_owned(), span_style)
            }));
            let used = spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();
            spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), style));
            lines.push(Line::from(spans).style(style));
        }
    }
    if !marked && !content.is_empty() {
        lines.push(Line::styled(
            format!("{marker}{}", " ".repeat(width.saturating_sub(2))),
            block_style,
        ));
    }
    lines.push(Line::styled(" ".repeat(width), block_style));
}

fn wrap_styled_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let mut output = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;
    for span in &line.spans {
        let mut segment = String::new();
        for character in span.content.chars() {
            let character_width = character.width().unwrap_or_default();
            if used + character_width > width && used > 0 {
                if !segment.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut segment), span.style));
                }
                output.push(Line::from(std::mem::take(&mut spans)).style(line.style));
                used = 0;
            }
            segment.push(character);
            used += character_width;
        }
        if !segment.is_empty() {
            spans.push(Span::styled(segment, span.style));
        }
    }
    if !spans.is_empty() || output.is_empty() {
        output.push(Line::from(spans).style(line.style));
    }
    output
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
                session_id: Some("00000000-0000-4000-8000-000000000000".into()),
                allow_shell: false,
                allow_write: false,
                interactive_approvals: true,
            },
            CommandCatalog {
                commands: vec![
                    phi_runtime::CommandSpec {
                        name: "help".into(),
                        usage: "/help".into(),
                        description: "List commands.".into(),
                        source: "core".into(),
                    },
                    phi_runtime::CommandSpec {
                        name: "model".into(),
                        usage: "/model".into(),
                        description: "Select model.".into(),
                        source: "core".into(),
                    },
                ],
                models: vec![phi_runtime::ModelSpec {
                    provider: "test".into(),
                    id: "test/model".into(),
                    model: "model".into(),
                    label: "Test".into(),
                    description: "Test model.".into(),
                    function_tools: true,
                    hosted_tools: Vec::new(),
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
                selected_model: Some("test/model".into()),
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
    fn shows_search_while_running_and_retains_completion() {
        let mut app = app();
        app.turn_started = Some(Instant::now());
        app.on_runtime(RuntimeEvent::ToolStarted {
            name: "web_search".into(),
            arguments: serde_json::json!({}),
        });
        assert_eq!(app.status, "searching");
        assert_eq!(app.transcript[0].1, "Searching the web");
        app.tool_started = Some(("web_search".into(), Instant::now() - Duration::from_secs(3)));

        app.on_runtime(RuntimeEvent::ToolCompleted {
            name: "web_search".into(),
            result: serde_json::json!({
                "action": { "type": "search", "sources": [{}, {}] }
            }),
        });

        assert_eq!(app.status, "working");
        assert_eq!(app.transcript[0].1, "Searched the web · 3s\n\n2 sources");
    }

    #[test]
    fn labels_opened_web_pages_separately() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ToolStarted {
            name: "web_search".into(),
            arguments: serde_json::json!({}),
        });
        app.on_runtime(RuntimeEvent::ToolCompleted {
            name: "web_search".into(),
            result: serde_json::json!({
                "action": {
                    "type": "open_page",
                    "url": "https://rusecure.fifa.com/fifa-world-ranking/men"
                }
            }),
        });

        assert_eq!(app.transcript[0].1, "Opened rusecure.fifa.com · 0s");
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
    fn short_history_is_anchored_above_input() {
        let mut app = app();
        app.transcript.push(("phi".into(), "hello".into()));
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.cell((2, 4)).unwrap().symbol(), "h");
        assert_eq!(buffer.cell((0, 5)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell((0, 6)).unwrap().bg, Color::Rgb(30, 30, 34));
    }

    #[test]
    fn shows_provider_usage_and_cache_counts() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ContextUpdated {
            estimated_tokens: 1_500,
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
    fn shows_async_compaction_and_keeps_a_completion_marker() {
        let mut app = app();
        app.turn_started = Some(Instant::now());
        app.current_model = "answer".into();
        app.on_runtime(RuntimeEvent::ContextUpdated {
            estimated_tokens: 1_500,
            token_budget: 2_000,
            compactions: 0,
            input_tokens: None,
            cached_tokens: None,
            cache_write_tokens: None,
            output_tokens: None,
        });
        app.on_runtime(RuntimeEvent::ActivityChanged {
            activity: "compacting".into(),
        });
        assert_eq!(
            app.transcript.last().unwrap(),
            &("phi".into(), "answer".into())
        );
        assert!(app.current_model.is_empty());
        assert!(
            transcript_text(&app, 80)
                .lines
                .iter()
                .any(|line| line.to_string().contains("Compacting for"))
        );

        app.on_runtime(RuntimeEvent::ContextUpdated {
            estimated_tokens: 200,
            token_budget: 2_000,
            compactions: 1,
            input_tokens: None,
            cached_tokens: None,
            cache_write_tokens: None,
            output_tokens: None,
        });
        assert_eq!(app.transcript.last().unwrap().0, "compaction_end");
        assert!(
            app.transcript
                .last()
                .unwrap()
                .1
                .contains("context 1500 → 200 tokens")
        );

        app.on_runtime(RuntimeEvent::Finished {
            content: "answer".into(),
        });
        assert_eq!(
            app.transcript
                .iter()
                .filter(|(role, content)| role == "phi" && content == "answer")
                .count(),
            1
        );
        assert_eq!(app.transcript[0], ("phi".into(), "answer".into()));
        assert_eq!(app.transcript[1].0, "compaction_end");
        assert_eq!(app.transcript[2].0, "turn_end");
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
        assert!(text.lines[1].to_string().starts_with("‣ hello"));
        assert_eq!(text.lines[3].style.bg, None);
    }

    #[test]
    fn renders_markdown_styles_and_highlighted_code() {
        let mut lines = Vec::new();
        push_message(
            &mut lines,
            "phi",
            "plain **bold** *italic* ~~gone~~ `inline` [link](https://example.com)\n\n```rust\nfn main() {}\n```",
            80,
        );
        let spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect::<Vec<_>>();
        assert!(spans.iter().any(|span| {
            span.content == "bold"
                && span.style.add_modifier.contains(Modifier::BOLD)
                && span.style.fg == Some(Color::Rgb(235, 235, 230))
        }));
        assert!(spans.iter().any(|span| {
            span.content == "plain " && span.style.fg == Some(Color::Rgb(190, 190, 185))
        }));
        assert!(spans.iter().any(|span| {
            span.content == "italic" && span.style.add_modifier.contains(Modifier::ITALIC)
        }));
        assert!(spans.iter().any(|span| {
            span.content == "gone" && span.style.add_modifier.contains(Modifier::CROSSED_OUT)
        }));
        assert!(spans.iter().any(|span| {
            span.content.contains("https://example.com")
                && span.style.add_modifier.contains(Modifier::UNDERLINED)
        }));
        assert!(spans.iter().any(|span| {
            span.content == "inline" && span.style.bg == Some(Color::Rgb(27, 28, 31))
        }));
        assert!(!lines.iter().any(|line| line.to_string().contains("```")));
        let code = lines
            .iter()
            .find(|line| line.to_string().contains("fn main"))
            .unwrap();
        assert_eq!(code.style.bg, Some(Color::Rgb(27, 28, 31)));
        assert!(code.spans.iter().any(|span| span.style.fg.is_some()));
        let code_index = lines
            .iter()
            .position(|line| line.to_string().contains("fn main"))
            .unwrap();
        assert_eq!(lines[code_index - 1].style.bg, Some(Color::Rgb(27, 28, 31)));
        assert_eq!(lines[code_index + 1].style.bg, Some(Color::Rgb(27, 28, 31)));
    }

    #[test]
    fn streaming_does_not_override_manual_scroll() {
        let mut app = app();
        app.follow = false;
        app.on_runtime(RuntimeEvent::ModelDelta {
            content: "partial".into(),
        });
        assert!(!app.follow);
    }

    #[tokio::test]
    async fn accepts_queued_input_but_not_submission_during_a_turn() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = app();
                app.handle = Some(phi_runtime::start(app.options.clone(), "work".into()));
                for character in "next".chars() {
                    app.on_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
                }
                app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                assert_eq!(app.composer.text, "next");
                assert!(app.handle.is_some());
                app.handle.as_ref().unwrap().cancel();
            })
            .await;
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
        assert!(app.follow);
    }

    #[test]
    fn reaching_bottom_resumes_following_streamed_output() {
        let mut app = app();
        app.transcript.push((
            "phi".into(),
            (0..30).map(|n| format!("line {n}\n")).collect(),
        ));
        app.follow = false;
        app.scroll = u16::MAX;
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.follow);

        app.on_runtime(RuntimeEvent::ModelDelta {
            content: "more ".repeat(100),
        });
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.follow);
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

    #[test]
    fn shows_live_working_divider_at_end_of_history() {
        let mut app = app();
        app.current_model = "partial response".into();
        app.turn_started = Some(Instant::now() - Duration::from_secs(133));
        let text = transcript_text(&app, 40);
        assert!(
            text.lines[text.lines.len() - 2]
                .to_string()
                .starts_with("─ Working for 2m ")
        );
        assert!(text.lines.last().unwrap().to_string().trim().is_empty());
    }

    #[test]
    fn working_divider_has_a_gap_after_user_block() {
        let mut app = app();
        app.transcript.push(("you".into(), "hello".into()));
        app.turn_started = Some(Instant::now());
        let text = transcript_text(&app, 40);
        let gap = &text.lines[text.lines.len() - 3];
        assert!(gap.to_string().trim().is_empty());
        assert_eq!(gap.style.bg, None);
        assert!(
            text.lines[text.lines.len() - 2]
                .to_string()
                .starts_with("─ Working for ")
        );
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
            "/help — List commands.\n/model — Select model."
        );
        assert_eq!(app.transcript.last().unwrap().0, "note");
        assert!(!app.transcript.iter().any(|(role, _)| role == "you"));
    }

    #[test]
    fn arrows_select_and_prefill_command_suggestions() {
        let mut app = app();
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(app.command_suggestions().len(), 2);
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.composer.text, "/model");
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.composer.text, "/help");
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.composer.text, "/help ");
    }

    #[test]
    fn model_picker_and_status_use_qualified_model_id() {
        let app = app();
        let options = picker_options(&Picker::Model { selected: 0 }, &app.catalog);
        assert_eq!(options[0].label, "test/model");

        let mut app = app;
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let content = terminal.backend().to_string();
        assert!(content.contains("test/model · low · default"));
        assert!(!content.contains(" · ready · "));
    }

    #[test]
    fn composer_moves_by_character_word_and_line() {
        let mut app = app();
        app.composer.set("one two".into());

        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(app.composer.cursor, 4);
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER));
        assert_eq!(app.composer.cursor, 0);
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SUPER));
        assert_eq!(app.composer.cursor, 7);
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        assert_eq!(app.composer.text, "one tw!o");
    }

    #[test]
    fn word_right_stops_at_the_current_word_end() {
        let mut app = app();
        app.composer.set("one two".into());
        app.composer.cursor = 1;

        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));

        assert_eq!(app.composer.cursor, 3);
    }

    #[test]
    fn modified_backspace_deletes_by_word_or_line() {
        let mut app = app();
        app.composer.set("one two".into());
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(app.composer.text, "one ");

        app.composer.set("one two\nthree".into());
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER));
        assert_eq!(app.composer.text, "one two\n");
    }

    #[test]
    fn up_and_down_browse_user_message_history() {
        let mut app = app();
        app.message_history = vec!["first".into(), "second".into()];
        app.composer.set("draft".into());

        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.composer.text, "second");
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.composer.text, "first");
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.composer.text, "second");
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.composer.text, "draft");
    }

    #[test]
    fn up_moves_within_multiline_input_before_history() {
        let mut app = app();
        app.message_history.push("previous".into());
        app.composer.set("top\nbottom".into());

        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.composer.text, "top\nbottom");
        assert!(app.composer.on_first_line());
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.composer.text, "previous");
    }

    #[test]
    fn welcome_is_local_to_new_threads() {
        let resumed = app();
        let mut options = resumed.options.clone();
        options.session_id = None;
        let fresh = App::new(options, resumed.catalog.clone());
        assert_eq!(fresh.transcript.len(), 1);
        assert_eq!(fresh.transcript[0].0, "note");
        assert!(fresh.transcript[0].1.starts_with("Ready in "));
        assert!(fresh.transcript[0].1.ends_with("Type / for commands."));
        assert!(resumed.transcript.is_empty());
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
