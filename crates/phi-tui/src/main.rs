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

struct ApprovalPrompt {
    name: String,
    detail: String,
}

struct App {
    options: RunOptions,
    catalog: CommandCatalog,
    session_id: Option<String>,
    transcript: Vec<(String, String)>,
    current_model: String,
    current_model_revision: u64,
    current_model_cache: Option<RenderedLiveModel>,
    current_commentary: Option<usize>,
    current_reasoning_summary: Option<usize>,
    composer: Composer,
    handle: Option<Handle>,
    command_task: Option<tokio::task::JoinHandle<Result<CommandExecution>>>,
    picker: Option<Picker>,
    approval: Option<ApprovalPrompt>,
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
    kind: TranscriptBlockKind,
    previous_kind: Option<TranscriptBlockKind>,
    lines: Vec<Line<'static>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranscriptBlockKind {
    User,
    Assistant,
    Commentary,
    Reasoning,
    Tool,
    Patch,
    Note,
    Error,
    ProcessOutput,
    ResponseStart,
    TurnEnd,
    CompactionEnd,
    TurnWorking,
    Activity,
}

impl TranscriptBlockKind {
    fn from_role(role: &str) -> Self {
        match role {
            "you" => Self::User,
            "phi" => Self::Assistant,
            "commentary" => Self::Commentary,
            "reasoning_summary" => Self::Reasoning,
            "tool" => Self::Tool,
            "patch" => Self::Patch,
            "error" => Self::Error,
            "processes" => Self::ProcessOutput,
            "response_start" => Self::ResponseStart,
            "turn_end" => Self::TurnEnd,
            "compaction_end" => Self::CompactionEnd,
            "turn_working" => Self::TurnWorking,
            _ => Self::Note,
        }
    }
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
            current_commentary: None,
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
                "keys" if invocation.arguments.is_empty() => {
                    self.push_transcript("note", self.keybinding_help());
                }
                "keys" => self.push_transcript("error", "usage: /keys"),
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
        self.start_messages_with_display(messages, true);
    }

    fn start_messages_with_display(&mut self, messages: Vec<String>, display: bool) {
        if messages.is_empty() {
            return;
        }
        self.options.session_id = self.session_id.clone();
        if display {
            self.display_user_messages(&messages);
        }
        self.handle = Some(phi_runtime::start_messages(self.options.clone(), messages));
        self.turn_started = Some(Instant::now());
        self.final_response_rendered = false;
        self.status = "working".into();
        self.follow = true;
    }

    fn display_user_messages(&mut self, messages: &[String]) {
        for message in messages {
            self.push_transcript("you", message.clone());
            self.message_history.push(message.clone());
        }
        self.history_index = None;
        self.history_draft.clear();
        self.follow = true;
    }

    fn remove_displayed_restart_messages(&mut self, messages: &[String]) {
        for message in messages.iter().rev() {
            if let Some(index) = self
                .transcript
                .iter()
                .rposition(|(role, content)| role == "you" && content == message)
            {
                self.transcript.remove(index);
                self.transcript_cache.remove(index);
            }
            if let Some(index) = self
                .message_history
                .iter()
                .rposition(|content| content == message)
            {
                self.message_history.remove(index);
            }
        }
        self.transcript_offsets.clear();
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

    fn keybinding_help(&self) -> String {
        let detail =
            |value: Option<u64>| value.map_or_else(|| "—".into(), |value| value.to_string());
        let context = match (self.estimated_tokens, self.token_budget) {
            (Some(used), Some(budget)) => format!("{used} / {budget}"),
            _ => "—".into(),
        };
        format!(
            "## Keys\n\n\
• **Send / steer:** `Enter` sends when idle or queues guidance for the active turn.\n\
• **Queue next turn:** `Tab` during an active turn.\n\
• **New line:** `Shift+Enter` or `Ctrl+Enter`.\n\
• **Move:** arrow keys; `Alt+Left/Right` or `Alt+B/F` by word; `Home/End` or `Ctrl+A/E` by line; `Cmd+Up/Down` to the start/end.\n\
• **Delete:** `Backspace/Delete`; `Alt+Backspace` by word; `Ctrl+U` or `Cmd+Backspace` to line start.\n\
• **History:** `Up/Down` at the first/last composer row.\n\
• **Scroll:** `Shift+Up/Down`, `PageUp/PageDown`, or mouse wheel.\n\
• **Commands:** type `/`, then use `Up/Down` and `Enter`.\n\
• **Cancel:** `Ctrl+C` cancels an active turn; `Esc` removes queued input before cancelling the turn. `Ctrl+C` quits when idle.\n\
• **Picker:** `Up/Down`, `Enter` to select, `Esc` to cancel.\n\
• **Approval:** `y` allows once; `n` or `Esc` denies.\n\n\
## Token details\n\n\
• Context: {context}\n\
• Input: {}\n\
• Cached input: {}\n\
• Cache write: {}\n\
• Output: {}",
            detail(self.input_tokens),
            detail(self.cached_tokens),
            detail(self.cache_write_tokens),
            detail(self.output_tokens),
        )
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
                let stage = match picker {
                    Picker::Model { .. } => "Model",
                    Picker::Reasoning { .. } => "Reasoning",
                    Picker::ServiceTier { .. } => "Service tier",
                };
                self.push_transcript("note", format!("{stage} selection cancelled."));
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
            RuntimeEvent::History { messages } => self.restore_history(messages),
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
                    let already_displayed = self
                        .restart_after_cancel
                        .as_ref()
                        .is_some_and(|messages| messages.contains(content));
                    if !already_displayed {
                        self.push_transcript("you", content.clone());
                        self.message_history.push(content.clone());
                    }
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
                self.current_commentary = None;
                self.current_reasoning_summary = None;
                self.mark_response_start_after_activity();
                self.current_model.push_str(&content);
                self.current_model_changed();
            }
            RuntimeEvent::CommentaryDelta { content } => {
                self.flush_model();
                let index = if let Some(index) = self.current_commentary {
                    index
                } else {
                    let index = self.transcript.len();
                    self.push_transcript("commentary", "");
                    self.current_commentary = Some(index);
                    index
                };
                if let Some((_, commentary)) = self.transcript.get_mut(index) {
                    commentary.push_str(&content);
                    self.transcript_changed(index);
                }
            }
            RuntimeEvent::CommentaryStarted => {
                self.flush_model();
                self.current_commentary = None;
            }
            RuntimeEvent::ReasoningSummaryDelta { content } => {
                self.current_commentary = None;
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
                self.current_commentary = None;
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
            RuntimeEvent::ApprovalRequested { name, detail } => {
                self.approval = Some(ApprovalPrompt { name, detail });
            }
            RuntimeEvent::Finished { content } => {
                self.current_commentary = None;
                self.current_reasoning_summary = None;
                if !self.final_response_rendered
                    && self.current_model.is_empty()
                    && !content.is_empty()
                {
                    self.mark_response_start_after_activity();
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
                self.current_commentary = None;
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
                    let queued = self.steering_queue.drain(..).collect::<Vec<_>>();
                    self.display_user_messages(&queued);
                    messages.extend(queued);
                    self.start_messages_with_display(messages, false);
                } else {
                    if let Some(messages) = self.restart_after_cancel.take() {
                        self.remove_displayed_restart_messages(&messages);
                    }
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

    fn restore_history(&mut self, messages: Vec<serde_json::Value>) {
        let trailing = std::mem::take(&mut self.transcript);
        self.transcript_cache.clear();
        let mut tool_names = HashMap::new();
        for message in messages {
            match message["kind"].as_str() {
                Some("message") => {
                    let content = message["content"].as_str().unwrap_or_default();
                    if content.trim().is_empty() {
                        continue;
                    }
                    match message["role"].as_str() {
                        Some("user") => self.push_transcript("you", content),
                        Some("assistant") if message["phase"] == "commentary" => {
                            self.push_transcript("commentary", content)
                        }
                        Some("assistant") => {
                            self.mark_response_start_after_activity();
                            self.push_transcript("phi", content);
                        }
                        _ => {}
                    }
                }
                Some("reasoning_summary") => {
                    let content = message["content"].as_str().unwrap_or_default();
                    if !content.trim().is_empty() {
                        self.push_transcript("reasoning_summary", content);
                    }
                }
                Some("tool_call") => {
                    let call_id = message["call_id"].as_str().unwrap_or_default().to_owned();
                    let name = message["name"].as_str().unwrap_or_default().to_owned();
                    let arguments = message["arguments"]
                        .as_str()
                        .and_then(|value| serde_json::from_str(value).ok())
                        .unwrap_or_else(|| message["arguments"].clone());
                    tool_names.insert(call_id.clone(), name.clone());
                    self.on_runtime(RuntimeEvent::ToolStarted {
                        call_id,
                        name,
                        arguments,
                    });
                }
                Some("tool_result") => {
                    let call_id = message["call_id"].as_str().unwrap_or_default().to_owned();
                    let name = tool_names.get(&call_id).cloned().unwrap_or_default();
                    let result = message["content"]
                        .as_str()
                        .and_then(|value| serde_json::from_str(value).ok())
                        .unwrap_or_else(|| message["content"].clone());
                    self.on_runtime(RuntimeEvent::ToolCompleted {
                        call_id,
                        name,
                        result,
                    });
                }
                _ => {}
            }
        }
        for (role, content) in trailing {
            self.push_transcript(role, content);
        }
    }

    fn mark_response_start_after_activity(&mut self) {
        if !self.current_model.is_empty() {
            return;
        }
        let mut activity_rendered = false;
        for (role, content) in self.transcript.iter().rev() {
            match role.as_str() {
                "you" | "turn_end" => break,
                "response_start" => return,
                "commentary" | "reasoning_summary" if !content.trim().is_empty() => {
                    activity_rendered = true;
                }
                "tool" | "patch" => activity_rendered = true,
                _ => {}
            }
        }
        if activity_rendered {
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
            } else if self.command_task.is_some() {
                self.push_transcript(
                    "note",
                    "Slash command cancellation is unavailable; waiting for it to finish.",
                );
                self.follow = true;
            } else if self.command_task.is_none() {
                self.quit = true;
            }
            return;
        }
        match key.code {
            KeyCode::Esc if self.handle.is_some() => {
                if !self.steering_queue.is_empty() {
                    let queued = self.steering_queue.drain(..).collect::<Vec<_>>();
                    self.display_user_messages(&queued);
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
                        let messages = self.restart_after_cancel.take().unwrap_or_default();
                        self.remove_displayed_restart_messages(&messages);
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
    let mut frame_tick = tokio::time::interval(Duration::from_millis(16));
    frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    frame_tick.tick().await;
    terminal.draw(|frame| draw(frame, app))?;
    let mut redraw = false;
    while !app.quit {
        tokio::select! {
            biased;
            event = input.next().fuse() => {
                match event.transpose()? {
                    Some(Event::Key(key)) if key.is_press() => app.on_key(key),
                    Some(Event::Mouse(mouse)) => app.on_mouse(mouse.kind),
                    Some(Event::Resize(_, _)) => {},
                    Some(_) => {},
                    None => app.quit = true,
                }
                redraw = true;
            }
            _ = tick.tick(), if app.turn_started.is_some() => {
                app.on_tick();
                redraw = true;
            },
            _ = frame_tick.tick(), if redraw => {
                terminal.draw(|frame| draw(frame, app))?;
                redraw = false;
            },
            event = next_runtime(&mut app.handle).fuse() => {
                if let Some(event) = event {
                    app.on_runtime(event);
                } else {
                    app.handle = None;
                }
                redraw = true;
            }
            result = next_command(&mut app.command_task).fuse() => {
                app.command_task = None;
                if let Some(result) = result {
                    app.on_command(result);
                }
                redraw = true;
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

    if let Some(approval) = &app.approval {
        let width = frame.area().width.min(60);
        let content_width = width.saturating_sub(2).max(1) as usize;
        let mut detail = wrap_line(&approval.detail, content_width);
        let height = (detail.len() as u16)
            .saturating_add(3)
            .min(frame.area().height);
        detail.truncate(height.saturating_sub(3) as usize);
        let area = centered(frame.area(), width, height);
        frame.render_widget(Clear, area);
        let mut lines = vec![Line::raw(format!("Allow {} once?", approval.name))];
        lines.extend(detail.into_iter().map(Line::raw));
        lines.push(Line::raw(""));
        lines.push(Line::raw(if content_width >= 17 {
            "[y] yes   [n] no"
        } else if content_width >= 7 {
            "[y] [n]"
        } else {
            "y/n"
        }));
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Color::Rgb(45, 40, 25))),
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
    let mut previous_kind = None;
    for (index, (role, content)) in app.transcript.iter().enumerate() {
        let kind = TranscriptBlockKind::from_role(role);
        let stale = app.transcript_cache[index].as_ref().is_none_or(|cache| {
            cache.width != width || cache.kind != kind || cache.previous_kind != previous_kind
        });
        if stale {
            first_changed = Some(first_changed.map_or(index, |changed| changed.min(index)));
            let mut lines = Vec::new();
            push_transcript_block(&mut lines, previous_kind, kind, role, content, width);
            app.transcript_cache[index] = Some(RenderedTranscriptBlock {
                width,
                kind,
                previous_kind,
                lines,
            });
            #[cfg(test)]
            {
                app.transcript_render_count += 1;
            }
        }
        if app.transcript_cache[index]
            .as_ref()
            .is_some_and(|cache| !cache.lines.is_empty())
        {
            previous_kind = Some(kind);
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
        let previous_kind = app
            .transcript_cache
            .iter()
            .rev()
            .filter_map(Option::as_ref)
            .find(|block| !block.lines.is_empty())
            .map(|block| block.kind);
        let stale = app.current_model_cache.as_ref().is_none_or(|cache| {
            cache.revision != app.current_model_revision
                || cache.block.width != width
                || cache.block.previous_kind != previous_kind
        });
        if stale {
            let mut model_lines = Vec::new();
            push_transcript_block(
                &mut model_lines,
                previous_kind,
                TranscriptBlockKind::Assistant,
                "phi",
                &app.current_model,
                width,
            );
            app.current_model_cache = Some(RenderedLiveModel {
                revision: app.current_model_revision,
                block: RenderedTranscriptBlock {
                    width,
                    kind: TranscriptBlockKind::Assistant,
                    previous_kind,
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
    let mut previous_kind = app
        .current_model_cache
        .as_ref()
        .filter(|cache| !cache.block.lines.is_empty())
        .map(|cache| cache.block.kind)
        .or_else(|| {
            app.transcript_cache
                .iter()
                .rev()
                .filter_map(Option::as_ref)
                .find(|block| !block.lines.is_empty())
                .map(|block| block.kind)
        });
    if let Some(turn_started) = app.turn_started {
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
        let kind = if activity == "Searching" {
            TranscriptBlockKind::TurnWorking
        } else {
            TranscriptBlockKind::Activity
        };
        push_block_separator(&mut lines, previous_kind, kind, width);
        if activity == "Searching" {
            push_message(&mut lines, "turn_working", &label, width);
        } else {
            lines.push(activity_indicator(&app.throbber_state, &label, width));
        }
        previous_kind = Some(kind);
    }
    if push_message_queue(
        &mut lines,
        previous_kind,
        "Queued after tool call:",
        &app.steering_queue,
        width,
        Color::LightYellow,
    ) {
        previous_kind = Some(TranscriptBlockKind::Note);
    }
    if push_message_queue(
        &mut lines,
        previous_kind,
        "Queued for next turn:",
        &app.next_turn_queue,
        width,
        Color::LightBlue,
    ) {
        previous_kind = Some(TranscriptBlockKind::Note);
    }
    if previous_kind.is_some() {
        lines.push(Line::raw(" ".repeat(width)));
    }
    lines
}

fn push_message_queue(
    lines: &mut Vec<Line<'static>>,
    previous_kind: Option<TranscriptBlockKind>,
    label: &str,
    messages: &VecDeque<String>,
    width: usize,
    color: Color,
) -> bool {
    if messages.is_empty() || width == 0 {
        return false;
    }
    push_block_separator(lines, previous_kind, TranscriptBlockKind::Note, width);
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
    true
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

/// Transcript blocks always have exactly one blank line between them and no
/// leading separator at the start of the document. Blank lines emitted while
/// rendering a block are therefore meaningful internal content layout.
fn transcript_separator_lines(
    previous: Option<TranscriptBlockKind>,
    _next: TranscriptBlockKind,
) -> usize {
    usize::from(previous.is_some())
}

fn push_block_separator(
    lines: &mut Vec<Line<'static>>,
    previous: Option<TranscriptBlockKind>,
    next: TranscriptBlockKind,
    width: usize,
) {
    lines.extend(
        std::iter::repeat_with(|| Line::raw(" ".repeat(width)))
            .take(transcript_separator_lines(previous, next)),
    );
}

fn push_transcript_block(
    lines: &mut Vec<Line<'static>>,
    previous: Option<TranscriptBlockKind>,
    kind: TranscriptBlockKind,
    role: &str,
    content: &str,
    width: usize,
) {
    let mut content_lines = Vec::new();
    push_message(&mut content_lines, role, content, width);
    if content_lines.is_empty() {
        return;
    }
    push_block_separator(lines, previous, kind, width);
    lines.extend(content_lines);
}

fn push_message(lines: &mut Vec<Line<'static>>, role: &str, content: &str, width: usize) {
    if role == "turn_end" || role == "turn_working" || role == "compaction_end" {
        lines.push(Line::raw(turn_divider(content, width)));
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
    if role == "commentary" {
        push_commentary(lines, content, width);
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
}

fn push_reasoning_summary(lines: &mut Vec<Line<'static>>, content: &str, width: usize) {
    if content.trim().is_empty() {
        return;
    }
    let content_style = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::ITALIC);
    push_markdown_content(
        lines,
        content,
        width,
        MarkdownBlockStyle {
            normal: Color::Gray,
            strong: Color::Gray,
            block: Style::default(),
            base: content_style,
            marker: "  ",
            code_inset: 0,
            vertical_inset: 0,
        },
    );
}

fn push_commentary(lines: &mut Vec<Line<'static>>, content: &str, width: usize) {
    if content.trim().is_empty() {
        return;
    }
    push_markdown_content(
        lines,
        content,
        width,
        MarkdownBlockStyle {
            normal: Color::DarkGray,
            strong: Color::Gray,
            block: Style::default(),
            base: Style::default().fg(Color::DarkGray),
            marker: "  ",
            code_inset: 0,
            vertical_inset: 0,
        },
    );
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
    let block_style = if role == "you" {
        Style::default().bg(Color::Rgb(38, 40, 45))
    } else {
        Style::default()
    };
    let code_inset = usize::from(role == "processes") * 2;
    push_markdown_content(
        lines,
        content,
        width,
        MarkdownBlockStyle {
            normal: Color::Rgb(190, 190, 185),
            strong: Color::Rgb(235, 235, 230),
            block: block_style,
            base: Style::default(),
            marker: if role == "you" { "‣ " } else { "• " },
            code_inset,
            vertical_inset: usize::from(role == "you"),
        },
    );
}

#[derive(Clone, Copy)]
struct MarkdownBlockStyle {
    normal: Color,
    strong: Color,
    block: Style,
    base: Style,
    marker: &'static str,
    code_inset: usize,
    vertical_inset: usize,
}

fn push_markdown_content(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    width: usize,
    block: MarkdownBlockStyle,
) {
    let content_start = lines.len();
    let code_padding = block.code_inset;
    let options = tui_markdown::Options::new(PhiMarkdown);
    let markdown = tui_markdown::from_str_with_options(content, &options);
    let content_width = width.saturating_sub(2).max(1);
    let mut marked = false;
    let mut in_code = false;
    let mut list_markers = 0;
    let mut list_item_indents = Vec::new();
    let mut continuation_hints = list_continuation_hints(content).into_iter().peekable();
    let mut previous_was_blank = false;
    let mut markdown_lines = markdown.lines.into_iter().peekable();
    while let Some(mut line) = markdown_lines.next() {
        let marker_text = line.to_string();
        if is_list_marker_only(&marker_text)
            && let Some(next) = markdown_lines.peek()
            && !next.to_string().trim().is_empty()
        {
            let item = markdown_lines.next().expect("peeked list item");
            if !marker_text.chars().last().is_some_and(char::is_whitespace) {
                line.spans.push(Span::raw(" "));
            }
            line.spans.extend(item.spans);
        }
        let plain = line.to_string();
        if plain.trim_start().starts_with("```") {
            if in_code {
                in_code = false;
            } else {
                // tui-markdown inserts one blank line before every non-leading
                // fence. Replace that renderer-owned padding with our single
                // intentional prose/code separator, while leaving any blank
                // lines inside the preceding code block untouched.
                if lines
                    .last()
                    .is_some_and(|line| line.to_string().trim().is_empty())
                {
                    lines.pop();
                }
                if !lines.is_empty() {
                    lines.push(Line::styled(
                        " ".repeat(width),
                        block.block.patch(block.base),
                    ));
                }
                in_code = true;
            }
            continue;
        }
        let list_marker = (!in_code).then(|| list_marker(&plain)).flatten();
        let continuation_depth = if !in_code && previous_was_blank {
            continuation_hints
                .next_if(|hint| hint.after_marker == list_markers)
                .map(|hint| hint.depth)
        } else {
            None
        };
        let hanging_list = list_marker.filter(|marker| width > 2 + marker.prefix_width);
        let line_width = if let Some(marker) = hanging_list {
            width.saturating_sub(2 + marker.prefix_width)
        } else if list_marker.is_some() {
            width.saturating_sub(2).max(1)
        } else if let Some(depth) = continuation_depth {
            width
                .saturating_sub(list_item_indents.get(depth).copied().unwrap_or(2))
                .max(1)
        } else if in_code && block.code_inset > 0 {
            width
                .saturating_sub(block.code_inset * 2 + code_padding * 2)
                .max(1)
        } else {
            content_width
        };
        let (line_prefix, line) = if let Some(marker) = hanging_list {
            split_styled_line(&line, marker.prefix_width)
        } else {
            (Line::default(), line)
        };
        for (wrap_index, wrapped) in wrap_styled_line(&line, line_width).into_iter().enumerate() {
            let has_content = !wrapped.to_string().trim().is_empty();
            let prefix = if let Some(marker) = hanging_list {
                if has_content {
                    marked = true;
                }
                if wrap_index == 0 {
                    "  ".to_owned()
                } else {
                    " ".repeat(2 + marker.prefix_width)
                }
            } else if list_marker.is_some() {
                if has_content {
                    marked = true;
                }
                " ".repeat(width.min(2))
            } else if let Some(depth) = continuation_depth {
                if has_content {
                    marked = true;
                }
                " ".repeat(list_item_indents.get(depth).copied().unwrap_or(2))
            } else if !marked && has_content {
                marked = true;
                block.marker.to_owned()
            } else {
                "  ".to_owned()
            };
            let inherited = wrapped.style.fg.unwrap_or(block.normal);
            let style = Style::default()
                .fg(inherited)
                .patch(block.block)
                .patch(block.base)
                .patch(wrapped.style);
            let prefix_style = if in_code && block.code_inset > 0 {
                block.block
            } else {
                style
            };
            let line_style = if in_code && block.code_inset > 0 {
                block.block
            } else {
                style
            };
            let mut spans = vec![Span::styled(prefix, prefix_style)];
            if hanging_list.is_some() && wrap_index == 0 {
                spans.extend(line_prefix.spans.iter().map(|span| {
                    Span::styled(
                        span.content.clone().into_owned(),
                        Style::default()
                            .fg(inherited)
                            .patch(block.block)
                            .patch(block.base)
                            .patch(span.style),
                    )
                }));
            }
            if in_code && code_padding > 0 {
                spans.push(Span::styled(" ".repeat(code_padding), style));
            }
            spans.extend(wrapped.spans.into_iter().map(|span| {
                let mut span_style = Style::default()
                    .fg(inherited)
                    .patch(block.block)
                    .patch(block.base)
                    .patch(span.style);
                if span_style.add_modifier.contains(Modifier::BOLD) {
                    span_style = span_style.fg(block.strong);
                }
                Span::styled(span.content.into_owned(), span_style)
            }));
            let used = spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();
            if in_code && block.code_inset > 0 {
                spans.push(Span::styled(
                    " ".repeat(width.saturating_sub(used + block.code_inset)),
                    style,
                ));
                spans.push(Span::styled(" ".repeat(block.code_inset), block.block));
            } else {
                spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), style));
            }
            lines.push(Line::from(spans).style(line_style));
        }
        if let Some(marker) = list_marker {
            list_markers += 1;
            let item_indent = width.min(2 + marker.prefix_width);
            list_item_indents.truncate(marker.depth);
            list_item_indents.resize(marker.depth + 1, item_indent);
            list_item_indents[marker.depth] = item_indent;
        }
        previous_was_blank = plain.trim().is_empty();
    }
    if !marked && !content.is_empty() {
        lines.push(Line::styled(
            format!("{}{}", block.marker, " ".repeat(width.saturating_sub(2))),
            block.block.patch(block.base),
        ));
    }
    if lines.len() > content_start && block.vertical_inset > 0 {
        let padding = || Line::styled(" ".repeat(width), block.block.patch(block.base));
        lines.splice(
            content_start..content_start,
            std::iter::repeat_with(padding).take(block.vertical_inset),
        );
        lines.extend(std::iter::repeat_with(padding).take(block.vertical_inset));
    }
}

#[derive(Clone, Copy)]
struct ListMarker {
    depth: usize,
    prefix_width: usize,
}

fn list_marker(line: &str) -> Option<ListMarker> {
    let leading = line.len() - line.trim_start_matches(' ').len();
    let trimmed = &line[leading..];
    let marker_width = if trimmed.starts_with("- ") {
        2
    } else {
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || !trimmed[digits..].starts_with(". ") {
            return None;
        }
        digits + 2
    };
    Some(ListMarker {
        depth: leading / 4,
        prefix_width: leading + marker_width,
    })
}

fn is_list_marker_only(line: &str) -> bool {
    list_marker(line)
        .is_some_and(|marker| UnicodeWidthStr::width(line.trim_end()) < marker.prefix_width)
}

#[derive(Clone, Copy)]
struct ListContinuationHint {
    after_marker: usize,
    depth: usize,
}

fn list_continuation_hints(content: &str) -> Vec<ListContinuationHint> {
    #[derive(Clone, Copy)]
    struct Item {
        indent: usize,
        content_indent: usize,
        depth: usize,
    }

    let mut items = Vec::<Item>::new();
    let mut hints = Vec::new();
    let mut marker = 0;
    let mut after_blank = false;
    let mut fence = None;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(delimiter) = fence {
            if trimmed.starts_with(delimiter) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            continue;
        }
        if line.trim().is_empty() {
            after_blank = true;
            continue;
        }
        if let Some((indent, marker_width)) = source_list_marker(line) {
            while items.last().is_some_and(|item| item.indent >= indent) {
                items.pop();
            }
            let depth = items.len();
            marker += 1;
            items.push(Item {
                indent,
                content_indent: indent + marker_width,
                depth,
            });
            after_blank = false;
            continue;
        }
        if after_blank {
            let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
            if let Some(item) = items
                .iter()
                .rev()
                .find(|item| indent >= item.content_indent)
            {
                hints.push(ListContinuationHint {
                    after_marker: marker,
                    depth: item.depth,
                });
            } else {
                items.clear();
            }
        }
        after_blank = false;
    }
    hints
}

fn source_list_marker(line: &str) -> Option<(usize, usize)> {
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    let trimmed = &line[indent..];
    if matches!(trimmed.as_bytes(), [b'-' | b'+' | b'*', b' ', ..]) {
        return Some((indent, 2));
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0 && trimmed[digits..].starts_with(". ")).then_some((indent, digits + 2))
}

fn split_styled_line(line: &Line<'_>, prefix_width: usize) -> (Line<'static>, Line<'static>) {
    let mut prefix = Vec::new();
    let mut content = Vec::new();
    let mut used = 0;
    for span in &line.spans {
        let mut before = String::new();
        let mut after = String::new();
        for character in span.content.chars() {
            if used < prefix_width {
                before.push(character);
                used += character.width().unwrap_or_default();
            } else {
                after.push(character);
            }
        }
        if !before.is_empty() {
            prefix.push(Span::styled(before, span.style));
        }
        if !after.is_empty() {
            content.push(Span::styled(after, span.style));
        }
    }
    (
        Line::from(prefix).style(line.style),
        Line::from(content).style(line.style),
    )
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

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
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

    fn trimmed(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    fn is_user_background(line: &Line<'_>) -> bool {
        line.style.bg == Some(Color::Rgb(38, 40, 45))
    }

    #[test]
    fn separator_policy_is_deterministic_for_every_block_kind() {
        let kinds = [
            TranscriptBlockKind::User,
            TranscriptBlockKind::Assistant,
            TranscriptBlockKind::Reasoning,
            TranscriptBlockKind::Tool,
            TranscriptBlockKind::Patch,
            TranscriptBlockKind::Note,
            TranscriptBlockKind::Error,
            TranscriptBlockKind::ProcessOutput,
            TranscriptBlockKind::ResponseStart,
            TranscriptBlockKind::TurnEnd,
            TranscriptBlockKind::CompactionEnd,
            TranscriptBlockKind::TurnWorking,
            TranscriptBlockKind::Activity,
        ];

        for next in kinds {
            assert_eq!(transcript_separator_lines(None, next), 0);
            for previous in kinds {
                assert_eq!(
                    transcript_separator_lines(Some(previous), next),
                    1,
                    "separator for {previous:?} -> {next:?}"
                );
            }
        }
    }

    #[test]
    fn non_user_block_renderers_do_not_add_outer_padding() {
        let cases = [
            ("phi", "assistant"),
            ("reasoning_summary", "reasoning"),
            ("tool", "tool"),
            ("patch", "patch"),
            ("note", "note"),
            ("error", "error"),
            ("processes", "process"),
            ("response_start", ""),
            ("turn_end", "done"),
            ("compaction_end", "compacted"),
            ("turn_working", "working"),
        ];

        for (role, content) in cases {
            let mut lines = Vec::new();
            push_message(&mut lines, role, content, 40);
            assert!(!lines.is_empty(), "renderer for {role}");
            assert!(
                !lines.first().unwrap().to_string().trim().is_empty(),
                "leading padding from {role}"
            );
            assert!(
                !lines.last().unwrap().to_string().trim().is_empty(),
                "trailing padding from {role}"
            );
        }
    }

    #[test]
    fn user_renderer_adds_one_full_width_background_inset() {
        for (content, width) in [
            ("hello", 20),
            ("first\nsecond", 20),
            ("a message that wraps", 8),
            ("**bold** and `code`", 20),
            ("- list item\n\n```text\ncode\n```", 20),
            ("hello", 1),
            ("hello", 0),
        ] {
            let mut lines = Vec::new();
            push_message(&mut lines, "you", content, width);
            assert!(lines.len() >= 3, "rendering {content:?} at width {width}");
            assert!(lines[0].to_string().trim().is_empty());
            assert!(lines.last().unwrap().to_string().trim().is_empty());
            assert!(is_user_background(&lines[0]));
            assert!(is_user_background(lines.last().unwrap()));
            assert_eq!(lines[0].width(), width);
            assert_eq!(lines.last().unwrap().width(), width);
            assert!(lines.iter().all(is_user_background));
            assert!(
                lines[1..lines.len() - 1]
                    .iter()
                    .any(|line| !line.to_string().trim().is_empty())
            );
        }
    }

    #[test]
    fn empty_user_block_has_no_orphan_inset() {
        let mut lines = Vec::new();
        push_message(&mut lines, "you", "", 20);
        assert!(lines.is_empty());
    }

    #[test]
    fn user_inset_does_not_consume_markdown_edge_blank_lines() {
        let content = "\nhello\n\n";
        let mut user = Vec::new();
        let mut assistant = Vec::new();
        push_message(&mut user, "you", content, 20);
        push_message(&mut assistant, "phi", content, 20);

        assert_eq!(user.len(), assistant.len() + 2);
        assert!(is_user_background(&user[0]));
        assert!(is_user_background(user.last().unwrap()));
        assert_eq!(
            trimmed(&user[1..user.len() - 1])
                .into_iter()
                .map(|line| line.replacen('‣', "•", 1))
                .collect::<Vec<_>>(),
            trimmed(&assistant)
        );
    }

    #[test]
    fn adjacent_blocks_have_exactly_one_compositor_separator() {
        let cases = [
            ("reasoning_summary", "reasoning", "tool", "tool"),
            ("reasoning_summary", "reasoning", "phi", "assistant"),
            (
                "reasoning_summary",
                "reasoning one",
                "reasoning_summary",
                "reasoning two",
            ),
            ("you", "user", "reasoning_summary", "reasoning"),
            ("you", "user", "phi", "assistant"),
            ("you", "user", "tool", "tool"),
            ("tool", "tool one", "tool", "tool two"),
            ("tool", "tool", "phi", "assistant"),
            ("tool", "tool", "patch", "patch"),
            ("patch", "patch", "tool", "tool"),
            ("note", "note", "error", "error"),
            ("error", "error", "processes", "process"),
            ("processes", "process", "note", "note"),
            ("response_start", "", "phi", "assistant"),
            ("turn_end", "done", "you", "user"),
            ("compaction_end", "compacted", "turn_end", "done"),
            ("turn_working", "working", "tool", "tool"),
        ];

        for width in [8, 40] {
            for (first_role, first_content, second_role, second_content) in cases {
                let mut first = Vec::new();
                push_message(&mut first, first_role, first_content, width);
                let mut second = Vec::new();
                push_message(&mut second, second_role, second_content, width);
                let mut expected = trimmed(&first);
                expected.push(String::new());
                expected.extend(trimmed(&second));
                expected.push(String::new());

                let mut app = app();
                app.push_transcript(first_role, first_content);
                app.push_transcript(second_role, second_content);
                assert_eq!(
                    trimmed(&transcript_text(&mut app, width).lines),
                    expected,
                    "rendering {first_role} -> {second_role} at width {width}"
                );
            }
        }
    }

    #[test]
    fn user_transitions_keep_container_insets_distinct_from_separators() {
        let other_blocks = [
            ("phi", "assistant"),
            ("reasoning_summary", "reasoning"),
            ("tool", "tool"),
            ("patch", "patch"),
            ("processes", "process"),
            ("note", "note"),
            ("error", "error"),
            ("turn_end", "done"),
        ];

        for (role, content) in other_blocks {
            for transcript in [
                [("you", "user"), (role, content)],
                [(role, content), ("you", "user")],
            ] {
                let mut app = app();
                for (role, content) in transcript {
                    app.push_transcript(role, content);
                }
                let lines = transcript_text(&mut app, 20).lines;
                let user = lines
                    .iter()
                    .position(|line| line.to_string().contains("user"))
                    .unwrap();
                assert!(is_user_background(&lines[user - 1]), "before {role}");
                assert!(is_user_background(&lines[user + 1]), "after {role}");
                let separator = if transcript[0].0 == "you" {
                    &lines[user + 2]
                } else {
                    &lines[user - 2]
                };
                assert!(separator.to_string().trim().is_empty(), "role {role}");
                assert_eq!(separator.style.bg, None, "role {role}");
            }
        }

        let mut app = app();
        app.push_transcript("you", "first");
        app.push_transcript("you", "second");
        let lines = transcript_text(&mut app, 20).lines;
        assert_eq!(lines.len(), 8);
        assert!(is_user_background(&lines[0]));
        assert!(is_user_background(&lines[2]));
        assert_eq!(lines[3].style.bg, None);
        assert!(is_user_background(&lines[4]));
        assert!(is_user_background(&lines[6]));
        assert_eq!(lines[7].style.bg, None);
    }

    #[test]
    fn streamed_assistant_spacing_is_stable_when_cached() {
        let mut app = app();
        app.push_transcript("reasoning_summary", "because");
        app.current_model = "final answer".into();
        app.current_model_changed();
        let streaming = trimmed(&transcript_text(&mut app, 20).lines);

        app.current_model.clear();
        app.current_model_changed();
        app.push_transcript("phi", "final answer");
        let cached = trimmed(&transcript_text(&mut app, 20).lines);

        assert_eq!(streaming, cached);
        let answer = cached
            .iter()
            .position(|line| line.contains("final"))
            .unwrap();
        assert!(!cached[answer - 2].is_empty());
        assert!(cached[answer - 1].is_empty());
    }

    #[test]
    fn empty_blocks_do_not_create_or_double_separators() {
        let mut with_empty = app();
        with_empty.push_transcript("reasoning_summary", "because");
        with_empty.push_transcript("phi", "");
        with_empty.push_transcript("tool", "tool");

        let mut without_empty = app();
        without_empty.push_transcript("reasoning_summary", "because");
        without_empty.push_transcript("tool", "tool");

        assert_eq!(
            trimmed(&transcript_text(&mut with_empty, 20).lines),
            trimmed(&transcript_text(&mut without_empty, 20).lines)
        );
        assert!(
            with_empty.transcript_cache[1]
                .as_ref()
                .unwrap()
                .lines
                .is_empty()
        );
    }

    #[test]
    fn mixed_transcript_window_matches_full_composition() {
        let mut app = app();
        for (role, content) in [
            ("you", "a wrapped user message"),
            ("reasoning_summary", "reasoning\ncontinued"),
            ("tool", "Ran `test`\n\nline one\nline two"),
            ("patch", "patch output"),
            ("phi", "```text\nanswer\n```"),
            ("turn_end", "done"),
        ] {
            app.push_transcript(role, content);
        }
        sync_transcript_cache(&mut app, 12);
        sync_live_model_cache(&mut app, 12);
        let tail = live_transcript_tail(&app, 12);
        let height = cached_transcript_height(&app) + tail.len();
        let full = transcript_window(&app, &tail, 0, height);

        assert_eq!(full.len(), height);
        assert!(
            app.transcript_offsets
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        for start in 0..=height {
            let end = (start + 5).min(height);
            assert_eq!(
                transcript_window(&app, &tail, start, end),
                full[start..end],
                "window {start}..{end}"
            );
        }
    }

    #[test]
    fn user_insets_are_included_in_cached_heights_offsets_windows_and_resizes() {
        let mut app = app();
        app.push_transcript("you", "abcdefghijk");
        app.push_transcript("phi", "ok");

        sync_transcript_cache(&mut app, 20);
        assert_eq!(app.transcript_offsets, [3, 5]);
        assert_eq!(cached_transcript_height(&app), 5);
        let full = transcript_window(&app, &[], 0, 5);
        assert!(is_user_background(&full[0]));
        assert!(is_user_background(&full[2]));
        assert_eq!(full[3].style.bg, None);
        assert_eq!(transcript_window(&app, &[], 1, 4), full[1..4]);

        sync_transcript_cache(&mut app, 8);
        assert_eq!(app.transcript_offsets, [4, 6]);
        assert_eq!(cached_transcript_height(&app), 6);
        let resized = transcript_window(&app, &[], 0, 6);
        assert!(is_user_background(&resized[0]));
        assert!(is_user_background(&resized[3]));
        assert_eq!(resized[4].style.bg, None);
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
            content: "Checked **the ".into(),
        });
        app.on_runtime(RuntimeEvent::ReasoningSummaryDelta {
            content: "request**.".into(),
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
                (
                    "reasoning_summary".into(),
                    "Checked **the request**.".into()
                ),
                ("tool".into(), "Read src/main.rs".into()),
                ("response_start".into(), String::new()),
            ]
        );
        assert_eq!(app.transcript[3], ("phi".into(), "Final answer".into()));
        let lines = transcript_text(&mut app, 48).lines;
        assert_eq!(lines[0].to_string().trim_end(), "  Checked the request.");
        let rendered = lines.iter().map(Line::to_string).collect::<String>();
        assert!(!rendered.contains("Provider reasoning summary"));
        assert!(rendered.contains("Checked the request."));
        assert!(!rendered.contains("**"));
        assert_eq!(rendered.matches("Final answer").count(), 1);
    }

    #[test]
    fn separates_streamed_final_response_after_reasoning_only() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ReasoningSummaryDelta {
            content: "Checked the request.".into(),
        });
        app.on_runtime(RuntimeEvent::ModelDelta {
            content: "Final ".into(),
        });
        app.on_runtime(RuntimeEvent::ModelDelta {
            content: "answer".into(),
        });
        app.on_runtime(RuntimeEvent::Finished {
            content: "Final answer".into(),
        });

        assert_eq!(
            app.transcript
                .iter()
                .map(|(role, content)| (role.as_str(), content.as_str()))
                .collect::<Vec<_>>(),
            [
                ("reasoning_summary", "Checked the request."),
                ("response_start", ""),
                ("phi", "Final answer"),
                ("turn_end", "Worked for 0s"),
            ]
        );
        assert_eq!(
            trimmed(&transcript_text(&mut app, 18).lines)[..6],
            [
                "  Checked the requ",
                "  est.",
                "",
                "──────────────────",
                "",
                "• Final answer",
            ]
        );
    }

    #[test]
    fn separates_finished_fallback_after_tool_and_patch_events() {
        for name in ["read_file", "patch"] {
            let mut app = app();
            app.on_runtime(RuntimeEvent::ToolStarted {
                call_id: name.into(),
                name: name.into(),
                arguments: serde_json::json!({ "path": "src/main.rs" }),
            });
            app.on_runtime(RuntimeEvent::ToolCompleted {
                call_id: name.into(),
                name: name.into(),
                result: if name == "patch" {
                    serde_json::json!({ "changes": [] })
                } else {
                    serde_json::json!({ "content": "source" })
                },
            });
            app.on_runtime(RuntimeEvent::Finished {
                content: "Fallback answer".into(),
            });

            assert_eq!(
                app.transcript
                    .iter()
                    .filter(|(role, _)| role == "response_start")
                    .count(),
                1,
                "{name} fallback"
            );
            assert_eq!(app.transcript[1].0, "response_start", "{name} fallback");
            assert_eq!(app.transcript[2], ("phi".into(), "Fallback answer".into()));
        }
    }

    #[test]
    fn inserts_one_response_divider_after_multiple_activity_cycles() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ReasoningSummaryDelta {
            content: "Planning.".into(),
        });
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "read".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({ "path": "src/main.rs" }),
        });
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "read".into(),
            name: "read_file".into(),
            result: serde_json::json!({ "content": "source" }),
        });
        app.on_runtime(RuntimeEvent::ReasoningSummaryDelta {
            content: "Reviewing.".into(),
        });
        app.on_runtime(RuntimeEvent::ContextCompactionStatus {
            job_id: "J1".into(),
            status: "applied".into(),
        });
        app.on_runtime(RuntimeEvent::ModelDelta {
            content: "Final ".into(),
        });
        app.on_runtime(RuntimeEvent::ModelDelta {
            content: "answer".into(),
        });
        app.on_runtime(RuntimeEvent::Finished {
            content: "Final answer".into(),
        });

        assert_eq!(
            app.transcript
                .iter()
                .filter(|(role, _)| role == "response_start")
                .count(),
            1
        );
        assert_eq!(
            app.transcript
                .iter()
                .map(|(role, _)| role.as_str())
                .collect::<Vec<_>>(),
            [
                "reasoning_summary",
                "tool",
                "reasoning_summary",
                "note",
                "response_start",
                "phi",
                "turn_end",
            ]
        );
    }

    #[test]
    fn does_not_separate_final_response_without_prior_activity() {
        for fallback in [false, true] {
            let mut app = app();
            if fallback {
                app.on_runtime(RuntimeEvent::Finished {
                    content: "Ordinary answer".into(),
                });
            } else {
                app.on_runtime(RuntimeEvent::ModelDelta {
                    content: "Ordinary answer".into(),
                });
                app.on_runtime(RuntimeEvent::Finished {
                    content: "Ordinary answer".into(),
                });
            }
            assert!(
                app.transcript
                    .iter()
                    .all(|(role, _)| role != "response_start")
            );
            assert_eq!(app.transcript[0], ("phi".into(), "Ordinary answer".into()));
        }
    }

    #[test]
    fn streams_commentary_around_tools_with_one_final_response_divider() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::CommentaryDelta {
            content: "Inspecting **the config**.".into(),
        });
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "read".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({ "path": "config.scm" }),
        });
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "read".into(),
            name: "read_file".into(),
            result: serde_json::json!({ "content": "config" }),
        });
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "status".into(),
            name: "list_processes".into(),
            arguments: serde_json::json!({}),
        });
        app.on_runtime(RuntimeEvent::ToolCompleted {
            call_id: "status".into(),
            name: "list_processes".into(),
            result: serde_json::json!({ "processes": [] }),
        });
        app.on_runtime(RuntimeEvent::CommentaryDelta {
            content: "The summary request is enabled.".into(),
        });
        app.on_runtime(RuntimeEvent::ModelDelta {
            content: "Updated it.".into(),
        });
        app.on_runtime(RuntimeEvent::Finished {
            content: "Updated it.".into(),
        });

        assert_eq!(
            app.transcript
                .iter()
                .map(|(role, _)| role.as_str())
                .collect::<Vec<_>>(),
            [
                "commentary",
                "tool",
                "tool",
                "commentary",
                "response_start",
                "phi",
                "turn_end"
            ]
        );
        assert_eq!(
            app.transcript
                .iter()
                .filter(|(role, _)| role == "response_start")
                .count(),
            1
        );
        let rendered = transcript_text(&mut app, 48)
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<String>();
        assert!(rendered.contains("Inspecting the config."));
        assert!(!rendered.contains("**"));
    }

    #[test]
    fn keeps_multiple_commentary_message_items_distinct_before_a_tool() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::CommentaryStarted);
        app.on_runtime(RuntimeEvent::CommentaryDelta {
            content: "First checkpoint.".into(),
        });
        app.on_runtime(RuntimeEvent::CommentaryStarted);
        app.on_runtime(RuntimeEvent::CommentaryDelta {
            content: "Second checkpoint.".into(),
        });
        app.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "read".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({ "path": "config.scm" }),
        });

        assert_eq!(
            app.transcript
                .iter()
                .map(|(role, content)| (role.as_str(), content.as_str()))
                .collect::<Vec<_>>(),
            [
                ("commentary", "First checkpoint."),
                ("commentary", "Second checkpoint."),
                ("tool", "Read config.scm")
            ]
        );
    }

    #[test]
    fn streamed_commentary_matches_cached_rendering_at_narrow_widths() {
        let mut streamed = app();
        streamed.on_runtime(RuntimeEvent::CommentaryStarted);
        streamed.on_runtime(RuntimeEvent::CommentaryDelta {
            content: "Checking **the narrow transcript** carefully.".into(),
        });
        let live = transcript_text(&mut streamed, 16).lines;

        let mut cached = app();
        cached.push_transcript(
            "commentary",
            "Checking **the narrow transcript** carefully.",
        );
        let finalized = transcript_text(&mut cached, 16).lines;

        assert_eq!(live, finalized);
        assert_eq!(streamed.transcript_offsets, cached.transcript_offsets);
    }

    #[test]
    fn empty_commentary_has_no_layout_effect() {
        let mut with_empty = app();
        with_empty.on_runtime(RuntimeEvent::CommentaryDelta {
            content: " \n\t".into(),
        });
        with_empty.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "read".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({ "path": "config.scm" }),
        });

        let mut without_empty = app();
        without_empty.on_runtime(RuntimeEvent::ToolStarted {
            call_id: "read".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({ "path": "config.scm" }),
        });

        assert_eq!(
            trimmed(&transcript_text(&mut with_empty, 30).lines),
            trimmed(&transcript_text(&mut without_empty, 30).lines)
        );

        let mut before_answer = app();
        before_answer.on_runtime(RuntimeEvent::CommentaryStarted);
        before_answer.on_runtime(RuntimeEvent::CommentaryDelta {
            content: " \n\t".into(),
        });
        before_answer.on_runtime(RuntimeEvent::ModelDelta {
            content: "Answer".into(),
        });
        assert!(
            !before_answer
                .transcript
                .iter()
                .any(|(role, _)| role == "response_start")
        );
    }

    #[test]
    fn cancellation_keeps_streamed_commentary_visible() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::CommentaryStarted);
        app.on_runtime(RuntimeEvent::CommentaryDelta {
            content: "Work completed before cancellation.".into(),
        });
        app.on_runtime(RuntimeEvent::Error {
            message: "cancelled".into(),
        });

        assert_eq!(app.transcript[0].0, "commentary");
        assert_eq!(app.transcript[1].1, "Cancelled by user");
    }

    #[test]
    fn restored_history_preserves_commentary_tool_and_final_order() {
        let mut app = app();
        app.push_transcript("you", "new question");
        app.on_runtime(RuntimeEvent::History {
            messages: vec![
                serde_json::json!({
                    "kind": "message", "role": "user", "content": "old question"
                }),
                serde_json::json!({
                    "kind": "message", "role": "assistant",
                    "phase": "commentary", "content": "Checking."
                }),
                serde_json::json!({
                    "kind": "tool_call", "call_id": "read", "name": "read_file",
                    "arguments": "{\"path\":\"config.scm\"}"
                }),
                serde_json::json!({
                    "kind": "tool_result", "call_id": "read",
                    "content": "{\"content\":\"config\"}"
                }),
                serde_json::json!({
                    "kind": "message", "role": "assistant",
                    "phase": "final_answer", "content": "Done."
                }),
            ],
        });

        assert_eq!(
            app.transcript
                .iter()
                .map(|(role, _)| role.as_str())
                .collect::<Vec<_>>(),
            ["you", "commentary", "tool", "response_start", "phi", "you"]
        );
        assert_eq!(app.transcript.last().unwrap().1, "new question");
    }

    #[test]
    fn restored_history_keeps_legacy_reasoning_summaries_readable() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::History {
            messages: vec![
                serde_json::json!({
                    "kind": "reasoning_summary", "content": "Legacy **summary**."
                }),
                serde_json::json!({
                    "kind": "message", "role": "assistant",
                    "phase": "final_answer", "content": "Answer."
                }),
            ],
        });

        assert_eq!(app.transcript[0].0, "reasoning_summary");
        assert_eq!(app.transcript[1].0, "response_start");
        assert_eq!(app.transcript[2].1, "Answer.");
        let rendered = transcript_text(&mut app, 30)
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<String>();
        assert!(rendered.contains("Legacy summary."));
    }

    #[test]
    fn streamed_reasoning_matches_finalized_cached_rendering() {
        let mut streamed = app();
        streamed.on_runtime(RuntimeEvent::ReasoningSummaryDelta {
            content: "Checked **the ".into(),
        });
        let partial = transcript_text(&mut streamed, 18).lines;
        assert!(partial[0].to_string().starts_with("  Checked"));
        assert!(
            !partial
                .iter()
                .any(|line| line.to_string().contains("Provider"))
        );

        streamed.on_runtime(RuntimeEvent::ReasoningSummaryDelta {
            content: "request** carefully.".into(),
        });
        streamed.on_runtime(RuntimeEvent::ModelDelta {
            content: "Final answer".into(),
        });
        let live = transcript_text(&mut streamed, 18).lines;

        let mut cached = app();
        cached.push_transcript("reasoning_summary", "Checked **the request** carefully.");
        cached.push_transcript("response_start", "");
        cached.current_model = "Final answer".into();
        cached.current_model_changed();
        let finalized = transcript_text(&mut cached, 18).lines;

        assert_eq!(live, finalized);
        assert_eq!(streamed.transcript_offsets, cached.transcript_offsets);
        assert_eq!(streamed.transcript_offsets, [2, 4]);
        assert_eq!(
            trimmed(&live),
            [
                "  Checked the requ",
                "  est carefully.",
                "",
                "──────────────────",
                "",
                "• Final answer",
                ""
            ]
        );
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
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), content);
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
    fn keys_view_documents_interactions_and_exposes_detailed_tokens() {
        let mut app = app();
        app.on_runtime(RuntimeEvent::ContextUpdated {
            estimated_tokens: 1_500,
            token_budget: 6_000,
            compactions: 0,
            input_tokens: Some(1_234),
            cached_tokens: Some(1_024),
            cache_write_tokens: Some(128),
            output_tokens: Some(50),
        });
        app.composer.text = "/keys".into();

        app.submit();

        assert!(app.command_task.is_none());
        let (role, content) = app.transcript.last().unwrap();
        assert_eq!(role, "note");
        for expected in [
            "Send / steer",
            "History",
            "Scroll",
            "Queue next turn",
            "Cancel",
            "Picker",
            "Approval",
            "Context: 1500 / 6000",
            "Input: 1234",
            "Cached input: 1024",
            "Cache write: 128",
            "Output: 50",
        ] {
            assert!(
                content.contains(expected),
                "missing {expected:?}: {content}"
            );
        }
    }

    #[test]
    fn keys_command_rejects_arguments_locally() {
        let mut app = app();
        app.composer.text = "/keys now".into();
        app.submit();
        assert_eq!(
            app.transcript.last().unwrap(),
            &("error".into(), "usage: /keys".into())
        );
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
        assert_eq!(text.lines.len(), 4);
        assert_eq!(text.lines[0].to_string().trim(), "• Question?");
        assert_eq!(text.lines[1].to_string().trim(), "");
        assert_eq!(text.lines[2].to_string().trim(), "Answer.");
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
            text.lines[..3]
                .iter()
                .all(|line| { UnicodeWidthStr::width(line.to_string().as_str()) == 20 })
        );
        assert!(
            text.lines[..3]
                .iter()
                .all(|line| line.style.bg == Some(Color::Rgb(38, 40, 45)))
        );
        assert!(text.lines[0].to_string().trim().is_empty());
        assert!(text.lines[1].to_string().starts_with("‣ hello"));
        assert!(text.lines[2].to_string().trim().is_empty());
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
        assert_eq!(code_index, lines.len() - 1);
    }

    #[test]
    fn fenced_code_uses_one_separator_only_when_adjacent_to_markdown_content() {
        let cases = [
            ("```text\none\n```", vec!["• one"]),
            ("before\n\n```text\none\n```", vec!["• before", "", "  one"]),
            ("```text\none\n```\n\nafter", vec!["• one", "", "  after"]),
            (
                "before\n\n```text\none\n```\n\nafter",
                vec!["• before", "", "  one", "", "  after"],
            ),
            (
                "```text\none\n```\n\n```text\ntwo\n```",
                vec!["• one", "", "  two"],
            ),
        ];

        for (content, expected) in cases {
            let mut lines = Vec::new();
            push_message(&mut lines, "phi", content, 30);
            assert_eq!(trimmed(&lines), expected, "rendering {content:?}");
        }
    }

    #[test]
    fn fenced_code_does_not_stack_with_transcript_block_separators() {
        let adjacent_blocks = [
            ("tool", "before", "patch", "after"),
            ("patch", "before", "reasoning_summary", "after"),
            ("reasoning_summary", "before", "turn_end", "after"),
            ("turn_end", "before", "tool", "after"),
        ];
        for role in ["you", "phi", "processes", "reasoning_summary"] {
            for (before_role, before, after_role, after) in adjacent_blocks {
                let mut app = app();
                app.push_transcript(before_role, before);
                app.push_transcript(role, "```text\ncode\n```");
                app.push_transcript(after_role, after);
                let lines = transcript_text(&mut app, 30).lines;
                let rendered = trimmed(&lines);
                let code = rendered
                    .iter()
                    .position(|line| line.contains("code"))
                    .unwrap();

                assert!(rendered[code - 1].is_empty(), "before {role} code");
                assert!(rendered[code + 1].is_empty(), "after {role} code");
                if role == "you" {
                    assert!(is_user_background(&lines[code - 1]));
                    assert_eq!(lines[code - 2].style.bg, None);
                    assert!(is_user_background(&lines[code + 1]));
                    assert_eq!(lines[code + 2].style.bg, None);
                } else {
                    assert!(
                        !rendered[code - 2].is_empty(),
                        "duplicate separator before {role} code"
                    );
                    assert!(
                        !rendered[code + 2].is_empty(),
                        "duplicate separator after {role} code"
                    );
                }
            }
        }
    }

    #[test]
    fn non_user_fenced_code_at_markdown_content_edges_has_no_outer_padding() {
        for role in ["phi", "processes", "reasoning_summary"] {
            let mut lines = Vec::new();
            push_message(&mut lines, role, "```text\ncode\n```\n\nafter", 30);
            let rendered = trimmed(&lines);
            let code = rendered
                .iter()
                .position(|line| line.contains("code"))
                .unwrap();
            assert_eq!(rendered[code + 1], "", "code -> prose for {role}");
            assert!(!rendered[code + 2].is_empty(), "prose after {role} code");

            lines.clear();
            push_message(&mut lines, role, "before\n\n```text\ncode\n```", 30);
            let rendered = trimmed(&lines);
            let code = rendered
                .iter()
                .position(|line| line.contains("code"))
                .unwrap();
            assert_eq!(rendered[code - 1], "", "prose -> code for {role}");
            assert_eq!(code, rendered.len() - 1, "trailing padding for {role}");
        }
    }

    #[test]
    fn fenced_code_preserves_content_blank_lines_and_narrow_wrapping() {
        let mut lines = Vec::new();
        push_message(&mut lines, "phi", "```text\n\nalpha\n\n```", 30);
        assert_eq!(trimmed(&lines), ["", "• alpha", ""]);

        lines.clear();
        push_message(&mut lines, "phi", "```text\nabcdefghij\n```", 8);
        assert_eq!(trimmed(&lines), ["• abcdef", "  ghij"]);

        lines.clear();
        push_message(&mut lines, "phi", "```\n```", 8);
        assert_eq!(trimmed(&lines), ["•"]);
    }

    #[test]
    fn streamed_fenced_code_spacing_is_stable_when_cached() {
        let mut app = app();
        app.push_transcript("reasoning_summary", "because");
        app.current_model = "before\n\n```text\nanswer\n```".into();
        app.current_model_changed();
        let streaming = trimmed(&transcript_text(&mut app, 20).lines);

        app.current_model.clear();
        app.current_model_changed();
        app.push_transcript("phi", "before\n\n```text\nanswer\n```");
        let cached = trimmed(&transcript_text(&mut app, 20).lines);

        assert_eq!(streaming, cached);
        assert_eq!(cached, ["  because", "", "• before", "", "  answer", ""]);
    }

    #[test]
    fn renders_reasoning_summary_markdown_with_italic_base_style() {
        let mut lines = Vec::new();
        push_message(
            &mut lines,
            "reasoning_summary",
            "plain **bold** *italic* ***both*** `inline` [link](https://example.com)\n\n- list item\n\n```rust\nfn main() {}\n```",
            80,
        );

        let rendered = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(lines[0].to_string().starts_with("  plain "));
        assert!(!rendered.contains("Provider reasoning summary"));
        assert!(!rendered.contains("**"));
        assert!(!rendered.contains("*italic*"));
        assert!(!rendered.contains("`inline`"));
        assert!(!rendered.contains("[link]"));
        assert!(!rendered.contains("```"));
        assert!(rendered.contains("list item"));
        assert!(rendered.contains("fn main"));

        let spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect::<Vec<_>>();
        let plain = spans.iter().find(|span| span.content == "plain ").unwrap();
        assert_eq!(plain.style.fg, Some(Color::Gray));
        assert!(plain.style.add_modifier.contains(Modifier::ITALIC));

        for content in ["bold", "both"] {
            let span = spans.iter().find(|span| span.content == content).unwrap();
            assert!(span.style.add_modifier.contains(Modifier::BOLD));
            assert!(span.style.add_modifier.contains(Modifier::ITALIC));
            assert_eq!(span.style.fg, Some(Color::Gray));
        }
        let italic = spans.iter().find(|span| span.content == "italic").unwrap();
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
        let inline = spans.iter().find(|span| span.content == "inline").unwrap();
        assert!(inline.style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(inline.style.bg, Some(Color::Rgb(27, 28, 31)));
        let link = spans
            .iter()
            .find(|span| span.content.contains("https://example.com"))
            .unwrap();
        assert!(link.style.add_modifier.contains(Modifier::ITALIC));
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
        let code = lines
            .iter()
            .find(|line| line.to_string().contains("fn main"))
            .unwrap();
        assert!(
            code.spans
                .iter()
                .filter(|span| !span.content.trim().is_empty())
                .all(|span| span.style.add_modifier.contains(Modifier::ITALIC))
        );
    }

    #[test]
    fn reasoning_summary_plain_text_is_the_first_rendered_line() {
        let mut lines = Vec::new();
        push_message(
            &mut lines,
            "reasoning_summary",
            "First paragraph\n\nSecond paragraph",
            40,
        );

        assert_eq!(
            trimmed(&lines),
            ["  First paragraph", "", "  Second paragraph"]
        );
        assert!(lines[0].to_string().contains("First paragraph"));
        assert!(
            !lines
                .iter()
                .any(|line| line.to_string().contains("Provider"))
        );
    }

    #[test]
    fn wraps_formatted_reasoning_without_losing_styles() {
        let mut lines = Vec::new();
        push_message(
            &mut lines,
            "reasoning_summary",
            "**This formatted reasoning is long enough to wrap across several narrow lines.**",
            18,
        );

        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.to_string().as_str()) == 18)
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .filter(|span| !span.content.trim().is_empty())
                .all(|span| {
                    span.style.add_modifier.contains(Modifier::BOLD)
                        && span.style.add_modifier.contains(Modifier::ITALIC)
                })
        );
    }

    #[test]
    fn empty_reasoning_summaries_render_no_orphan_block() {
        for content in ["", " ", " \n\t"] {
            let mut lines = Vec::new();
            push_message(&mut lines, "reasoning_summary", content, 40);
            assert!(lines.is_empty(), "rendering {content:?}");
        }

        let mut with_empty = app();
        with_empty.push_transcript("you", "question");
        with_empty.push_transcript("reasoning_summary", " \n\t");
        with_empty.push_transcript("tool", "tool");
        let mut without_empty = app();
        without_empty.push_transcript("you", "question");
        without_empty.push_transcript("tool", "tool");
        assert_eq!(
            transcript_text(&mut with_empty, 40).lines,
            transcript_text(&mut without_empty, 40).lines
        );
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

    #[test]
    fn rapid_model_deltas_render_once_per_frame_and_preserve_scroll_input() {
        let mut app = app();
        app.follow = false;
        app.scroll = 12;
        for _ in 0..10_000 {
            app.on_runtime(RuntimeEvent::ModelDelta {
                content: "x".into(),
            });
        }

        assert_eq!(app.current_model_render_count, 0);
        app.scroll_up(3);
        transcript_text(&mut app, 80);

        assert_eq!(app.scroll, 9);
        assert!(!app.follow);
        assert_eq!(app.current_model_render_count, 1);
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
                assert!(
                    app.transcript
                        .iter()
                        .any(|entry| entry == &("you".into(), "steer".into()))
                );

                app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                assert!(app.next_turn_queue.is_empty());
                assert_eq!(app.restart_after_cancel, Some(vec!["steer".into()]));

                app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                assert_eq!(app.restart_after_cancel, None);
                assert!(
                    !app.transcript
                        .iter()
                        .any(|entry| entry == &("you".into(), "steer".into()))
                );
            })
            .await;
    }

    #[tokio::test]
    async fn escape_displays_preserved_steering_before_cancellation_finishes() {
        let mut app = app();
        app.handle = Some(phi_runtime::start(app.options.clone(), "work".into()));
        app.steering_queue.push_back("steer now".into());

        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.status, "cancelling");
        assert_eq!(app.restart_after_cancel, Some(vec!["steer now".into()]));
        assert_eq!(
            app.transcript
                .iter()
                .filter(|entry| entry == &&("you".into(), "steer now".into()))
                .count(),
            1
        );

        app.on_runtime(RuntimeEvent::QueuedMessagesInjected {
            contents: vec!["steer now".into()],
        });
        app.on_runtime(RuntimeEvent::Error {
            message: "cancelled".into(),
        });

        assert_eq!(
            app.transcript
                .iter()
                .filter(|entry| entry == &&("you".into(), "steer now".into()))
                .count(),
            1
        );
        app.handle.as_ref().unwrap().cancel();
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
        assert!(lines[0].to_string().starts_with("• Ran `rg"));
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(lines[1].to_string(), "");
        assert_eq!(lines[2].to_string(), "  └ match");
        assert_eq!(lines[3].to_string(), "    second");
        assert_eq!(lines[2].style.fg, Some(Color::Rgb(125, 125, 122)));
    }

    #[test]
    fn leaves_one_blank_line_between_tool_calls() {
        let mut lines = Vec::new();
        push_transcript_block(
            &mut lines,
            None,
            TranscriptBlockKind::Tool,
            "tool",
            "Ran `one`",
            40,
        );
        push_transcript_block(
            &mut lines,
            Some(TranscriptBlockKind::Tool),
            TranscriptBlockKind::Tool,
            "tool",
            "Ran `two`",
            40,
        );

        assert_eq!(lines.len(), 3);
        assert!(lines[0].to_string().contains("Ran `one`"));
        assert!(lines[1].to_string().trim().is_empty());
        assert!(lines[2].to_string().contains("Ran `two`"));
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
        assert_eq!(rendered[4].to_string(), "    … (+ 2 lines)");
        assert_eq!(rendered[4].style.fg, Some(Color::Rgb(125, 125, 122)));

        assert_eq!(truncate_middle_width("abcdefghijk", 5), "ab…jk");
        let mut single_long_line = Vec::new();
        push_tool(
            &mut single_long_line,
            &format!("Ran `json`\n\n{}", "x".repeat(200)),
            40,
        );
        assert_eq!(single_long_line.len(), 3);
        assert_eq!(
            UnicodeWidthStr::width(single_long_line[2].to_string().as_str()),
            40
        );
        assert!(single_long_line[2].to_string().contains('…'));
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
        assert_eq!(lines.len(), 1);

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
                .any(|line| line.to_string().starts_with("  1. First"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().starts_with("  2. Second"))
        );
        assert!(!lines.iter().any(|line| line.to_string().trim() == "1."));
        assert!(!lines.iter().any(|line| line.to_string().trim() == "2."));
    }

    #[test]
    fn markdown_lists_have_consistent_inset_and_hanging_indent() {
        let cases = [
            (
                "1. First numbered item\n2. Second numbered item",
                vec![
                    "  1. First numbe",
                    "     red item",
                    "  2. Second numb",
                    "     ered item",
                ],
            ),
            (
                "- first unordered item\n- second unordered item",
                vec![
                    "  - first unorde",
                    "    red item",
                    "  - second unord",
                    "    ered item",
                ],
            ),
        ];
        for (content, expected) in cases {
            let mut lines = Vec::new();
            push_message(&mut lines, "phi", content, 16);
            assert_eq!(trimmed(&lines), expected, "rendering {content:?}");
            assert!(lines.iter().all(|line| line.width() == 16));
        }

        for marker in ['-', '+', '*'] {
            let mut lines = Vec::new();
            push_message(&mut lines, "phi", &format!("{marker} item"), 16);
            assert_eq!(trimmed(&lines), ["  - item"]);
        }

        let mut narrow = Vec::new();
        push_message(&mut narrow, "phi", "10. item", 4);
        assert_eq!(trimmed(&narrow), ["  10", "  .", "  it", "  em"]);
        assert!(narrow.iter().all(|line| line.width() == 4));
    }

    #[test]
    fn markdown_lists_preserve_nested_and_paragraph_indentation() {
        let mut lines = Vec::new();
        push_message(
            &mut lines,
            "phi",
            "10. outer item\n    - nested item\n\n      nested paragraph\n\n11. final outer item",
            24,
        );
        assert_eq!(
            trimmed(&lines),
            [
                "  10. outer item",
                "      - nested item",
                "",
                "        nested paragraph",
                "  11. final outer item",
            ]
        );
        assert!(lines.iter().all(|line| line.width() == 24));
    }

    #[test]
    fn markdown_list_layout_is_shared_and_stable_when_cached() {
        let content = "- an item that wraps across lines";
        for role in ["you", "phi", "processes"] {
            let mut lines = Vec::new();
            push_message(&mut lines, role, content, 18);
            let expected = if role == "you" {
                vec![
                    "",
                    "  - an item that w",
                    "    raps across li",
                    "    nes",
                    "",
                ]
            } else {
                vec!["  - an item that w", "    raps across li", "    nes"]
            };
            assert_eq!(trimmed(&lines), expected, "role {role}");
        }

        let mut reasoning = Vec::new();
        push_message(&mut reasoning, "reasoning_summary", content, 18);
        assert_eq!(
            trimmed(&reasoning),
            ["  - an item that w", "    raps across li", "    nes"]
        );

        let mut app = app();
        app.current_model = content.into();
        app.current_model_changed();
        let live = trimmed(&transcript_text(&mut app, 18).lines);
        app.current_model.clear();
        app.current_model_changed();
        app.push_transcript("phi", content);
        let cached = trimmed(&transcript_text(&mut app, 18).lines);
        assert_eq!(live, cached);
        assert_eq!(
            cached,
            ["  - an item that w", "    raps across li", "    nes", ""]
        );
    }

    #[test]
    fn markdown_lists_keep_inset_without_adding_block_spacing() {
        let mut app = app();
        app.push_transcript("phi", "before\n\n- item\n\n```text\ncode\n```");
        app.push_transcript("tool", "Ran `test`");
        app.push_transcript("patch", "Updated file");
        app.push_transcript("reasoning_summary", "1. reason");
        app.push_transcript("turn_end", "done");

        let rendered = trimmed(&transcript_text(&mut app, 24).lines);
        for expected in ["  - item", "  1. reason"] {
            let index = rendered.iter().position(|line| line == expected).unwrap();
            assert_eq!(
                rendered[index].chars().position(|char| char != ' '),
                Some(2)
            );
        }
        assert!(
            !rendered
                .windows(2)
                .any(|lines| lines[0].is_empty() && lines[1].is_empty())
        );
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
            rendered.iter().any(|line| line.starts_with("  1. one")),
            "rendered lines: {rendered:#?}"
        );
        assert!(rendered.iter().any(|line| line.starts_with("  2. two")));
        assert!(rendered.iter().any(|line| line.starts_with("  3. three")));
        assert!(rendered.iter().any(|line| line.starts_with("  4. four")));
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
    fn picker_cancellation_names_the_active_stage() {
        let cases = [
            (Picker::Model { selected: 0 }, "Model selection cancelled."),
            (
                Picker::Reasoning {
                    model: "test/model".into(),
                    selected: 0,
                },
                "Reasoning selection cancelled.",
            ),
            (
                Picker::ServiceTier {
                    model: "test/model".into(),
                    reasoning: "low".into(),
                    selected: 0,
                },
                "Service tier selection cancelled.",
            ),
        ];

        for (picker, expected) in cases {
            let mut app = app();
            app.picker = Some(picker);
            app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert!(app.picker.is_none());
            assert_eq!(app.transcript.last().unwrap().1, expected);
        }
    }

    #[tokio::test]
    async fn ctrl_c_during_slash_command_reports_unavailable_cancellation() {
        let mut app = app();
        app.command_task = Some(tokio::spawn(async {
            pending::<Result<CommandExecution>>().await
        }));

        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(!app.quit);
        assert!(app.command_task.is_some());
        assert_eq!(
            app.transcript.last().unwrap(),
            &(
                "note".into(),
                "Slash command cancellation is unavailable; waiting for it to finish.".into()
            )
        );
        app.command_task.take().unwrap().abort();
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
    fn approval_dialog_is_clamped_and_renders_on_narrow_terminals() {
        for (width, height) in [(1, 1), (8, 3), (24, 4), (49, 8), (80, 10)] {
            let terminal_area = Rect::new(0, 0, width, height);
            let dialog = centered(terminal_area, width.min(60), height.min(8));
            assert!(dialog.right() <= terminal_area.right());
            assert!(dialog.bottom() <= terminal_area.bottom());
            assert_eq!(dialog.width, width.min(60));
            assert_eq!(dialog.height, height.min(8));

            let mut app = app();
            app.approval = Some(ApprovalPrompt {
                name: "patch".into(),
                detail: "patch: 240 characters across a deliberately narrow approval dialog".into(),
            });
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            if height >= 3 {
                let rendered = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(rendered.contains(if width >= 19 {
                    "[y] yes   [n] no"
                } else if width >= 9 {
                    "[y] [n]"
                } else {
                    "y/n"
                }));
                if width >= 24 && height >= 4 {
                    assert!(rendered.contains("patch: 240"));
                }
            }
        }
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
