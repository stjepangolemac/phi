use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use futures_util::{FutureExt, StreamExt, future::pending};
use phi_runtime::{
    CommandAction, CommandCatalog, CommandExecution, CommandInvocation, Handle, RunOptions,
    RuntimeCommand, RuntimeEvent,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
};
use throbber_widgets_tui::{BRAILLE_SIX, Throbber, ThrobberState};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod composer;
mod diff_render;

use composer::Composer;

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

struct App {
    options: RunOptions,
    catalog: CommandCatalog,
    session_id: Option<String>,
    transcript: Vec<(String, String)>,
    current_model: String,
    current_model_revision: u64,
    current_model_cache: Option<RenderedLiveModel>,
    current_reasoning_summary: Option<usize>,
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
    throbber_state: ThrobberState,
    context_jobs: BTreeMap<String, String>,
    tool_started: HashMap<String, (String, Instant)>,
    tool_blocks: HashMap<String, usize>,
    workflow_names: HashMap<String, String>,
    shell_streams: HashMap<String, ShellStream>,
    final_response_rendered: bool,
    command_filter: Option<String>,
    command_selected: usize,
    message_history: Vec<String>,
    steering_queue: VecDeque<String>,
    next_turn_queue: VecDeque<String>,
    restart_after_cancel: Option<Vec<String>>,
    history_index: Option<usize>,
    history_draft: String,
    composer_width: usize,
    scroll: usize,
    follow: bool,
    transcript_cache: Vec<Option<RenderedTranscriptBlock>>,
    transcript_offsets: Vec<usize>,
    #[cfg(test)]
    transcript_render_count: usize,
    #[cfg(test)]
    current_model_render_count: usize,
    quit: bool,
}

struct ShellStream {
    heading: String,
    output: String,
}

struct RenderedTranscriptBlock {
    width: usize,
    lines: Vec<Line<'static>>,
}

struct RenderedLiveModel {
    revision: u64,
    block: RenderedTranscriptBlock,
}

impl App {
    fn new(options: RunOptions, catalog: CommandCatalog) -> Self {
        let token_budget = selected_context_window(&catalog);
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
        let transcript_cache = (0..transcript.len()).map(|_| None).collect();
        Self {
            session_id: options.session_id.clone(),
            options,
            catalog,
            transcript,
            current_model: String::new(),
            current_model_revision: 0,
            current_model_cache: None,
            current_reasoning_summary: None,
            composer: Composer::default(),
            handle: None,
            command_task: None,
            picker: None,
            approval: None,
            status: "ready".into(),
            estimated_tokens: token_budget.map(|_| 0),
            token_budget,
            input_tokens: None,
            cached_tokens: None,
            cache_write_tokens: None,
            output_tokens: None,
            compactions: 0,
            turn_started: None,
            compaction_started: None,
            throbber_state: ThrobberState::default(),
            context_jobs: BTreeMap::new(),
            tool_started: HashMap::new(),
            tool_blocks: HashMap::new(),
            workflow_names: HashMap::new(),
            shell_streams: HashMap::new(),
            final_response_rendered: false,
            command_filter: None,
            command_selected: 0,
            message_history: Vec::new(),
            steering_queue: VecDeque::new(),
            next_turn_queue: VecDeque::new(),
            restart_after_cancel: None,
            history_index: None,
            history_draft: String::new(),
            composer_width: 80,
            scroll: 0,
            follow: true,
            transcript_cache,
            transcript_offsets: Vec::new(),
            #[cfg(test)]
            transcript_render_count: 0,
            #[cfg(test)]
            current_model_render_count: 0,
            quit: false,
        }
    }

    fn push_transcript(&mut self, role: impl Into<String>, content: impl Into<String>) {
        self.transcript.push((role.into(), content.into()));
        self.transcript_cache.push(None);
    }

    fn refresh_context_job_status(&mut self) {
        if let Some((job_id, status)) = self.context_jobs.first_key_value() {
            let others = self.context_jobs.len() - 1;
            self.status = if others == 0 {
                format!("context {job_id} {status}")
            } else {
                format!("context {job_id} {status} (+{others})")
            };
        } else {
            self.status = "working".into();
        }
    }

    fn transcript_changed(&mut self, index: usize) {
        if let Some(cache) = self.transcript_cache.get_mut(index) {
            *cache = None;
        }
    }

    fn current_model_changed(&mut self) {
        self.current_model_revision = self.current_model_revision.wrapping_add(1);
        self.current_model_cache = None;
    }

    fn on_tick(&mut self) {
        if self.turn_started.is_some() && self.status != "searching" {
            self.throbber_state.calc_next();
        }
    }

    fn submit(&mut self) {
        if self.handle.is_some() {
            self.queue_for_current_turn();
            return;
        }
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
                    self.push_transcript("note", self.help());
                }
                "compact" if invocation.arguments.is_empty() => self.start_compaction(),
                "compact" => self.push_transcript("error", "usage: /compact"),
                "model" if invocation.arguments.is_empty() => self.open_model_picker(),
                _ => self.start_command(invocation),
            }
            self.follow = true;
            return;
        }
        self.push_transcript("you", prompt.clone());
        self.message_history.push(prompt.clone());
        self.history_index = None;
        self.history_draft.clear();
        self.handle = Some(phi_runtime::start(self.options.clone(), prompt));
        self.turn_started = Some(Instant::now());
        self.final_response_rendered = false;
        self.status = "working".into();
        self.follow = true;
    }

    fn take_composer_message(&mut self) -> Option<String> {
        self.command_filter = None;
        self.command_selected = 0;
        let message = self.composer.take();
        (!message.trim().is_empty()).then_some(message)
    }

    fn queue_for_current_turn(&mut self) {
        let Some(message) = self.take_composer_message() else {
            return;
        };
        if let Some(handle) = &self.handle {
            handle.queue_messages(vec![message.clone()]);
        }
        self.steering_queue.push_back(message);
        self.follow = true;
    }

    fn queue_for_next_turn(&mut self) {
        if self.handle.is_none() {
            return;
        }
        let Some(message) = self.take_composer_message() else {
            return;
        };
        self.next_turn_queue.push_back(message);
        self.follow = true;
    }

    fn start_messages(&mut self, messages: Vec<String>) {
        if messages.is_empty() {
            return;
        }
        self.options.session_id = self.session_id.clone();
        for message in &messages {
            self.push_transcript("you", message.clone());
            self.message_history.push(message.clone());
        }
        self.history_index = None;
        self.history_draft.clear();
        self.handle = Some(phi_runtime::start_messages(self.options.clone(), messages));
        self.turn_started = Some(Instant::now());
        self.final_response_rendered = false;
        self.status = "working".into();
        self.follow = true;
    }

    fn start_next_queued_turn(&mut self) {
        if let Some(message) = self.next_turn_queue.pop_front() {
            self.start_messages(vec![message]);
        }
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

    fn start_compaction(&mut self) {
        self.handle = Some(phi_runtime::compact(self.options.clone()));
        self.turn_started = Some(Instant::now());
        self.final_response_rendered = false;
        self.status = "compacting".into();
    }

    fn on_command(&mut self, result: Result<CommandExecution>) {
        match result {
            Ok(execution) => {
                self.session_id = Some(execution.session_id);
                self.options.session_id = self.session_id.clone();
                self.catalog = execution.catalog;
                self.token_budget = selected_context_window(&self.catalog);
                if execution.action == CommandAction::NewSession {
                    self.reset_chat_state();
                }
                self.push_transcript(execution.role, execution.content);
            }
            Err(error) => self.push_transcript("error", error.to_string()),
        }
        self.status = "ready".into();
    }

    fn reset_chat_state(&mut self) {
        self.transcript.clear();
        self.transcript_cache.clear();
        self.transcript_offsets.clear();
        self.current_model.clear();
        self.current_model_changed();
        self.current_reasoning_summary = None;
        self.approval = None;
        self.estimated_tokens = self.token_budget.map(|_| 0);
        self.input_tokens = None;
        self.cached_tokens = None;
        self.cache_write_tokens = None;
        self.output_tokens = None;
        self.compactions = 0;
        self.turn_started = None;
        self.compaction_started = None;
        self.tool_started.clear();
        self.tool_blocks.clear();
        self.shell_streams.clear();
        self.final_response_rendered = false;
        self.command_filter = None;
        self.command_selected = 0;
        self.message_history.clear();
        self.steering_queue.clear();
        self.next_turn_queue.clear();
        self.restart_after_cancel = None;
        self.history_index = None;
        self.history_draft.clear();
        self.scroll = 0;
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
                self.push_transcript("note", "Model selection cancelled.");
                self.follow = true;
                return;
            }
            KeyCode::Enter => {
                let Some(value) = options.get(*selected).map(|option| option.value.clone()) else {
                    self.push_transcript("error", "No models available.");
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
            RuntimeEvent::QueuedMessagesInjected { contents } => {
                for content in &contents {
                    if self.steering_queue.front() == Some(content) {
                        self.steering_queue.pop_front();
                    } else if let Some(index) = self
                        .steering_queue
                        .iter()
                        .position(|queued| queued == content)
                    {
                        self.steering_queue.remove(index);
                    }
                    self.push_transcript("you", content.clone());
                    self.message_history.push(content.clone());
                }
                self.history_index = None;
                self.history_draft.clear();
                self.follow = true;
            }
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
                    self.push_transcript(
                        "compaction_end",
                        format!(
                            "Compacted in {} · context {} → {} tokens",
                            human_duration(elapsed),
                            before,
                            estimated_tokens
                        ),
                    );
                }
                self.estimated_tokens = Some(estimated_tokens);
                self.token_budget = Some(token_budget);
                self.input_tokens = input_tokens;
                self.cached_tokens = cached_tokens;
                self.cache_write_tokens = cache_write_tokens;
                self.output_tokens = output_tokens;
                self.compactions = compactions;
            }
            RuntimeEvent::CatalogUpdated { catalog } => {
                self.catalog = catalog;
                self.token_budget = selected_context_window(&self.catalog);
            }
            RuntimeEvent::ActivityChanged { activity } => match activity.as_str() {
                "compacting" | "selective_compacting" => {
                    self.final_response_rendered = !self.current_model.is_empty();
                    self.flush_model();
                    self.compaction_started = Some(Instant::now());
                    self.status = "compacting".into();
                }
                "searching" => {
                    self.flush_model();
                    self.status = activity;
                }
                "working" => {
                    if self.context_jobs.is_empty() {
                        self.status = activity;
                    } else {
                        self.refresh_context_job_status();
                    }
                }
                _ => {}
            },
            RuntimeEvent::ContextCompactionStatus { job_id, status } => {
                if matches!(status.as_str(), "queued" | "running") {
                    self.context_jobs.insert(job_id, status);
                } else {
                    self.context_jobs.remove(&job_id);
                    self.push_transcript("note", format!("Context compaction {job_id}: {status}"));
                }
                self.refresh_context_job_status();
            }
            RuntimeEvent::ToolRouteSelected { .. } => {}
            RuntimeEvent::ModelDelta { content } => {
                self.current_reasoning_summary = None;
                self.mark_response_start_after_tools();
                self.current_model.push_str(&content);
                self.current_model_changed();
            }
            RuntimeEvent::ReasoningSummaryDelta { content } => {
                self.flush_model();
                let index = if let Some(index) = self.current_reasoning_summary {
                    index
                } else {
                    let index = self.transcript.len();
                    self.push_transcript("reasoning_summary", "");
                    self.current_reasoning_summary = Some(index);
                    index
                };
                if let Some((_, summary)) = self.transcript.get_mut(index) {
                    summary.push_str(&content);
                    self.transcript_changed(index);
                }
            }
            RuntimeEvent::ToolStarted {
                call_id,
                name,
                arguments,
            } => {
                self.current_reasoning_summary = None;
                self.flush_model();
                self.tool_started
                    .insert(call_id.clone(), (name.clone(), Instant::now()));
                if name == "web_search" {
                    self.status = "searching".into();
                }
                let content = match name.as_str() {
                    "exec_command" => format!("Ran `{}`", display_command(&arguments)),
                    "write_stdin" => "Checked background process".into(),
                    "list_processes" => "Checking background processes".into(),
                    "terminate_process" => "Stopping background process".into(),
                    "read_file" => read_file_label(
                        &self.options.workspace,
                        arguments["path"].as_str().unwrap_or("file"),
                    ),
                    "web_search" => "Searching the web".into(),
                    "patch" => "Applying patch".into(),
                    "reload_config" => "Reloading configuration".into(),
                    "Workflow" => format!(
                        "Starting workflow `{}`",
                        arguments["name"].as_str().unwrap_or("unknown")
                    ),
                    "TaskOutput" => {
                        workflow_action_label("Checking", &arguments, &self.workflow_names)
                    }
                    "TaskStop" => {
                        workflow_action_label("Stopping", &arguments, &self.workflow_names)
                    }
                    _ => format!("Ran `{name}`"),
                };
                if matches!(name.as_str(), "exec_command" | "write_stdin") {
                    self.shell_streams.insert(
                        call_id.clone(),
                        ShellStream {
                            heading: content.clone(),
                            output: String::new(),
                        },
                    );
                }
                let block = self.transcript.len();
                self.push_transcript("tool", content);
                self.tool_blocks.insert(call_id, block);
            }
            RuntimeEvent::ToolOutput {
                call_id,
                name,
                content,
            } => {
                if matches!(name.as_str(), "exec_command" | "write_stdin") {
                    let rendered = self.shell_streams.get_mut(&call_id).map(|stream| {
                        stream.output.push_str(&content);
                        format!(
                            "{}\n\n{}",
                            stream.heading,
                            compact_shell_output(&stream.output)
                        )
                    });
                    if let Some(rendered) = rendered
                        && let Some(index) = self.tool_blocks.get(&call_id).copied()
                        && let Some((_, block)) = self.transcript.get_mut(index)
                    {
                        *block = rendered;
                        self.transcript_changed(index);
                    }
                }
            }
            RuntimeEvent::ToolCompleted {
                call_id,
                name,
                result,
            } => {
                if name == "Workflow"
                    && let (Some(task_id), Some(workflow)) =
                        (result["task_id"].as_str(), result["workflow"].as_str())
                {
                    self.workflow_names
                        .insert(task_id.to_owned(), workflow.to_owned());
                }
                let elapsed = self
                    .tool_started
                    .remove(&call_id)
                    .filter(|(started_name, _)| started_name == &name)
                    .map_or(Duration::ZERO, |(_, started)| started.elapsed());
                let block = self.tool_blocks.remove(&call_id);
                let streamed = self
                    .shell_streams
                    .remove(&call_id)
                    .is_some_and(|stream| !stream.output.is_empty());
                if let Some(index) = block
                    && let Some((role, content)) = self.transcript.get_mut(index)
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
                    } else if name == "patch" {
                        *role = "patch".into();
                        *content = patch_result(&result, elapsed);
                    } else if matches!(name.as_str(), "exec_command" | "write_stdin") {
                        let result_text = shell_result(&result);
                        if !result_text.is_empty() && (!streamed || result.get("error").is_some()) {
                            content.push_str("\n\n");
                            content.push_str(&result_text);
                        }
                        if result["session_id"].as_u64().is_some() {
                            content.push_str(if name == "write_stdin" {
                                "\n\nStill running"
                            } else {
                                "\n\nProcess running"
                            });
                        } else if name == "write_stdin" && result["exit_code"].is_number() {
                            content.push_str("\n\nFinished");
                        } else if let Some(exit_code) = result["exit_code"].as_i64()
                            && exit_code != 0
                        {
                            content.push_str(&format!("\n\nExited with code {exit_code}"));
                        }
                    } else if name == "list_processes" {
                        *content = process_tool_result(&result, elapsed);
                    } else if name == "terminate_process" {
                        if let Some(error) = result["error"].as_str() {
                            *content = format!("Failed to stop background process\n\n{error}");
                        } else if result["status"] == "exited" {
                            *content = format!(
                                "Background process already finished · {}",
                                human_duration(elapsed)
                            );
                        } else {
                            *content = format!(
                                "Stopped background process · {}\n\n{}",
                                human_duration(elapsed),
                                result["signal"].as_str().unwrap_or("terminated")
                            );
                        }
                    } else if name == "reload_config" {
                        if let Some(error) = result["error"].as_str() {
                            *content = format!("Failed to reload configuration\n\n{error}");
                        } else {
                            *content = format!(
                                "Reloaded configuration · {} models · {} commands",
                                result["models"].as_u64().unwrap_or_default(),
                                result["commands"].as_u64().unwrap_or_default()
                            );
                        }
                    } else if matches!(name.as_str(), "Workflow" | "TaskOutput" | "TaskStop") {
                        *content = workflow_tool_result(&name, &result, elapsed);
                    } else {
                        let result = tool_result(&result);
                        if !result.is_empty() {
                            content.push_str("\n\n");
                            content.push_str(&result);
                        }
                    }
                    self.transcript_changed(index);
                }
            }
            RuntimeEvent::ApprovalRequested { name } => self.approval = Some(name),
            RuntimeEvent::Finished { content } => {
                self.current_reasoning_summary = None;
                if !self.final_response_rendered
                    && self.current_model.is_empty()
                    && !content.is_empty()
                {
                    self.mark_response_start_after_tools();
                    self.current_model = content;
                    self.current_model_changed();
                }
                self.flush_model();
                let elapsed = self
                    .turn_started
                    .take()
                    .map_or(Duration::ZERO, |time| time.elapsed());
                self.push_transcript(
                    "turn_end",
                    format!("Worked for {}", human_duration(elapsed)),
                );
                self.handle = None;
                self.compaction_started = None;
                self.tool_started.clear();
                self.tool_blocks.clear();
                self.shell_streams.clear();
                self.final_response_rendered = false;
                self.status = "ready".into();
                if !self.steering_queue.is_empty() {
                    let messages = self.steering_queue.drain(..).collect();
                    self.start_messages(messages);
                } else {
                    self.start_next_queued_turn();
                }
            }
            RuntimeEvent::Error { message } => {
                self.current_reasoning_summary = None;
                self.flush_model();
                let restarting = message == "cancelled" && self.restart_after_cancel.is_some();
                if !restarting {
                    self.push_transcript(
                        "error",
                        if message == "cancelled" {
                            "Cancelled by user".into()
                        } else {
                            message
                        },
                    );
                }
                self.handle = None;
                self.approval = None;
                self.turn_started = None;
                self.compaction_started = None;
                self.tool_started.clear();
                self.tool_blocks.clear();
                self.shell_streams.clear();
                self.final_response_rendered = false;
                self.status = "ready".into();
                if restarting {
                    let mut messages = self.restart_after_cancel.take().unwrap_or_default();
                    messages.extend(self.steering_queue.drain(..));
                    self.start_messages(messages);
                } else {
                    self.restart_after_cancel = None;
                    self.start_next_queued_turn();
                }
            }
        }
    }

    fn flush_model(&mut self) {
        if !self.current_model.is_empty() {
            let content = std::mem::take(&mut self.current_model);
            self.current_model_changed();
            self.push_transcript("phi", content);
        }
    }

    fn mark_response_start_after_tools(&mut self) {
        if self.current_model.is_empty()
            && self
                .transcript
                .last()
                .is_some_and(|(role, _)| matches!(role.as_str(), "tool" | "patch"))
        {
            self.push_transcript("response_start", "");
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

    fn scroll_up(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
        self.follow = false;
    }

    fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_add(lines);
        self.follow = false;
    }

    fn on_mouse(&mut self, kind: MouseEventKind) {
        match kind {
            MouseEventKind::ScrollUp => self.scroll_up(3),
            MouseEventKind::ScrollDown => self.scroll_down(3),
            _ => {}
        }
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
                if !self.steering_queue.is_empty() {
                    let queued = self.steering_queue.drain(..).collect::<Vec<_>>();
                    self.restart_after_cancel
                        .get_or_insert_with(Vec::new)
                        .extend(queued);
                    if let Some(handle) = &self.handle {
                        handle.cancel();
                    }
                    self.status = "cancelling".into();
                } else if !self.next_turn_queue.is_empty() {
                    self.next_turn_queue.clear();
                    self.follow = true;
                } else {
                    if self.status == "cancelling" && self.restart_after_cancel.is_some() {
                        self.restart_after_cancel = None;
                    }
                    if let Some(handle) = &self.handle {
                        handle.cancel();
                    }
                    self.status = "cancelling".into();
                }
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                    && !self.composer_locked() =>
            {
                self.edit_composer();
                self.composer.insert('\n');
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Tab if !self.composer_locked() => self.queue_for_next_turn(),
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
            KeyCode::Char('u')
                if !self.composer_locked() && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.edit_composer();
                self.composer.delete_to_line_start();
            }
            KeyCode::Char(character) if !self.composer_locked() => {
                self.edit_composer();
                self.composer.insert(character);
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => self.scroll_up(3),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => self.scroll_down(3),
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
            KeyCode::Up
                if !self.composer_locked()
                    && self.composer.on_first_visual_row(self.composer_width) =>
            {
                self.previous_message();
            }
            KeyCode::Down if !self.composer_locked() && self.next_message() => {}
            KeyCode::Up if !self.composer_locked() => {
                self.composer.move_up(self.composer_width);
            }
            KeyCode::Down
                if !self.composer_locked()
                    && !self.composer.on_last_visual_row(self.composer_width) =>
            {
                self.composer.move_down(self.composer_width);
            }
            KeyCode::PageUp => {
                self.scroll_up(10);
            }
            KeyCode::PageDown => {
                self.scroll_down(10);
            }
            _ => {}
        }
    }
}

pub async fn launch(options: RunOptions, prompt: Option<String>) -> Result<()> {
    let catalog = phi_runtime::command_catalog(&options)?;
    let update_notice = phi_runtime::plugin_update_notice(&options);
    let mut app = App::new(options, catalog);
    if let Some(notice) = update_notice {
        app.transcript.push(("note".into(), notice));
        app.transcript_cache.push(None);
    }
    if let Some(prompt) = prompt {
        app.composer.set(prompt);
        app.submit();
    }

    let mut terminal = ratatui::init();
    if let Err(error) = crossterm::execute!(std::io::stdout(), EnableMouseCapture) {
        ratatui::restore();
        return Err(error.into());
    }
    let keyboard_enhanced = crossterm::execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok();
    let result = event_loop(&mut terminal, &mut app).await;
    if keyboard_enhanced {
        let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while !app.quit {
        terminal.draw(|frame| draw(frame, app))?;
        tokio::select! {
            _ = tick.tick(), if app.turn_started.is_some() => app.on_tick(),
            event = input.next().fuse() => {
                match event.transpose()? {
                    Some(Event::Key(key)) if key.is_press() => app.on_key(key),
                    Some(Event::Mouse(mouse)) => app.on_mouse(mouse.kind),
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
    let composer_width = frame.area().width.saturating_sub(4).max(1) as usize;
    app.composer_width = composer_width;
    let composer_layout = app.composer.layout(composer_width);
    let max_composer_rows = frame.area().height.saturating_sub(6).max(1) as usize;
    let composer_rows = composer_layout.row_count().min(max_composer_rows).max(1) as u16;
    let composer_area = draw_content(frame, app, frame.area(), composer_layout, composer_rows + 2);
    if let Some(composer_area) = composer_area {
        draw_command_suggestions(frame, app, composer_area);
    }

    if let Some(name) = &app.approval {
        let area = centered(frame.area(), 50, 5);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(format!("Allow {name} once?\n\n[y] yes   [n] no"))
                .style(Style::default().bg(Color::Rgb(45, 40, 25))),
            area,
        );
    }

    if let (Some(picker), Some(composer_area)) = (&app.picker, composer_area) {
        draw_picker(frame, picker, &app.catalog, composer_area);
    }
}

fn draw_content(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    composer_layout: composer::ComposerLayout,
    composer_height: u16,
) -> Option<Rect> {
    let composer_height = composer_height.min(area.height.saturating_sub(1));
    let width = area.width as usize;
    sync_transcript_cache(app, width);
    sync_live_model_cache(app, width);
    let live_tail = live_transcript_tail(app, width);
    let content_height =
        cached_transcript_height(app) + cached_live_model_height(app) + live_tail.len();
    let minimum_transcript_height = (area.height as usize)
        .saturating_sub(composer_height as usize)
        .saturating_sub(1);
    let padding = minimum_transcript_height.saturating_sub(content_height);
    let transcript_height = padding + content_height;
    let document_height = transcript_height + composer_height as usize + 1;
    let max_scroll = document_height.saturating_sub(area.height as usize);
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll == max_scroll {
            app.follow = true;
        }
    }
    frame.render_widget(Clear, area);

    let viewport_start = app.scroll;
    let viewport_end = viewport_start + area.height as usize;
    let visible_content_start = viewport_start.saturating_sub(padding);
    let visible_content_end = viewport_end.saturating_sub(padding).min(content_height);
    if visible_content_start < visible_content_end {
        let lines = transcript_window(app, &live_tail, visible_content_start, visible_content_end);
        let document_y = padding + visible_content_start;
        frame.render_widget(
            Paragraph::new(lines),
            Rect {
                x: area.x,
                y: area.y + (document_y - viewport_start) as u16,
                width: area.width,
                height: (visible_content_end - visible_content_start) as u16,
            },
        );
    }

    let composer_offset = transcript_height.saturating_sub(viewport_start);
    let composer_area = if composer_offset < area.height as usize {
        let composer_area = Rect {
            x: area.x,
            y: area.y + composer_offset as u16,
            width: area.width,
            height: composer_height.min(area.height - composer_offset as u16),
        };
        draw_composer(frame, app, composer_area, composer_height, composer_layout);
        Some(composer_area)
    } else {
        None
    };

    let status_offset = transcript_height + composer_height as usize;
    if status_offset >= viewport_start && status_offset < viewport_end {
        draw_status(
            frame,
            app,
            Rect {
                x: area.x,
                y: area.y + (status_offset - viewport_start) as u16,
                width: area.width,
                height: 1,
            },
        );
    }
    composer_area
}

fn draw_composer(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    full_height: u16,
    layout: composer::ComposerLayout,
) {
    let visible_height = full_height.saturating_sub(2) as usize;
    let row_offset = layout
        .cursor_row()
        .saturating_add(1)
        .saturating_sub(visible_height);
    let lines = layout
        .visible_rows(&app.composer.text, row_offset, visible_height)
        .map(|row| Line::raw(row.to_owned()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(30, 30, 34))),
        area,
    );

    let text_height = area.height.saturating_sub(1).min(visible_height as u16);
    if text_height > 0 {
        frame.render_widget(
            Paragraph::new(lines),
            Rect {
                x: area.x + 2,
                y: area.y + 1,
                width: area.width.saturating_sub(4),
                height: text_height,
            },
        );
    }

    if !app.composer_locked() && app.approval.is_none() && visible_height > 0 {
        let inner_width = area.width.saturating_sub(4).max(1);
        let x = area.x + 2 + (layout.cursor_column() as u16).min(inner_width.saturating_sub(1));
        let y = area.y + 1 + layout.cursor_row().saturating_sub(row_offset) as u16;
        if y < area.bottom() {
            frame.set_cursor_position((x, y));
        }
    }
}

fn draw_command_suggestions(frame: &mut Frame, app: &App, composer: Rect) {
    let suggestions = if app.picker.is_none() {
        app.command_suggestions()
    } else {
        Vec::new()
    };
    if !suggestions.is_empty() {
        let height = (suggestions.len() as u16).min(composer.y);
        let area = Rect {
            x: composer.x,
            y: composer.y.saturating_sub(height),
            width: composer.width,
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
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let context = match (app.estimated_tokens, app.token_budget) {
        (Some(used), Some(budget)) => {
            format!("{}/{} tokens", human_tokens(used), human_tokens(budget))
        }
        _ => "—".into(),
    };
    let model = app.catalog.selected_model.as_deref().unwrap_or("model —");
    let reasoning = app.catalog.selected_reasoning.as_deref().unwrap_or("—");
    let tier = app.catalog.selected_service_tier.as_deref();
    let selection = tier.map_or_else(
        || format!("{model} {reasoning}"),
        |tier| format!("{model} {reasoning} {tier}"),
    );
    let mut status = vec![selection];
    if app.status != "ready" {
        status.push(app.status.clone());
    }
    status.extend([context, format!("{} compactions", app.compactions)]);
    frame.render_widget(
        Paragraph::new(status.join(" · ")).style(Style::default().fg(Color::DarkGray)),
        area,
    );
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

fn sync_transcript_cache(app: &mut App, width: usize) {
    let mut first_changed = (app.transcript_offsets.len() != app.transcript.len())
        .then_some(app.transcript_offsets.len().min(app.transcript.len()));
    app.transcript_cache.truncate(app.transcript.len());
    app.transcript_cache
        .resize_with(app.transcript.len(), || None);
    for (index, (role, content)) in app.transcript.iter().enumerate() {
        let stale = app.transcript_cache[index]
            .as_ref()
            .is_none_or(|cache| cache.width != width);
        if stale {
            first_changed = Some(first_changed.map_or(index, |changed| changed.min(index)));
            let mut lines = Vec::new();
            if role == "response_start" && index > 0 {
                lines.push(Line::raw(" ".repeat(width)));
            }
            push_message(&mut lines, role, content, width);
            app.transcript_cache[index] = Some(RenderedTranscriptBlock { width, lines });
            #[cfg(test)]
            {
                app.transcript_render_count += 1;
            }
        }
    }
    if let Some(first_changed) = first_changed {
        app.transcript_offsets.truncate(first_changed);
        let mut offset = app.transcript_offsets.last().copied().unwrap_or_default();
        for block in app.transcript_cache[first_changed..]
            .iter()
            .filter_map(Option::as_ref)
        {
            offset += block.lines.len();
            app.transcript_offsets.push(offset);
        }
    }
}

fn cached_transcript_height(app: &App) -> usize {
    app.transcript_offsets.last().copied().unwrap_or_default()
}

fn sync_live_model_cache(app: &mut App, width: usize) {
    if !app.current_model.is_empty() {
        let stale = app.current_model_cache.as_ref().is_none_or(|cache| {
            cache.revision != app.current_model_revision || cache.block.width != width
        });
        if stale {
            let mut model_lines = Vec::new();
            push_message(&mut model_lines, "phi", &app.current_model, width);
            app.current_model_cache = Some(RenderedLiveModel {
                revision: app.current_model_revision,
                block: RenderedTranscriptBlock {
                    width,
                    lines: model_lines,
                },
            });
            #[cfg(test)]
            {
                app.current_model_render_count += 1;
            }
        }
    }
}

fn cached_live_model_height(app: &App) -> usize {
    app.current_model_cache
        .as_ref()
        .map_or(0, |cache| cache.block.lines.len())
}

fn live_transcript_tail(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(turn_started) = app.turn_started {
        let last_line = app
            .current_model_cache
            .as_ref()
            .and_then(|cache| cache.block.lines.last())
            .or_else(|| {
                app.transcript_cache
                    .iter()
                    .rev()
                    .filter_map(Option::as_ref)
                    .find_map(|block| block.lines.last())
            });
        if last_line
            .is_some_and(|line| !line.to_string().trim().is_empty() || line.style.bg.is_some())
        {
            lines.push(Line::raw(" ".repeat(width)));
        }
        let (activity, started) = match app.status.as_str() {
            "compacting" => ("Compacting", app.compaction_started.unwrap_or(turn_started)),
            "searching" => (
                "Searching",
                app.tool_started
                    .values()
                    .filter(|(name, _)| name == "web_search")
                    .map(|(_, started)| *started)
                    .min()
                    .unwrap_or(turn_started),
            ),
            _ => ("Working", turn_started),
        };
        let label = format!("{activity} for {}", human_duration(started.elapsed()));
        if activity == "Searching" {
            push_message(&mut lines, "turn_working", &label, width);
        } else {
            lines.push(activity_indicator(&app.throbber_state, &label, width));
            lines.push(Line::raw(" ".repeat(width)));
        }
    }
    push_message_queue(
        &mut lines,
        "Queued after tool call:",
        &app.steering_queue,
        width,
        Color::LightYellow,
    );
    push_message_queue(
        &mut lines,
        "Queued for next turn:",
        &app.next_turn_queue,
        width,
        Color::LightBlue,
    );
    if lines
        .last()
        .or_else(|| {
            app.current_model_cache
                .as_ref()
                .and_then(|cache| cache.block.lines.last())
        })
        .or_else(|| {
            app.transcript_cache
                .iter()
                .rev()
                .filter_map(Option::as_ref)
                .find_map(|block| block.lines.last())
        })
        .is_some_and(|line| !line.to_string().trim().is_empty() || line.style.bg.is_some())
    {
        lines.push(Line::raw(" ".repeat(width)));
    }
    lines
}

fn push_message_queue(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    messages: &VecDeque<String>,
    width: usize,
    color: Color,
) {
    if messages.is_empty() || width == 0 {
        return;
    }
    let style = Style::default().fg(color);
    lines.push(Line::styled(
        truncate_width(&format!("• {label}"), width),
        style.add_modifier(Modifier::BOLD),
    ));
    for message in messages {
        let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
        lines.push(Line::styled(
            truncate_width(&format!("  └ {message}"), width),
            style,
        ));
    }
}

fn transcript_window(
    app: &App,
    live_tail: &[Line<'static>],
    start: usize,
    end: usize,
) -> Vec<Line<'static>> {
    let mut output = Vec::with_capacity(end.saturating_sub(start));
    let first_block = app
        .transcript_offsets
        .partition_point(|offset| *offset <= start);
    let mut offset = first_block
        .checked_sub(1)
        .and_then(|index| app.transcript_offsets.get(index))
        .copied()
        .unwrap_or_default();
    for lines in app.transcript_cache[first_block..]
        .iter()
        .filter_map(Option::as_ref)
        .map(|block| block.lines.as_slice())
        .chain(
            app.current_model_cache
                .as_ref()
                .map(|cache| cache.block.lines.as_slice()),
        )
        .chain(std::iter::once(live_tail))
    {
        let block_end = offset + lines.len();
        if block_end > start && offset < end {
            let local_start = start.saturating_sub(offset);
            let local_end = (end - offset).min(lines.len());
            output.extend(lines[local_start..local_end].iter().cloned());
        }
        offset = block_end;
        if offset >= end {
            break;
        }
    }
    output
}

#[cfg(test)]
fn transcript_text(app: &mut App, width: usize) -> ratatui::text::Text<'static> {
    sync_transcript_cache(app, width);
    sync_live_model_cache(app, width);
    let live_tail = live_transcript_tail(app, width);
    let height = cached_transcript_height(app) + cached_live_model_height(app) + live_tail.len();
    ratatui::text::Text::from(transcript_window(app, &live_tail, 0, height))
}

fn push_message(lines: &mut Vec<Line<'static>>, role: &str, content: &str, width: usize) {
    if role == "turn_end" || role == "turn_working" || role == "compaction_end" {
        lines.push(Line::raw(turn_divider(content, width)));
        lines.push(Line::raw(" ".repeat(width)));
        return;
    }
    if role == "response_start" {
        lines.push(Line::raw(turn_divider("", width)));
        return;
    }
    if role == "tool" {
        push_tool(lines, content, width);
        return;
    }
    if role == "patch" {
        push_patch(lines, content, width);
        return;
    }
    if role == "reasoning_summary" {
        push_reasoning_summary(lines, content, width);
        return;
    }
    if role == "you" || role == "phi" || role == "processes" {
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

fn push_reasoning_summary(lines: &mut Vec<Line<'static>>, content: &str, width: usize) {
    let label_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD | Modifier::ITALIC);
    let content_style = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::ITALIC);
    lines.push(Line::raw(" ".repeat(width)));
    lines.push(Line::styled(
        truncate_width("• Provider reasoning summary", width),
        label_style,
    ));
    let content_width = width.saturating_sub(2).max(1);
    for content_line in content.split('\n') {
        for wrapped in wrap_line(content_line, content_width) {
            let used = UnicodeWidthStr::width(wrapped.as_str()).min(content_width);
            lines.push(Line::styled(
                format!("  {wrapped}{}", " ".repeat(width.saturating_sub(2 + used))),
                content_style,
            ));
        }
    }
    lines.push(Line::raw(" ".repeat(width)));
}

fn activity_indicator(state: &ThrobberState, label: &str, width: usize) -> Line<'static> {
    const INSET: usize = 2;

    let mut line = Line::from(Span::raw(" ".repeat(width.min(INSET))));
    let content_width = width.saturating_sub(INSET);
    if content_width == 0 {
        return line;
    }
    let throbber = Throbber::default().throbber_set(BRAILLE_SIX);
    if content_width == 1 {
        let mut symbol = throbber.to_symbol_span(state);
        symbol.content = symbol.content.chars().take(1).collect::<String>().into();
        line.spans.push(symbol);
        return line;
    }
    let label_width = content_width - 2;
    let mut label = truncate_width(label, label_width);
    let used = UnicodeWidthStr::width(label.as_str());
    label.push_str(&" ".repeat(label_width.saturating_sub(used)));
    line.spans
        .extend(throbber.label(label).to_line(state).spans);
    line
}

fn push_markdown(lines: &mut Vec<Line<'static>>, role: &str, content: &str, width: usize) {
    let normal = Color::Rgb(190, 190, 185);
    let strong = Color::Rgb(235, 235, 230);
    let block_style = if role == "you" {
        Style::default().bg(Color::Rgb(38, 40, 45))
    } else {
        Style::default()
    };
    let code_inset = usize::from(role == "processes") * 2;
    let code_padding = code_inset;
    lines.push(Line::styled(" ".repeat(width), block_style));

    let options = tui_markdown::Options::new(PhiMarkdown);
    let markdown = tui_markdown::from_str_with_options(content, &options);
    let marker = if role == "you" { "‣ " } else { "• " };
    let content_width = width.saturating_sub(2).max(1);
    let mut marked = false;
    let mut in_code = false;
    let mut markdown_lines = markdown.lines.into_iter().peekable();
    while let Some(mut line) = markdown_lines.next() {
        let marker_text = line.to_string();
        if is_ordered_list_marker(&marker_text)
            && let Some(next) = markdown_lines.peek()
            && !next.to_string().trim().is_empty()
        {
            let item = markdown_lines.next().expect("peeked ordered list item");
            if !marker_text.chars().last().is_some_and(char::is_whitespace) {
                line.spans.push(Span::raw(" "));
            }
            line.spans.extend(item.spans);
        }
        let plain = line.to_string();
        if plain.trim_start().starts_with("```") {
            if in_code {
                lines.push(Line::styled(" ".repeat(width), block_style));
                in_code = false;
            } else {
                in_code = true;
                lines.push(Line::styled(" ".repeat(width), block_style));
            }
            continue;
        }
        let ordered_list_line = !in_code && role != "you" && is_ordered_list_line(&plain);
        let line_width = if ordered_list_line {
            width.max(1)
        } else if in_code && code_inset > 0 {
            width
                .saturating_sub(code_inset * 2 + code_padding * 2)
                .max(1)
        } else {
            content_width
        };
        for wrapped in wrap_styled_line(&line, line_width) {
            let has_content = !wrapped.to_string().trim().is_empty();
            let prefix = if ordered_list_line {
                if has_content {
                    marked = true;
                }
                ""
            } else if !marked && has_content {
                marked = true;
                marker
            } else {
                "  "
            };
            let inherited = wrapped.style.fg.unwrap_or(normal);
            let style = Style::default()
                .fg(inherited)
                .patch(block_style)
                .patch(wrapped.style);
            let prefix_style = if in_code && code_inset > 0 {
                block_style
            } else {
                style
            };
            let line_style = if in_code && code_inset > 0 {
                block_style
            } else {
                style
            };
            let mut spans = vec![Span::styled(prefix.to_owned(), prefix_style)];
            if in_code && code_padding > 0 {
                spans.push(Span::styled(" ".repeat(code_padding), style));
            }
            spans.extend(wrapped.spans.into_iter().map(|span| {
                let mut span_style = Style::default()
                    .fg(inherited)
                    .patch(block_style)
                    .patch(span.style);
                if span_style.add_modifier.contains(Modifier::BOLD) {
                    span_style = span_style.fg(strong);
                }
                Span::styled(span.content.into_owned(), span_style)
            }));
            let used = spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();
            if in_code && code_inset > 0 {
                spans.push(Span::styled(
                    " ".repeat(width.saturating_sub(used + code_inset)),
                    style,
                ));
                spans.push(Span::styled(" ".repeat(code_inset), block_style));
            } else {
                spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), style));
            }
            lines.push(Line::from(spans).style(line_style));
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

fn is_ordered_list_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && trimmed[digits..].starts_with(". ")
}

fn is_ordered_list_marker(line: &str) -> bool {
    let trimmed = line.trim();
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && &trimmed[digits..] == "."
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
        let output = output
            .split('\n')
            .map(|line| truncate_middle_width(line, width.saturating_sub(4).max(1)))
            .collect::<Vec<_>>();
        for (index, output) in output.iter().enumerate() {
            let prefix = if index == 0 { "  └ " } else { "    " };
            lines.push(Line::styled(format!("{prefix}{output}"), output_style));
        }
    }
}

fn push_patch(lines: &mut Vec<Line<'static>>, content: &str, width: usize) {
    diff_render::push(lines, content, width);
}

fn turn_divider(label: &str, width: usize) -> String {
    if label.is_empty() {
        return "─".repeat(width);
    }
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

fn truncate_middle_width(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let available = width - 1;
    let left_width = available.div_ceil(2);
    let right_width = available / 2;
    let left = value
        .chars()
        .scan(0, |used, character| {
            *used += character.width().unwrap_or_default();
            (*used <= left_width).then_some(character)
        })
        .collect::<String>();
    let mut right = value
        .chars()
        .rev()
        .scan(0, |used, character| {
            *used += character.width().unwrap_or_default();
            (*used <= right_width).then_some(character)
        })
        .collect::<String>();
    right = right.chars().rev().collect();
    format!("{left}…{right}")
}

fn display_command(arguments: &serde_json::Value) -> String {
    arguments
        .get("cmd")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("command")
        .to_owned()
}

fn process_tool_result(result: &serde_json::Value, elapsed: Duration) -> String {
    if let Some(error) = result.get("error").and_then(serde_json::Value::as_str) {
        return format!("Failed to list background processes\n\n{error}");
    }
    let processes = result["processes"].as_array().cloned().unwrap_or_default();
    if processes.is_empty() {
        return format!("No background processes · {}", human_duration(elapsed));
    }
    let output = processes
        .iter()
        .map(|process| {
            let status = if process["status"].as_str() == Some("running") {
                format!(
                    "Running for {}",
                    human_duration(Duration::from_millis(
                        process["elapsed_ms"].as_u64().unwrap_or_default()
                    ))
                )
            } else if let Some(code) = process["exit_code"].as_i64() {
                format!("Finished · exit {code}")
            } else {
                "Finished".into()
            };
            format!(
                "{status} · {}",
                process["command"].as_str().unwrap_or("command")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Checked background processes · {}\n\n{output}",
        human_duration(elapsed),
    )
}

fn display_relative_path(workspace: &Path, path: &str) -> String {
    Path::new(path)
        .strip_prefix(workspace)
        .unwrap_or_else(|_| Path::new(path))
        .display()
        .to_string()
        .trim_start_matches("./")
        .to_owned()
}

fn patch_result(result: &serde_json::Value, elapsed: Duration) -> String {
    if let Some(error) = result.get("error").and_then(serde_json::Value::as_str) {
        let error = error.split_whitespace().collect::<Vec<_>>().join(" ");
        return format!(
            "Patch failed · {} · {}",
            human_duration(elapsed),
            truncate_display(&error, 2_000)
        );
    }
    let changes = result
        .get("changes")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let descriptions = changes
        .iter()
        .map(|change| {
            let path = change["path"].as_str().unwrap_or("unknown");
            let description = match change["operation"].as_str() {
                Some("create") => format!("Created `{path}`"),
                Some("replace") => format!("Updated `{path}`"),
                Some("delete") => format!("Deleted `{path}`"),
                Some("move") => format!(
                    "Moved `{path}` → `{}`",
                    change["destination"].as_str().unwrap_or("unknown")
                ),
                _ => format!("Changed `{path}`"),
            };
            (
                description,
                change["diff"]
                    .as_str()
                    .unwrap_or("")
                    .trim_end_matches(['\r', '\n']),
            )
        })
        .collect::<Vec<_>>();
    if descriptions.len() == 1 {
        return format!(
            "{} · {}\n\n{}",
            descriptions[0].0,
            human_duration(elapsed),
            descriptions[0].1
        );
    }
    let details = descriptions
        .iter()
        .map(|(description, diff)| format!("{description}\n{diff}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "Patched {} files · {}\n\n{}",
        descriptions.len(),
        human_duration(elapsed),
        details
    )
}

fn read_file_label(workspace: &Path, path: &str) -> String {
    if let Some(resource) = path.strip_prefix("skill://") {
        if resource.ends_with("/SKILL.md") {
            let name = resource.trim_end_matches("/SKILL.md");
            return format!("Read skill `{name}`");
        }
        return format!("Read skill resource `{resource}`");
    }
    format!("Read {}", display_relative_path(workspace, path))
}

fn workflow_action_label(
    action: &str,
    arguments: &serde_json::Value,
    names: &HashMap<String, String>,
) -> String {
    arguments["task_id"]
        .as_str()
        .and_then(|task_id| names.get(task_id))
        .map_or_else(
            || format!("{action} workflow"),
            |workflow| format!("{action} workflow `{workflow}`"),
        )
}

fn workflow_tool_result(name: &str, result: &serde_json::Value, elapsed: Duration) -> String {
    if let Some(error) = result.get("error").and_then(serde_json::Value::as_str) {
        let action = match name {
            "Workflow" => "start",
            "TaskOutput" => "check",
            "TaskStop" => "stop",
            _ => "run",
        };
        return format!(
            "Failed to {action} workflow\n\n{}",
            truncate_display(error, 2_000)
        );
    }

    let workflow = result["workflow"]
        .as_str()
        .or_else(|| result["state"]["workflow"].as_str())
        .unwrap_or("unknown");
    match name {
        "Workflow" => format!(
            "Started workflow `{workflow}` · {}\n\nRunning in background",
            human_duration(elapsed)
        ),
        "TaskOutput" => {
            let status = result["status"].as_str().unwrap_or("unknown");
            let duration = workflow_duration(result, elapsed);
            let label = match status {
                "pending" | "running" => "still running",
                "completed" => "completed",
                "failed" => "failed",
                "cancelled" => "cancelled",
                other => other,
            };
            let mut output = format!(
                "Workflow `{workflow}` {label} · {}",
                human_duration(duration)
            );
            let mut details = workflow_summary(result);
            if status == "failed"
                && let Some(error) = result["state"]["error"].as_str()
            {
                details.push(truncate_display(error, 2_000));
            }
            if !details.is_empty() {
                output.push_str("\n\n");
                output.push_str(&details.join("\n"));
            }
            output
        }
        "TaskStop" => match result["status"].as_str() {
            Some("cancelled") => format!("Stopped workflow `{workflow}`"),
            Some("not_running") => format!("Workflow `{workflow}` is not running"),
            Some(status) => format!("Workflow `{workflow}`: {status}"),
            None => format!("Stopped workflow `{workflow}`"),
        },
        _ => format!("Ran workflow `{workflow}`"),
    }
}

fn workflow_duration(result: &serde_json::Value, fallback: Duration) -> Duration {
    let Some(started) = result["state"]["startedAt"].as_u64() else {
        return fallback;
    };
    let ended = result["state"]["completedAt"].as_u64().or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
    });
    ended
        .and_then(|ended| ended.checked_sub(started))
        .map(Duration::from_millis)
        .unwrap_or(fallback)
}

fn workflow_summary(result: &serde_json::Value) -> Vec<String> {
    let summary = &result["summary"];
    let mut details = Vec::new();
    if let Some(phase) = summary["phase"].as_str() {
        details.push(format!("Phase: {phase}"));
    }

    let agents = &summary["agents"];
    let running = agents["running"].as_u64().unwrap_or_default();
    let completed = agents["completed"].as_u64().unwrap_or_default();
    let failed = agents["failed"].as_u64().unwrap_or_default();
    let mut counts = Vec::new();
    if running > 0 {
        counts.push(format!("{running} running"));
    }
    if completed > 0 {
        counts.push(format!("{completed} completed"));
    }
    if failed > 0 {
        counts.push(format!("{failed} failed"));
    }
    if !counts.is_empty() {
        details.push(format!("Agents: {}", counts.join(" · ")));
    }
    if let Some(log) = summary["latestLog"].as_str() {
        details.push(format!("Latest: {}", truncate_display(log, 500)));
    }
    details
}

fn tool_result(result: &serde_json::Value) -> String {
    truncate_display(&raw_process_result(result), 2_000)
}

fn shell_result(result: &serde_json::Value) -> String {
    compact_shell_output(&raw_process_result(result))
}

fn raw_process_result(result: &serde_json::Value) -> String {
    if let Some(error) = result.get("error").and_then(serde_json::Value::as_str) {
        return error.to_owned();
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
    output
}

fn compact_shell_output(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n");
    let mut lines = normalized.split('\n').collect::<Vec<_>>();
    if normalized.ends_with('\n') {
        lines.pop();
    }
    if lines.len() <= 4 {
        return lines.join("\n");
    }
    let omitted = lines.len() - 4;
    let unit = if omitted == 1 { "line" } else { "lines" };
    format!(
        "{}\n{}\n… (+ {omitted} {unit})\n{}\n{}",
        lines[0],
        lines[1],
        lines[lines.len() - 2],
        lines[lines.len() - 1]
    )
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

fn selected_context_window(catalog: &CommandCatalog) -> Option<u64> {
    let selected = catalog.selected_model.as_deref()?;
    catalog
        .models
        .iter()
        .find(|model| model.id == selected)
        .map(|model| model.context_window)
}

fn human_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    let thousands = tokens as f64 / 1_000.0;
    let formatted = if thousands >= 100.0 {
        return format!("{thousands:.0}K");
    } else if thousands >= 10.0 {
        format!("{thousands:.1}")
    } else {
        format!("{thousands:.2}")
    };
    format!("{}K", formatted.trim_end_matches('0').trim_end_matches('.'))
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
    use ratatui::{Terminal, backend::TestBackend, layout::Position};

    fn app() -> App {
        App::new(
            RunOptions {
                workspace: ".".into(),
                config_path: "phi.json".into(),
                session_id: Some("00000000-0000-4000-8000-000000000000".into()),
                allow_shell: false,
                allow_write: false,
                interactive_approvals: true,
                full_access: false,
                processes: std::sync::Arc::new(phi_runtime::ProcessManager::default()),
                workflows: std::sync::Arc::new(phi_runtime::WorkflowTasks::default()),
                output_schema: None,
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
                    context_window: 1_000,
                    compaction_token_limit: 900,
                    strict_json_schema_capable: false,
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
            call_id: "read".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({ "path": "src/main.rs" }),
        });
        assert_eq!(app.transcript[0], ("phi".into(), "hi".into()));
        assert_eq!(app.transcript[1].1, "Read src/main.rs");
    }

    #[test]
    fn shows_context_compaction_lifecycle_without_blocking_the_transcript() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ContextCompactionStatus {
            job_id: "J1".into(),
            status: "queued".into(),
        });
        assert_eq!(app.status, "context J1 queued");
        assert!(app.transcript.is_empty());

        app.on_runtime(RuntimeEvent::ContextCompactionStatus {
            job_id: "J1".into(),
            status: "running".into(),
        });
        assert_eq!(app.status, "context J1 running");
        app.on_runtime(RuntimeEvent::ContextCompactionStatus {
            job_id: "J2".into(),
            status: "running".into(),
        });
        assert_eq!(app.status, "context J1 running (+1)");
        app.on_runtime(RuntimeEvent::ContextCompactionStatus {
            job_id: "J1".into(),
            status: "applied".into(),
        });
        assert_eq!(app.status, "context J2 running");
        assert_eq!(
            app.transcript[0],
            ("note".into(), "Context compaction J1: applied".into())
        );
        app.on_runtime(RuntimeEvent::ContextCompactionStatus {
            job_id: "J2".into(),
            status: "failed".into(),
        });
        assert_eq!(app.status, "working");
    }

    #[test]
    fn streams_reasoning_summaries_separately_and_preserves_event_order() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ReasoningSummaryDelta {
            content: "Checked ".into(),
        });
        app.on_runtime(RuntimeEvent::ReasoningSummaryDelta {
            content: "the request.".into(),
        });
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "read".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({ "path": "src/main.rs" }),
        });
        app.on_runtime(RuntimeEvent::ModelDelta {
            content: "Final answer".into(),
        });
        app.on_runtime(RuntimeEvent::Finished {
            content: "Final answer".into(),
        });

        assert_eq!(
            &app.transcript[..3],
            &[
                ("reasoning_summary".into(), "Checked the request.".into()),
                ("tool".into(), "Read src/main.rs".into()),
                ("response_start".into(), String::new()),
            ]
        );
        assert_eq!(app.transcript[3], ("phi".into(), "Final answer".into()));
        let rendered = transcript_text(&mut app, 48).to_string();
        assert!(rendered.contains("Provider reasoning summary"));
        assert!(rendered.contains("Checked the request."));
        assert_eq!(rendered.matches("Final answer").count(), 1);
    }

    #[test]
    fn labels_user_cancellation_clearly() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::Error {
            message: "cancelled".into(),
        });
        assert_eq!(
            app.transcript[0],
            ("error".into(), "Cancelled by user".into())
        );
    }

    #[test]
    fn shows_search_while_running_and_retains_completion() {
        let mut app = app();
        app.turn_started = Some(Instant::now());
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "search".into(),
            name: "web_search".into(),
            arguments: serde_json::json!({}),
        });
        assert_eq!(app.status, "searching");
        assert_eq!(app.transcript[0].1, "Searching the web");
        app.tool_started.insert(
            "search".into(),
            ("web_search".into(), Instant::now() - Duration::from_secs(3)),
        );

        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "search".into(),
            name: "web_search".into(),
            result: serde_json::json!({
                "action": { "type": "search", "sources": [{}, {}] }
            }),
        });

        assert_eq!(app.status, "working");
        assert_eq!(app.transcript[0].1, "Searched the web · 3s\n\n2 sources");
    }

    #[test]
    fn shows_skill_reads_without_echoing_instructions() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "skill".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({ "path": "skill://review/SKILL.md" }),
        });
        assert_eq!(app.transcript[0].1, "Read skill `review`");
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "skill".into(),
            name: "read_file".into(),
            result: serde_json::json!({
                "path": "skill://review/SKILL.md",
                "content": "secret instructions"
            }),
        });
        assert_eq!(app.transcript[0].1, "Read skill `review`");
        assert!(!app.transcript[0].1.contains("secret instructions"));

        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "reference".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({ "path": "skill://review/references/details.md" }),
        });
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "reference".into(),
            name: "read_file".into(),
            result: serde_json::json!({
                "path": "skill://review/references/details.md",
                "content": "more secret instructions"
            }),
        });
        assert_eq!(
            app.transcript[1].1,
            "Read skill resource `review/references/details.md`"
        );
        assert!(!app.transcript[1].1.contains("more secret instructions"));
    }

    #[test]
    fn shows_completed_patch_operations() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "patch".into(),
            name: "patch".into(),
            arguments: serde_json::json!({ "patch": "ignored by the UI" }),
        });
        assert_eq!(app.transcript[0].1, "Applying patch");
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "patch".into(),
            name: "patch".into(),
            result: serde_json::json!({
                "changes": [
                    {
                        "operation": "create",
                        "path": "new.rs",
                        "diff": "--- /dev/null\n+++ new.rs\n@@ -0,0 +1 @@\n+new\n"
                    },
                    {
                        "operation": "replace",
                        "path": "src/main.rs",
                        "diff": "--- src/main.rs\n+++ src/main.rs\n@@ -1 +1 @@\n-old\n+new\n"
                    },
                    {
                        "operation": "move",
                        "path": "old.rs",
                        "destination": "moved.rs",
                        "diff": "--- old.rs\n+++ moved.rs\n"
                    },
                    {
                        "operation": "delete",
                        "path": "unused.rs",
                        "diff": "--- unused.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-unused\n"
                    },
                ]
            }),
        });
        assert_eq!(
            app.transcript[0].1,
            concat!(
                "Patched 4 files · 0s\n\n",
                "Created `new.rs`\n--- /dev/null\n+++ new.rs\n@@ -0,0 +1 @@\n+new\n\n",
                "Updated `src/main.rs`\n--- src/main.rs\n+++ src/main.rs\n@@ -1 +1 @@\n-old\n+new\n\n",
                "Moved `old.rs` → `moved.rs`\n--- old.rs\n+++ moved.rs\n\n",
                "Deleted `unused.rs`\n--- unused.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-unused"
            )
        );
    }

    #[test]
    fn keeps_patch_failures_on_one_line() {
        let content = patch_result(
            &serde_json::json!({
                "error": "failed to match hunk\nexpected nearby context"
            }),
            Duration::from_secs(2),
        );
        assert_eq!(
            content,
            "Patch failed · 2s · failed to match hunk expected nearby context"
        );

        let mut lines = Vec::new();
        push_message(&mut lines, "patch", &content, 80);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].to_string().trim().is_empty());
        assert_eq!(lines[1].to_string(), content);
    }

    #[test]
    fn labels_opened_web_pages_separately() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "page".into(),
            name: "web_search".into(),
            arguments: serde_json::json!({}),
        });
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "page".into(),
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
    fn renders_one_row_terminal_without_hiding_status() {
        let mut app = app();
        app.push_transcript("phi", "hello");
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(terminal.backend().to_string().contains("test/model"));
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
    fn composer_grows_upward_for_soft_wrapped_input() {
        let mut app = app();
        app.composer.set("alpha beta gamma delta".into());
        let mut terminal = Terminal::new(TestBackend::new(20, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.cell((0, 7)).unwrap().bg, Color::Rgb(30, 30, 34));
        assert_eq!(buffer.cell((2, 8)).unwrap().symbol(), "a");
        assert_eq!(buffer.cell((2, 9)).unwrap().symbol(), "d");
        assert_eq!(terminal.backend().cursor_position(), Position::new(7, 9));
    }

    #[test]
    fn tall_composer_scrolls_internally_to_keep_cursor_visible() {
        let mut app = app();
        app.composer.set("one\ntwo\nthree\nfour\nfive\nsix".into());
        let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.cell((2, 4)).unwrap().symbol(), "t");
        assert_eq!(buffer.cell((2, 5)).unwrap().symbol(), "f");
        assert_eq!(buffer.cell((2, 6)).unwrap().symbol(), "f");
        assert_eq!(buffer.cell((2, 7)).unwrap().symbol(), "s");
        assert_eq!(terminal.backend().cursor_position(), Position::new(5, 7));
    }

    #[test]
    fn composer_scrolls_below_the_viewport_with_the_transcript() {
        let mut app = app();
        app.transcript.push(("phi".into(), "word ".repeat(200)));
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(
            terminal.backend().buffer().cell((0, 6)).unwrap().bg,
            Color::Rgb(30, 30, 34)
        );

        app.scroll_up(3);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        assert!((0..9).all(|y| buffer.cell((0, y)).unwrap().bg != Color::Rgb(30, 30, 34)));
        assert!(!app.follow);
    }

    #[test]
    fn status_bar_scrolls_with_the_document() {
        let mut app = app();
        app.push_transcript("phi", "word ".repeat(200));
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(terminal.backend().to_string().contains("test/model"));

        app.scroll_up(3);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(!terminal.backend().to_string().contains("test/model"));
        assert!(!app.follow);

        app.scroll_down(usize::MAX);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(terminal.backend().to_string().contains("test/model"));
        assert!(app.follow);
    }

    #[test]
    fn shows_context_without_cache_or_output_counts() {
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
        assert!(content.contains("1.5K/6K tokens"));
        assert!(!content.contains("context"));
        assert!(!content.contains("cache"));
        assert!(!content.contains("output"));
    }

    #[test]
    fn shows_model_directed_compaction_and_keeps_a_completion_marker() {
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
            activity: "selective_compacting".into(),
        });
        assert_eq!(app.status, "compacting");
        assert_eq!(
            app.transcript.last().unwrap(),
            &("phi".into(), "answer".into())
        );
        assert!(app.current_model.is_empty());
        assert!(
            transcript_text(&mut app, 80)
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
        let text = transcript_text(&mut app, 20);
        assert_eq!(text.lines.len(), 5);
        assert_eq!(text.lines[1].to_string().trim(), "• Question?");
        assert_eq!(text.lines[2].to_string().trim(), "");
        assert_eq!(text.lines[3].to_string().trim(), "Answer.");
    }

    #[test]
    fn live_model_only_rerenders_when_content_or_width_changes() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ModelDelta {
            content: "partial".into(),
        });
        transcript_text(&mut app, 80);
        let render_count = app.current_model_render_count;

        transcript_text(&mut app, 80);
        assert_eq!(app.current_model_render_count, render_count);

        app.on_runtime(RuntimeEvent::ModelDelta {
            content: " response".into(),
        });
        transcript_text(&mut app, 80);
        assert_eq!(app.current_model_render_count, render_count + 1);

        transcript_text(&mut app, 40);
        assert_eq!(app.current_model_render_count, render_count + 2);
    }

    #[test]
    fn user_background_spans_the_full_block() {
        let mut app = app();
        app.transcript.push(("you".into(), "hello".into()));
        let text = transcript_text(&mut app, 20);
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
        assert_eq!(code.style.bg, None);
        assert!(code.spans.iter().all(|span| span.style.bg.is_none()));
        assert!(code.spans.iter().any(|span| {
            !span.content.trim().is_empty() && span.style.fg != Some(Color::Rgb(190, 190, 185))
        }));
        let code_index = lines
            .iter()
            .position(|line| line.to_string().contains("fn main"))
            .unwrap();
        assert_eq!(lines[code_index - 1].style.bg, None);
        assert_eq!(lines[code_index + 1].style.bg, None);
    }

    #[test]
    fn renders_untagged_code_with_normal_text_color_and_background() {
        let mut lines = Vec::new();
        push_message(&mut lines, "phi", "```\nlet value = mystery();\n```", 80);

        let code = lines
            .iter()
            .find(|line| line.to_string().contains("let value"))
            .unwrap();
        assert_eq!(code.style.bg, None);
        assert!(code.spans.iter().all(|span| span.style.bg.is_none()));
        let content = code
            .spans
            .iter()
            .find(|span| span.content.contains("let value"))
            .unwrap();
        assert_eq!(content.style.fg, Some(Color::Rgb(190, 190, 185)));
    }

    #[test]
    fn renders_process_list_as_highlighted_shell_commands() {
        let mut lines = Vec::new();
        push_message(
            &mut lines,
            "processes",
            "## Background processes\n\n• **Running for 2m 13s**\n\n```bash\nwhile true; do echo hi; done\n```",
            80,
        );

        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("Background processes"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("Running for 2m 13s"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("• Running for 2m 13s"))
        );
        let command = lines
            .iter()
            .find(|line| line.to_string().contains("while true"))
            .unwrap();
        assert_eq!(command.style.bg, None);
        assert_eq!(command.spans.first().unwrap().style.bg, None);
        assert_eq!(command.spans.last().unwrap().style.bg, None);
        assert_eq!(command.spans.first().unwrap().content, "  ");
        assert_eq!(command.spans.last().unwrap().content, "  ");
        assert_eq!(command.spans[1].content, "  ");
        assert_eq!(command.spans[1].style.bg, None);
        assert!(command.to_string().starts_with("    while true"));
        assert!(command.spans.iter().all(|span| span.style.bg.is_none()));
        assert!(command.spans.iter().any(|span| span.style.fg.is_some()));
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
    async fn enter_queues_input_for_the_current_turn() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = app();
                app.handle = Some(phi_runtime::start(app.options.clone(), "work".into()));
                for character in "next".chars() {
                    app.on_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
                }
                app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                assert!(app.composer.text.is_empty());
                assert_eq!(app.steering_queue, VecDeque::from(["next".into()]));
                assert!(app.handle.is_some());
                app.handle.as_ref().unwrap().cancel();
            })
            .await;
    }

    #[tokio::test]
    async fn tab_queues_input_for_a_later_turn() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = app();
                app.handle = Some(phi_runtime::start(app.options.clone(), "work".into()));
                app.composer.set("later".into());

                app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

                assert!(app.composer.text.is_empty());
                assert_eq!(app.next_turn_queue, VecDeque::from(["later".into()]));
                app.handle.as_ref().unwrap().cancel();
            })
            .await;
    }

    #[test]
    fn renders_both_message_queues_as_single_line_entries() {
        let mut app = app();
        app.steering_queue
            .push_back("change direction and inspect the other implementation".into());
        app.next_turn_queue
            .push_back("write a follow-up summary\nwith details".into());

        let rendered = transcript_text(&mut app, 32).to_string();

        assert!(rendered.contains("• Queued after tool call:"));
        assert!(rendered.contains("• Queued for next turn:"));
        assert!(rendered.contains("  └ change direction"));
        assert!(rendered.contains("  └ write a follow-up"));
        assert!(rendered.matches('…').count() >= 2);
        assert!(!rendered.contains("with details\n"));
    }

    #[tokio::test]
    async fn escape_prioritizes_steering_then_future_queue_then_cancellation() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = app();
                app.handle = Some(phi_runtime::start(app.options.clone(), "work".into()));
                app.steering_queue.push_back("steer".into());
                app.next_turn_queue.push_back("later".into());

                app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                assert!(app.steering_queue.is_empty());
                assert_eq!(app.restart_after_cancel, Some(vec!["steer".into()]));
                assert_eq!(app.next_turn_queue, VecDeque::from(["later".into()]));

                app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                assert!(app.next_turn_queue.is_empty());
                assert_eq!(app.restart_after_cancel, Some(vec!["steer".into()]));

                app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                assert_eq!(app.restart_after_cancel, None);
            })
            .await;
    }

    #[test]
    fn injected_messages_leave_the_steering_queue_and_enter_history() {
        let mut app = app();
        app.steering_queue.extend(["one".into(), "two".into()]);

        app.on_runtime(RuntimeEvent::QueuedMessagesInjected {
            contents: vec!["one".into(), "two".into()],
        });

        assert!(app.steering_queue.is_empty());
        assert_eq!(
            app.transcript,
            vec![("you".into(), "one".into()), ("you".into(), "two".into())]
        );
        assert_eq!(app.message_history, vec!["one", "two"]);
    }

    #[tokio::test]
    async fn a_finished_turn_prefers_undelivered_steering_messages() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = app();
                app.handle = Some(phi_runtime::start(app.options.clone(), "work".into()));
                app.turn_started = Some(Instant::now());
                app.steering_queue.extend(["one".into(), "two".into()]);
                app.next_turn_queue.push_back("later".into());

                app.on_runtime(RuntimeEvent::Finished {
                    content: "done".into(),
                });

                assert!(app.steering_queue.is_empty());
                assert_eq!(app.next_turn_queue, VecDeque::from(["later".into()]));
                assert!(app.handle.is_some());
                assert_eq!(
                    app.transcript
                        .iter()
                        .filter(|(role, _)| role == "you")
                        .map(|(_, content)| content.as_str())
                        .collect::<Vec<_>>(),
                    vec!["one", "two"]
                );
                app.handle.as_ref().unwrap().cancel();
            })
            .await;
    }

    #[tokio::test]
    async fn a_cancelled_turn_starts_the_next_queued_turn() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = app();
                app.handle = Some(phi_runtime::start(app.options.clone(), "work".into()));
                app.next_turn_queue
                    .extend(["first".into(), "second".into()]);

                app.on_runtime(RuntimeEvent::Error {
                    message: "cancelled".into(),
                });

                assert_eq!(app.next_turn_queue, VecDeque::from(["second".into()]));
                assert!(app.handle.is_some());
                assert!(
                    app.transcript
                        .iter()
                        .any(|entry| entry == &("you".into(), "first".into()))
                );
                app.handle.as_ref().unwrap().cancel();
            })
            .await;
    }

    #[test]
    fn scrolling_reuses_the_rendered_transcript() {
        let mut app = app();
        app.push_transcript("phi", "**history**\n".repeat(10_000));
        transcript_text(&mut app, 80);
        let rendered = app.transcript_cache[0].as_ref().unwrap().lines.as_ptr();
        let render_count = app.transcript_render_count;

        app.scroll_up(3);
        transcript_text(&mut app, 80);
        assert_eq!(
            app.transcript_cache[0].as_ref().unwrap().lines.as_ptr(),
            rendered
        );
        assert_eq!(app.transcript_render_count, render_count);

        app.push_transcript("phi", "new message");
        transcript_text(&mut app, 80);
        assert_eq!(
            app.transcript_cache[0].as_ref().unwrap().lines.as_ptr(),
            rendered
        );
        assert_eq!(app.transcript_render_count, render_count + 1);
    }

    #[test]
    fn large_viewport_scroll_does_not_rerender_cached_blocks() {
        let mut app = app();
        for index in 0..500 {
            app.push_transcript("phi", format!("**message {index}**\nbody"));
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let render_count = app.transcript_render_count;
        assert_eq!(render_count, 500);

        app.scroll_up(10);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.transcript_render_count, render_count);

        app.push_transcript("phi", "new message");
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.transcript_render_count, render_count + 1);
        assert_eq!(app.transcript_offsets.len(), app.transcript.len());
    }

    #[test]
    fn updating_one_block_only_rerenders_that_block() {
        let mut app = app();
        app.push_transcript("phi", "first");
        app.push_transcript("tool", "Running");
        transcript_text(&mut app, 80);
        let first = app.transcript_cache[0].as_ref().unwrap().lines.as_ptr();
        let render_count = app.transcript_render_count;

        app.transcript[1].1 = "Finished\n\noutput".into();
        app.transcript_changed(1);
        transcript_text(&mut app, 80);

        assert_eq!(
            app.transcript_cache[0].as_ref().unwrap().lines.as_ptr(),
            first
        );
        assert_eq!(app.transcript_render_count, render_count + 1);

        transcript_text(&mut app, 40);
        assert_eq!(app.transcript_render_count, render_count + 3);
    }

    #[test]
    fn scroll_stops_at_end_of_history() {
        let mut app = app();
        app.transcript.push(("phi".into(), "hello".into()));
        app.follow = false;
        app.scroll = usize::MAX;
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
        app.scroll = usize::MAX;
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
            "cmd": "rg \"hello world\" src"
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
    fn leaves_one_blank_line_between_tool_calls() {
        let mut lines = Vec::new();
        push_tool(&mut lines, "Ran `one`", 40);
        push_tool(&mut lines, "Ran `two`", 40);

        assert_eq!(lines.len(), 4);
        assert!(lines[0].to_string().trim().is_empty());
        assert!(lines[1].to_string().contains("Ran `one`"));
        assert!(lines[2].to_string().trim().is_empty());
        assert!(lines[3].to_string().contains("Ran `two`"));
    }

    #[test]
    fn streams_shell_output_without_repeating_it_on_completion() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "shell".into(),
            name: "exec_command".into(),
            arguments: serde_json::json!({ "cmd": "printf hello" }),
        });
        app.on_runtime(RuntimeEvent::ToolOutput {
            call_id: "shell".into(),
            name: "exec_command".into(),
            content: "hello".into(),
        });
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "shell".into(),
            name: "exec_command".into(),
            result: serde_json::json!({
                "exit_code": 0,
                "stdout": "hello",
                "stderr": "",
                "session_id": null
            }),
        });

        assert_eq!(app.transcript[0].1, "Ran `printf hello`\n\nhello");
    }

    #[test]
    fn compacts_shell_output_to_its_first_and_last_two_lines() {
        assert_eq!(
            compact_shell_output("one\ntwo\nthree\nfour"),
            "one\ntwo\nthree\nfour"
        );
        assert_eq!(
            compact_shell_output("one\ntwo\nthree\nfour\nfive\n"),
            "one\ntwo\n… (+ 1 line)\nfour\nfive"
        );

        let mut rendered = Vec::new();
        push_tool(
            &mut rendered,
            "Ran `many-lines`\n\none\ntwo\n… (+ 2 lines)\nfive\nsix",
            40,
        );
        assert_eq!(rendered[5].to_string(), "    … (+ 2 lines)");
        assert_eq!(rendered[5].style.fg, Some(Color::Rgb(125, 125, 122)));

        assert_eq!(truncate_middle_width("abcdefghijk", 5), "ab…jk");
        let mut single_long_line = Vec::new();
        push_tool(
            &mut single_long_line,
            &format!("Ran `json`\n\n{}", "x".repeat(200)),
            40,
        );
        assert_eq!(single_long_line.len(), 4);
        assert_eq!(
            UnicodeWidthStr::width(single_long_line[3].to_string().as_str()),
            40
        );
        assert!(single_long_line[3].to_string().contains('…'));
    }

    #[test]
    fn normalizes_process_errors() {
        let result = serde_json::json!({
            "error": "command failed",
            "stdout": "ignored output",
            "stderr": "ignored error"
        });

        assert_eq!(raw_process_result(&result), "command failed");
        assert_eq!(tool_result(&result), "command failed");
        assert_eq!(shell_result(&result), "command failed");
    }

    #[test]
    fn joins_process_stdout_and_stderr() {
        let result = serde_json::json!({
            "exit_code": 1,
            "stdout": "standard output\n",
            "stderr": "standard error\n"
        });

        assert_eq!(
            raw_process_result(&result),
            "standard output\nstandard error"
        );
        assert_eq!(tool_result(&result), "standard output\nstandard error");
        assert_eq!(shell_result(&result), "standard output\nstandard error");
    }

    #[test]
    fn leaves_empty_process_output_empty() {
        let result = serde_json::json!({
            "stdout": " \n",
            "stderr": ""
        });

        assert_eq!(raw_process_result(&result), "");
        assert_eq!(tool_result(&result), "");
        assert_eq!(shell_result(&result), "");
    }

    #[test]
    fn falls_back_to_nonzero_process_exit_code() {
        let result = serde_json::json!({
            "exit_code": 7,
            "stdout": "",
            "stderr": "\n"
        });

        assert_eq!(raw_process_result(&result), "Exited with code 7");
        assert_eq!(tool_result(&result), "Exited with code 7");
        assert_eq!(shell_result(&result), "Exited with code 7");
    }

    #[test]
    fn compacts_streaming_exec_and_process_poll_output_incrementally() {
        for (name, arguments, heading) in [
            (
                "exec_command",
                serde_json::json!({ "cmd": "many-lines" }),
                "Ran `many-lines`",
            ),
            (
                "write_stdin",
                serde_json::json!({ "session_id": 7, "chars": "" }),
                "Checked background process",
            ),
        ] {
            let mut app = app();
            app.on_runtime(RuntimeEvent::ToolStarted {
                call_id: name.into(),
                name: name.into(),
                arguments,
            });
            app.on_runtime(RuntimeEvent::ToolOutput {
                call_id: name.into(),
                name: name.into(),
                content: "one\ntwo\nthree\n".into(),
            });
            app.on_runtime(RuntimeEvent::ToolOutput {
                call_id: name.into(),
                name: name.into(),
                content: "four\nfive\nsix".into(),
            });
            assert_eq!(
                app.transcript[0].1,
                format!("{heading}\n\none\ntwo\n… (+ 2 lines)\nfive\nsix")
            );

            app.on_runtime(RuntimeEvent::ToolOutput {
                call_id: name.into(),
                name: name.into(),
                content: " continued\nseven".into(),
            });
            assert_eq!(
                app.transcript[0].1,
                format!("{heading}\n\none\ntwo\n… (+ 3 lines)\nsix continued\nseven")
            );
        }
    }

    #[test]
    fn compacts_shell_output_that_arrives_only_on_completion() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "shell".into(),
            name: "exec_command".into(),
            arguments: serde_json::json!({ "cmd": "many-lines" }),
        });
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "shell".into(),
            name: "exec_command".into(),
            result: serde_json::json!({
                "exit_code": 0,
                "stdout": "one\ntwo\nthree\nfour\nfive\nsix\n",
                "stderr": "",
                "session_id": null
            }),
        });
        assert_eq!(
            app.transcript[0].1,
            "Ran `many-lines`\n\none\ntwo\n… (+ 2 lines)\nfive\nsix"
        );
    }

    #[test]
    fn routes_concurrent_shell_output_to_its_own_block() {
        let mut app = app();
        for (call_id, cmd) in [("first", "printf first"), ("second", "printf second")] {
            app.on_runtime(RuntimeEvent::ToolStarted {
                call_id: call_id.into(),
                name: "exec_command".into(),
                arguments: serde_json::json!({ "cmd": cmd }),
            });
        }
        app.on_runtime(RuntimeEvent::ToolOutput {
            call_id: "first".into(),
            name: "exec_command".into(),
            content: "first".into(),
        });
        app.on_runtime(RuntimeEvent::ToolOutput {
            call_id: "second".into(),
            name: "exec_command".into(),
            content: "second".into(),
        });

        assert!(app.transcript[0].1.ends_with("\n\nfirst"));
        assert!(app.transcript[1].1.ends_with("\n\nsecond"));
    }

    #[test]
    fn displays_model_process_listing() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "processes".into(),
            name: "list_processes".into(),
            arguments: serde_json::json!({}),
        });
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "processes".into(),
            name: "list_processes".into(),
            result: serde_json::json!({
                "processes": [{
                    "session_id": 4,
                    "command": "cargo test",
                    "status": "running",
                    "elapsed_ms": 2_000
                }]
            }),
        });

        assert_eq!(
            app.transcript[0].1,
            "Checked background processes · 0s\n\nRunning for 2s · cargo test"
        );
    }

    #[test]
    fn process_poll_hides_internal_session_id() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "poll".into(),
            name: "write_stdin".into(),
            arguments: serde_json::json!({ "session_id": 7, "chars": "" }),
        });
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "poll".into(),
            name: "write_stdin".into(),
            result: serde_json::json!({
                "session_id": 7,
                "exit_code": null,
                "stdout": "",
                "stderr": ""
            }),
        });

        assert_eq!(
            app.transcript[0].1,
            "Checked background process\n\nStill running"
        );
        assert!(!app.transcript[0].1.contains('7'));
    }

    #[test]
    fn process_termination_hides_internal_session_id() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "terminate".into(),
            name: "terminate_process".into(),
            arguments: serde_json::json!({ "session_id": 7 }),
        });
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "terminate".into(),
            name: "terminate_process".into(),
            result: serde_json::json!({
                "session_id": 7,
                "status": "terminated",
                "signal": "SIGINT",
                "exit_code": null
            }),
        });

        assert_eq!(
            app.transcript[0].1,
            "Stopped background process · 0s\n\nSIGINT"
        );
        assert!(!app.transcript[0].1.contains('7'));
    }

    #[test]
    fn renders_workflow_launch_with_its_name() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "workflow".into(),
            name: "Workflow".into(),
            arguments: serde_json::json!({ "name": "review" }),
        });
        assert_eq!(app.transcript[0].1, "Starting workflow `review`");

        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "workflow".into(),
            name: "Workflow".into(),
            result: serde_json::json!({
                "workflow": "review",
                "task_id": "task-1",
                "status": "async_launched"
            }),
        });
        assert_eq!(
            app.transcript[0].1,
            "Started workflow `review` · 0s\n\nRunning in background"
        );

        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "output".into(),
            name: "TaskOutput".into(),
            arguments: serde_json::json!({ "task_id": "task-1" }),
        });
        assert_eq!(app.transcript[1].1, "Checking workflow `review`");
    }

    #[test]
    fn renders_running_workflow_phase_and_agent_progress() {
        let rendered = workflow_tool_result(
            "TaskOutput",
            &serde_json::json!({
                "workflow": "review",
                "status": "running",
                "state": {},
                "summary": {
                    "phase": "Reviewing tests",
                    "latestLog": "Reviewed 4 of 10 files",
                    "agents": {
                        "started": 6,
                        "running": 2,
                        "completed": 4,
                        "failed": 0
                    }
                }
            }),
            Duration::from_secs(15),
        );
        assert_eq!(
            rendered,
            "Workflow `review` still running · 15s\n\nPhase: Reviewing tests\nAgents: 2 running · 4 completed\nLatest: Reviewed 4 of 10 files"
        );
    }

    #[test]
    fn renders_completed_workflow_total_duration() {
        let rendered = workflow_tool_result(
            "TaskOutput",
            &serde_json::json!({
                "workflow": "review",
                "status": "completed",
                "state": {
                    "startedAt": 1_000,
                    "completedAt": 63_000
                },
                "summary": {
                    "phase": "Final review",
                    "agents": { "completed": 3 }
                }
            }),
            Duration::ZERO,
        );
        assert_eq!(
            rendered,
            "Workflow `review` completed · 1m 2s\n\nPhase: Final review\nAgents: 3 completed"
        );
    }

    #[test]
    fn chat_history_scrolls_independently_of_input_history() {
        let mut app = app();
        app.scroll = 30;
        app.follow = true;

        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(app.scroll, 27);
        assert!(!app.follow);

        app.on_mouse(MouseEventKind::ScrollDown);
        assert_eq!(app.scroll, 30);
        assert!(!app.follow);
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

        assert_eq!(turn_divider("", 40), "─".repeat(40));
    }

    #[test]
    fn separates_a_response_that_starts_after_tool_calls() {
        let mut streaming_app = app();
        streaming_app
            .transcript
            .push(("tool".into(), "Ran `test`".into()));

        streaming_app.on_runtime(RuntimeEvent::ModelDelta {
            content: "Final answer".into(),
        });

        assert_eq!(streaming_app.transcript.last().unwrap().0, "response_start");
        let text = transcript_text(&mut streaming_app, 40);
        let rendered = text
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let divider = rendered
            .iter()
            .position(|line| line == &"─".repeat(40))
            .unwrap();
        assert!(!rendered[divider - 2].trim().is_empty());
        assert!(rendered[divider - 1].trim().is_empty());
        assert!(rendered[divider + 1].trim().is_empty());
        assert!(!rendered[divider + 2].trim().is_empty());

        let mut fallback_app = app();
        fallback_app
            .transcript
            .push(("patch".into(), "Updated file".into()));
        fallback_app.on_runtime(RuntimeEvent::Finished {
            content: "Fallback final answer".into(),
        });
        assert_eq!(fallback_app.transcript[1].0, "response_start");
        assert_eq!(fallback_app.transcript[2].0, "phi");
    }

    #[test]
    fn response_divider_has_no_leading_gap_without_preceding_transcript() {
        let mut app = app();
        app.transcript
            .push(("response_start".into(), String::new()));
        app.transcript.push(("phi".into(), "Final answer".into()));

        let text = transcript_text(&mut app, 40);
        assert_eq!(text.lines[0].to_string(), "─".repeat(40));
        assert!(text.lines[1].to_string().trim().is_empty());
        assert!(!text.lines[2].to_string().trim().is_empty());
    }

    #[test]
    fn keeps_ordered_list_markers_with_their_first_line() {
        let mut lines = Vec::new();
        push_message(
            &mut lines,
            "phi",
            "Intro\n\n1. First numbered item\n2. Second numbered item",
            12,
        );

        assert!(
            lines
                .iter()
                .any(|line| line.to_string().starts_with("• Intro"))
        );

        assert!(
            lines
                .iter()
                .any(|line| line.to_string().starts_with("1. First"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("2. Second"))
        );
        assert!(!lines.iter().any(|line| line.to_string().trim() == "1."));
        assert!(!lines.iter().any(|line| line.to_string().trim() == "2."));
    }

    #[test]
    fn keeps_spaced_ordered_list_markers_with_their_items() {
        let mut lines = Vec::new();
        push_message(
            &mut lines,
            "phi",
            "1. one\n\n2. two\n\n3. three\n4. four",
            40,
        );

        let rendered = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(
            rendered.iter().any(|line| line.starts_with("1. one")),
            "rendered lines: {rendered:#?}"
        );
        assert!(rendered.iter().any(|line| line.starts_with("2. two")));
        assert!(rendered.iter().any(|line| line.starts_with("3. three")));
        assert!(rendered.iter().any(|line| line.starts_with("4. four")));
        assert!(
            !rendered
                .iter()
                .any(|line| matches!(line.trim(), "1." | "2." | "3." | "4.")),
            "rendered lines: {rendered:#?}"
        );
    }

    #[test]
    fn shows_live_working_throbber_at_end_of_history() {
        let mut app = app();
        app.current_model = "partial response".into();
        app.turn_started = Some(Instant::now() - Duration::from_secs(133));
        let text = transcript_text(&mut app, 40);
        let activity = text.lines[text.lines.len() - 2].to_string();
        assert!(activity.starts_with("  ⠷ Working for 2m "));
        assert!(!activity.contains('─'));
        assert_eq!(UnicodeWidthStr::width(activity.as_str()), 40);
        assert!(text.lines.last().unwrap().to_string().trim().is_empty());
    }

    #[test]
    fn working_throbber_has_a_gap_after_user_block() {
        let mut app = app();
        app.transcript.push(("you".into(), "hello".into()));
        app.turn_started = Some(Instant::now());
        let text = transcript_text(&mut app, 40);
        let gap = &text.lines[text.lines.len() - 3];
        assert!(gap.to_string().trim().is_empty());
        assert_eq!(gap.style.bg, None);
        assert!(
            text.lines[text.lines.len() - 2]
                .to_string()
                .starts_with("  ⠷ Working for ")
        );
    }

    #[test]
    fn working_throbber_has_one_gap_after_tool_block() {
        let mut app = app();
        app.transcript.push(("tool".into(), "Ran `test`".into()));
        app.turn_started = Some(Instant::now());

        let text = transcript_text(&mut app, 40);
        let tail = &text.lines[text.lines.len() - 4..];
        assert!(!tail[0].to_string().trim().is_empty());
        assert!(tail[1].to_string().trim().is_empty());
        assert!(tail[2].to_string().starts_with("  ⠷ Working for "));
        assert!(tail[3].to_string().trim().is_empty());
    }

    #[test]
    fn shows_live_compaction_throbber() {
        let mut app = app();
        app.turn_started = Some(Instant::now());
        app.compaction_started = Some(Instant::now() - Duration::from_secs(3));
        app.status = "compacting".into();

        let text = transcript_text(&mut app, 32);
        let activity = text.lines[text.lines.len() - 2].to_string();
        assert!(activity.starts_with("  ⠷ Compacting for 3s"));
        assert!(!activity.contains('─'));
        assert_eq!(UnicodeWidthStr::width(activity.as_str()), 32);
        assert_eq!(text.lines.len(), 2);
        assert!(text.lines.last().unwrap().to_string().trim().is_empty());
    }

    #[test]
    fn compacting_throbber_has_one_gap_after_tool_block() {
        let mut app = app();
        app.transcript.push(("tool".into(), "Ran `test`".into()));
        app.turn_started = Some(Instant::now());
        app.compaction_started = Some(Instant::now());
        app.status = "compacting".into();

        let text = transcript_text(&mut app, 40);
        let tail = &text.lines[text.lines.len() - 4..];
        assert!(!tail[0].to_string().trim().is_empty());
        assert!(tail[1].to_string().trim().is_empty());
        assert!(tail[2].to_string().starts_with("  ⠷ Compacting for "));
        assert!(tail[3].to_string().trim().is_empty());
    }

    #[test]
    fn activity_tick_advances_braille_six_without_rerendering_history() {
        let mut app = app();
        app.push_transcript("phi", "cached history");
        app.turn_started = Some(Instant::now());
        let before = transcript_text(&mut app, 40);
        let before_activity = before.lines[before.lines.len() - 2].to_string();
        let render_count = app.transcript_render_count;

        app.on_tick();
        let after = transcript_text(&mut app, 40);
        let after_activity = after.lines[after.lines.len() - 2].to_string();

        assert!(before_activity.starts_with(&format!("  {}", BRAILLE_SIX.symbols[0])));
        assert!(after_activity.starts_with(&format!("  {}", BRAILLE_SIX.symbols[1])));
        assert_eq!(app.transcript_render_count, render_count);
    }

    #[test]
    fn activity_indicator_pads_and_truncates_to_narrow_display_widths() {
        let state = ThrobberState::default();
        for width in 0..=24 {
            let line = activity_indicator(&state, "Working for 12s", width).to_string();
            assert_eq!(
                UnicodeWidthStr::width(line.as_str()),
                width,
                "line at width {width}: {line:?}"
            );
            assert!(!line.contains('─'));
            match width {
                0 => assert!(line.is_empty()),
                1 => assert_eq!(line, " "),
                2 => assert_eq!(line, "  "),
                _ => assert!(
                    line.starts_with(&format!("  {}", BRAILLE_SIX.symbols[0])),
                    "line at width {width}: {line:?}"
                ),
            }
        }
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

    #[tokio::test]
    async fn compact_command_starts_runtime_compaction() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = app();
                app.composer.text = "/compact".into();
                app.submit();
                assert!(app.handle.is_some());
                assert!(app.command_task.is_none());
                assert_eq!(app.status, "compacting");
                assert!(!app.transcript.iter().any(|(role, _)| role == "you"));
            })
            .await;
    }

    #[test]
    fn compact_command_rejects_arguments_locally() {
        let mut app = app();
        app.composer.text = "/compact now".into();
        app.submit();
        assert!(app.handle.is_none());
        assert_eq!(
            app.transcript.last().unwrap(),
            &("error".into(), "usage: /compact".into())
        );
    }

    #[test]
    fn new_session_command_clears_chat_state() {
        let mut app = app();
        app.transcript.push(("you".into(), "old message".into()));
        app.current_model = "partial response".into();
        app.estimated_tokens = Some(700);
        app.input_tokens = Some(600);
        app.cached_tokens = Some(500);
        app.cache_write_tokens = Some(400);
        app.output_tokens = Some(300);
        app.compactions = 2;
        app.message_history.push("old message".into());
        app.history_index = Some(0);
        app.history_draft = "draft".into();
        app.scroll = 12;
        app.follow = false;
        let catalog = app.catalog.clone();

        app.on_command(Ok(CommandExecution {
            session_id: "11111111-1111-4111-8111-111111111111".into(),
            content: "Started a new chat.".into(),
            role: "note".into(),
            catalog,
            action: CommandAction::NewSession,
        }));

        assert_eq!(
            app.session_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
        assert_eq!(
            app.transcript,
            vec![("note".into(), "Started a new chat.".into())]
        );
        assert!(app.current_model.is_empty());
        assert_eq!(app.estimated_tokens, Some(0));
        assert_eq!(app.input_tokens, None);
        assert_eq!(app.cached_tokens, None);
        assert_eq!(app.cache_write_tokens, None);
        assert_eq!(app.output_tokens, None);
        assert_eq!(app.compactions, 0);
        assert!(app.message_history.is_empty());
        assert_eq!(app.history_index, None);
        assert!(app.history_draft.is_empty());
        assert_eq!(app.scroll, 0);
        assert!(app.follow);
        assert_eq!(app.status, "ready");
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
        assert_eq!(app.composer.text, "/help");
    }

    #[test]
    fn model_picker_and_status_use_qualified_model_id() {
        let app = app();
        assert_eq!(app.estimated_tokens, Some(0));
        assert_eq!(app.token_budget, Some(1_000));
        let options = picker_options(&Picker::Model { selected: 0 }, &app.catalog);
        assert_eq!(options[0].label, "test/model");

        let mut app = app;
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let content = terminal.backend().to_string();
        assert!(content.contains("test/model low default"));
        assert!(content.contains("0/1K tokens"));
        assert!(!content.contains(" · ready · "));
    }

    #[test]
    fn formats_token_counts_with_three_significant_digits() {
        assert_eq!(human_tokens(999), "999");
        assert_eq!(human_tokens(1_000), "1K");
        assert_eq!(human_tokens(4_812), "4.81K");
        assert_eq!(human_tokens(27_200), "27.2K");
        assert_eq!(human_tokens(272_000), "272K");
        assert_eq!(human_tokens(500_000), "500K");
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
    fn shift_enter_inserts_a_newline_without_submitting() {
        let mut app = app();
        app.composer.set("first".into());
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(app.composer.text, "first\n");
        assert!(app.handle.is_none());
        assert!(app.transcript.is_empty());
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

        app.composer.set("before\nalpha beta\nafter".into());
        app.composer.cursor = 12;
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.composer.text, "before\n beta\nafter");
        assert_eq!(app.composer.cursor, 7);
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
        assert!(app.composer.on_first_visual_row(app.composer_width));
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
