use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use phi_protocol::{Effect, Event};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use phi_protocol::{CommandInvocation, CommandSpec, ModelSpec, PickerOptionSpec};

#[derive(Clone)]
pub struct RunOptions {
    pub workspace: PathBuf,
    pub config_path: PathBuf,
    pub session_id: Option<String>,
    pub allow_shell: bool,
    pub allow_write: bool,
    pub interactive_approvals: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Session {
        id: String,
    },
    UserMessage {
        content: String,
    },
    ContextUpdated {
        estimated_tokens: u64,
        token_budget: u64,
        compactions: u64,
        input_tokens: Option<u64>,
        cached_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    ModelDelta {
        content: String,
    },
    ToolStarted {
        name: String,
        arguments: serde_json::Value,
    },
    ToolCompleted {
        name: String,
        result: serde_json::Value,
    },
    ApprovalRequested {
        name: String,
    },
    Finished {
        content: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug)]
pub enum RuntimeCommand {
    ApproveOnce,
    Deny,
}

pub struct Handle {
    pub events: mpsc::UnboundedReceiver<RuntimeEvent>,
    pub commands: mpsc::UnboundedSender<RuntimeCommand>,
    cancellation: CancellationToken,
}

#[derive(Debug, Clone)]
pub struct CommandCatalog {
    pub commands: Vec<CommandSpec>,
    pub models: Vec<ModelSpec>,
    pub selected_model: Option<String>,
    pub selected_reasoning: Option<String>,
    pub selected_service_tier: Option<String>,
}

pub struct CommandExecution {
    pub session_id: String,
    pub content: String,
    pub catalog: CommandCatalog,
}

impl Handle {
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
}

#[derive(Deserialize)]
struct Config {
    policy: PathBuf,
    provider: PathBuf,
    prompt: PathBuf,
    compaction: PathBuf,
    allowed_programs: HashSet<String>,
    allowed_http_origins: HashSet<String>,
    secrets: BTreeMap<String, phi_core::http::SecretConfig>,
    context_char_budget: usize,
}

pub fn command_catalog(options: &RunOptions) -> Result<CommandCatalog> {
    let workspace = options.workspace.canonicalize()?;
    let config = load_config(&options.config_path)?;
    let (sources, state) = match &options.session_id {
        Some(id) => {
            let session = phi_core::session::Session::open(&workspace.join(".phi/sessions"), id)?;
            let sources = session.sources(&config.prompt)?;
            (sources, Some(session.load_state()?))
        }
        None => {
            let active =
                phi_core::policy_store::active(&workspace.join(".phi/policies"), &config.policy)?;
            (
                phi_core::session::Sources {
                    policy: active,
                    provider: config.provider.clone(),
                    prompt: config.prompt.clone(),
                    compaction: config.compaction.clone(),
                },
                None,
            )
        }
    };
    let capabilities = capabilities(&sources);
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.policy,
        &sources.provider,
        &sources.prompt,
        &sources.compaction,
        &policy_config(
            &config,
            &capabilities,
            options.session_id.as_deref().unwrap_or("catalog"),
        ),
        state,
    )?;
    catalog(&mut policy)
}

pub fn execute_command(
    options: &RunOptions,
    invocation: &CommandInvocation,
) -> Result<CommandExecution> {
    let workspace = options.workspace.canonicalize()?;
    let config = load_config(&options.config_path)?;
    let sessions = workspace.join(".phi/sessions");
    let (session, sources, saved_state) = match &options.session_id {
        Some(id) => {
            let session = phi_core::session::Session::open(&sessions, id)?;
            let sources = session.sources(&config.prompt)?;
            let state = session.load_state()?;
            (session, sources, Some(state))
        }
        None => {
            let active =
                phi_core::policy_store::active(&workspace.join(".phi/policies"), &config.policy)?;
            let session = phi_core::session::Session::create(
                &sessions,
                &active,
                &config.provider,
                &config.prompt,
                &config.compaction,
            )?;
            let sources = session.sources(&config.prompt)?;
            (session, sources, None)
        }
    };
    let capabilities = capabilities(&sources);
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.policy,
        &sources.provider,
        &sources.prompt,
        &sources.compaction,
        &policy_config(&config, &capabilities, session.id()),
        saved_state,
    )?;
    let initial_catalog = catalog(&mut policy)?;
    let content = match invocation.name.as_str() {
        "help" => help(&initial_catalog),
        "model" => model_command(
            &session,
            &mut policy,
            &initial_catalog,
            &invocation.arguments,
        )?,
        name => {
            let command = initial_catalog
                .commands
                .iter()
                .find(|command| command.name == name)
                .with_context(|| format!("unknown command: /{name}"))?;
            if command.source == "core" {
                bail!("unsupported core command: /{name}");
            }
            let content = policy.run_command(name, &invocation.arguments)?;
            session.save_state(policy.state())?;
            content
        }
    };
    Ok(CommandExecution {
        session_id: session.id().into(),
        content,
        catalog: catalog(&mut policy)?,
    })
}

fn catalog(policy: &mut phi_steel::Policy) -> Result<CommandCatalog> {
    let mut commands = vec![
        CommandSpec {
            name: "help".into(),
            usage: "/help".into(),
            description: "List available commands.".into(),
            source: "core".into(),
        },
        CommandSpec {
            name: "model".into(),
            usage: "/model [MODEL [REASONING [SERVICE_TIER]]]".into(),
            description: "Show or select model settings.".into(),
            source: "core".into(),
        },
    ];
    commands.extend(policy.commands()?);
    let mut names = HashSet::new();
    for command in &commands {
        if !valid_name(&command.name) {
            bail!("invalid command name: {}", command.name);
        }
        if !names.insert(command.name.clone()) {
            bail!("duplicate command: /{}", command.name);
        }
    }
    commands.sort_by(|left, right| left.name.cmp(&right.name));

    let models = policy.models()?;
    let mut model_ids = HashSet::new();
    for model in &models {
        if !model_ids.insert(model.id.clone()) {
            bail!("duplicate model: {}", model.id);
        }
        if !model.reasoning.is_empty()
            && !model
                .reasoning
                .iter()
                .any(|option| option.id() == model.default_reasoning)
        {
            bail!("invalid default reasoning for model: {}", model.id);
        }
        if !model.service_tiers.is_empty()
            && !model
                .service_tiers
                .iter()
                .any(|option| option.id() == model.default_service_tier)
        {
            bail!("invalid default service tier for model: {}", model.id);
        }
    }
    if !models.is_empty() && models.iter().filter(|model| model.default).count() != 1 {
        bail!("provider must register exactly one default model");
    }
    let state: serde_json::Value = serde_json::from_str(policy.state())?;
    let selected_model = state["model"].as_str().map(str::to_owned).or_else(|| {
        models
            .iter()
            .find(|model| model.default)
            .map(|model| model.id.clone())
    });
    let selected_spec = selected_model
        .as_deref()
        .and_then(|id| models.iter().find(|model| model.id == id));
    let selected_reasoning = state["reasoning"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| selected_spec.map(|model| model.default_reasoning.clone()))
        .filter(|value| !value.is_empty());
    let selected_service_tier = state["service_tier"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| selected_spec.map(|model| model.default_service_tier.clone()))
        .filter(|value| !value.is_empty());
    Ok(CommandCatalog {
        commands,
        models,
        selected_model,
        selected_reasoning,
        selected_service_tier,
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

fn help(catalog: &CommandCatalog) -> String {
    catalog
        .commands
        .iter()
        .map(|command| format!("{:<20} {}", command.usage, command.description))
        .collect::<Vec<_>>()
        .join("\n")
}

fn model_command(
    session: &phi_core::session::Session,
    policy: &mut phi_steel::Policy,
    catalog: &CommandCatalog,
    requested: &str,
) -> Result<String> {
    let state: serde_json::Value = serde_json::from_str(policy.state())?;
    let current = state["model"]
        .as_str()
        .or_else(|| {
            catalog
                .models
                .iter()
                .find(|model| model.default)
                .map(|model| model.id.as_str())
        })
        .context("provider has no default model")?;
    if requested.is_empty() {
        let available = catalog
            .models
            .iter()
            .map(|model| {
                if model.id == current {
                    format!("• {} ({})", model.id, model.label)
                } else {
                    format!("  {} ({})", model.id, model.label)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(format!("Current model: {current}\n\n{available}"));
    }
    let mut arguments = requested.split_whitespace();
    let requested_model = arguments.next().context("model is required")?;
    let selected = catalog
        .models
        .iter()
        .find(|model| model.id == requested_model)
        .with_context(|| format!("unknown model: {requested_model}"))?;
    let reasoning = arguments.next().unwrap_or(&selected.default_reasoning);
    let service_tier = arguments.next().unwrap_or(&selected.default_service_tier);
    if arguments.next().is_some() {
        bail!("usage: /model MODEL [REASONING] [SERVICE_TIER]");
    }
    if !selected.reasoning.is_empty()
        && !selected
            .reasoning
            .iter()
            .any(|option| option.id() == reasoning)
    {
        bail!("unsupported reasoning for {requested_model}: {reasoning}");
    }
    if !selected.service_tiers.is_empty()
        && !selected
            .service_tiers
            .iter()
            .any(|option| option.id() == service_tier)
    {
        bail!("unsupported service tier for {requested_model}: {service_tier}");
    }
    let event = Event::ModelSelected {
        model: requested_model.into(),
        reasoning: reasoning.into(),
        service_tier: service_tier.into(),
    };
    session.append(&event)?;
    let output = policy.on_event(&event)?;
    session.append(&output)?;
    session.save_state(policy.state())?;
    match output.effects.into_iter().next() {
        Some(Effect::Finish { content }) => Ok(content),
        _ => bail!("model selection did not finish locally"),
    }
}

fn policy_config(
    config: &Config,
    capabilities: &phi_core::capability::Registry,
    session_id: &str,
) -> String {
    let mut tools = capabilities.specs();
    tools.push(phi_core::capability::shell_spec());
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    serde_json::json!({
        "context_char_budget": config.context_char_budget,
        "session_id": session_id,
        "tools": tools,
    })
    .to_string()
}

fn capabilities(sources: &phi_core::session::Sources) -> phi_core::capability::Registry {
    let mut capabilities = phi_core::capability::Registry::default();
    capabilities.register(phi_core::capability::ReadFile);
    capabilities.register(phi_core::capability::ReplaceFile);
    capabilities.register(phi_eval::SubmitPolicyCandidate {
        active_policy: sources.policy.clone(),
        provider: sources.provider.clone(),
        prompt: sources.prompt.clone(),
        compaction: sources.compaction.clone(),
    });
    capabilities
}

pub fn start(options: RunOptions, prompt: String) -> Handle {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let cancellation = CancellationToken::new();
    let run_cancellation = cancellation.clone();
    tokio::task::spawn_local(async move {
        if let Err(error) = run(options, prompt, &event_tx, command_rx, &run_cancellation).await {
            let _ = event_tx.send(RuntimeEvent::Error {
                message: format!("{error:#}"),
            });
        }
    });
    Handle {
        events: event_rx,
        commands: command_tx,
        cancellation,
    }
}

async fn run(
    options: RunOptions,
    prompt: String,
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    mut commands: mpsc::UnboundedReceiver<RuntimeCommand>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let workspace = options.workspace.canonicalize()?;
    let config = load_config(&options.config_path)?;
    let sessions = workspace.join(".phi/sessions");
    let (session, sources, saved_state) = match options.session_id {
        Some(id) => {
            let session = phi_core::session::Session::open(&sessions, &id)?;
            let sources = session.sources(&config.prompt)?;
            let state = session.load_state()?;
            (session, sources, Some(state))
        }
        None => {
            let active =
                phi_core::policy_store::active(&workspace.join(".phi/policies"), &config.policy)?;
            let session = phi_core::session::Session::create(
                &sessions,
                &active,
                &config.provider,
                &config.prompt,
                &config.compaction,
            )?;
            let sources = session.sources(&config.prompt)?;
            (session, sources, None)
        }
    };
    let capabilities = capabilities(&sources);
    send(
        events,
        RuntimeEvent::Session {
            id: session.id().into(),
        },
    )?;
    send(
        events,
        RuntimeEvent::UserMessage {
            content: prompt.clone(),
        },
    )?;
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.policy,
        &sources.provider,
        &sources.prompt,
        &sources.compaction,
        &policy_config(&config, &capabilities, session.id()),
        saved_state,
    )?;
    let mut event = Event::UserMessage { content: prompt };
    let permissions = phi_core::permissions::Permissions {
        allow_shell: options.allow_shell,
        allow_write: options.allow_write,
    };

    for _ in 0..16 {
        if cancellation.is_cancelled() {
            bail!("cancelled");
        }
        session.append(&event)?;
        let output = policy.on_event(&event)?;
        session.append(&output)?;
        session.save_state(policy.state())?;
        send_context(events, policy.state(), config.context_char_budget)?;
        let effect = output
            .effects
            .into_iter()
            .next()
            .context("policy emitted no effect")?;
        match effect {
            Effect::Process {
                program,
                args,
                stdin,
                timeout_ms,
            } => {
                event = cancellable(
                    cancellation,
                    phi_core::process::run(
                        &workspace,
                        &config.allowed_programs,
                        &program,
                        &args,
                        &stdin,
                        timeout_ms,
                    ),
                )
                .await??;
            }
            Effect::RunTool { name, arguments } => {
                send(
                    events,
                    RuntimeEvent::ToolStarted {
                        name: name.clone(),
                        arguments: arguments.clone(),
                    },
                )?;
                let approved = match permissions.authorize_tool(&name) {
                    Ok(()) => true,
                    Err(_) if options.interactive_approvals => {
                        send(
                            events,
                            RuntimeEvent::ApprovalRequested { name: name.clone() },
                        )?;
                        matches!(
                            cancellable(cancellation, commands.recv()).await?,
                            Some(RuntimeCommand::ApproveOnce)
                        )
                    }
                    Err(_) => false,
                };
                let result = if !approved {
                    serde_json::json!({ "error": format!("{name} approval denied") })
                } else if name == "shell" {
                    cancellable(
                        cancellation,
                        run_shell_tool(&workspace, &config.allowed_programs, &arguments),
                    )
                    .await
                    .and_then(|result| result)
                    .and_then(|event| serde_json::to_value(event).map_err(Into::into))
                    .unwrap_or_else(|error| serde_json::json!({ "error": error.to_string() }))
                } else {
                    capabilities
                        .execute(&workspace, &name, arguments)
                        .unwrap_or_else(|error| serde_json::json!({ "error": error.to_string() }))
                };
                send(
                    events,
                    RuntimeEvent::ToolCompleted {
                        name: name.clone(),
                        result: result.clone(),
                    },
                )?;
                event = Event::ToolCompleted { name, result };
            }
            Effect::HttpRequest {
                url,
                secret,
                headers,
                body,
                timeout_ms,
            } => {
                let request = phi_core::http::post_sse(
                    phi_core::http::SseRequest {
                        allowed_origins: &config.allowed_http_origins,
                        secrets: &config.secrets,
                        url: &url,
                        secret_name: &secret,
                        headers: &headers,
                        body,
                        timeout_ms,
                    },
                    |provider_event| {
                        if provider_event
                            .get("type")
                            .and_then(serde_json::Value::as_str)
                            == Some("response.output_text.delta")
                            && let Some(content) = provider_event
                                .get("delta")
                                .and_then(serde_json::Value::as_str)
                        {
                            let _ = events.send(RuntimeEvent::ModelDelta {
                                content: content.into(),
                            });
                        }
                    },
                );
                event = cancellable(cancellation, request).await??;
            }
            Effect::Finish { content } => {
                send(events, RuntimeEvent::Finished { content })?;
                return Ok(());
            }
        }
    }
    bail!("effect limit reached")
}

async fn cancellable<T>(
    cancellation: &CancellationToken,
    future: impl std::future::Future<Output = T>,
) -> Result<T> {
    tokio::select! {
        value = future => Ok(value),
        () = cancellation.cancelled() => bail!("cancelled"),
    }
}

fn send(events: &mpsc::UnboundedSender<RuntimeEvent>, event: RuntimeEvent) -> Result<()> {
    events
        .send(event)
        .map_err(|_| anyhow::anyhow!("frontend disconnected"))
}

fn send_context(
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    state: &str,
    context_char_budget: usize,
) -> Result<()> {
    let state: serde_json::Value = serde_json::from_str(state)?;
    let usage = &state["last_usage"];
    let input_details = &usage["input_tokens_details"];
    send(
        events,
        RuntimeEvent::ContextUpdated {
            estimated_tokens: state["estimated_tokens"].as_f64().unwrap_or_default() as u64,
            token_budget: (context_char_budget / 4) as u64,
            compactions: state["compactions"].as_f64().unwrap_or_default() as u64,
            input_tokens: number_u64(&usage["input_tokens"]),
            cached_tokens: number_u64(&input_details["cached_tokens"]),
            cache_write_tokens: number_u64(&input_details["cache_write_tokens"]),
            output_tokens: number_u64(&usage["output_tokens"]),
        },
    )
}

fn number_u64(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|value| value as u64))
}

fn load_config(path: &Path) -> Result<Config> {
    let root = path.parent().context("config has no parent")?;
    let mut config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    config.policy = root.join(config.policy);
    config.provider = root.join(config.provider);
    config.prompt = root.join(config.prompt);
    config.compaction = root.join(config.compaction);
    Ok(config)
}

async fn run_shell_tool(
    workspace: &Path,
    allowed: &HashSet<String>,
    arguments: &serde_json::Value,
) -> Result<Event> {
    let program = arguments
        .get("program")
        .and_then(serde_json::Value::as_str)
        .context("shell requires program")?;
    let args = arguments
        .get("args")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("shell args must be strings")
        })
        .collect::<Result<Vec<_>>>()?;
    let stdin = arguments
        .get("stdin")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let timeout_ms = arguments
        .get("timeout_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(30_000);
    phi_core::process::run(workspace, allowed, program, &args, stdin, timeout_ms).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> (tempfile::TempDir, RunOptions) {
        let workspace = tempfile::tempdir().unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let config_path = workspace.path().join("phi.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({
                "policy": root.join("policy/agent.scm"),
                "provider": root.join("policy/providers/openai.scm"),
                "prompt": root.join("policy/prompts/simple.scm"),
                "compaction": root.join("policy/compaction/simple.scm"),
                "allowed_programs": [],
                "allowed_http_origins": [],
                "secrets": {},
                "context_char_budget": 24000
            }))
            .unwrap(),
        )
        .unwrap();
        let options = RunOptions {
            workspace: workspace.path().into(),
            config_path,
            session_id: None,
            allow_shell: false,
            allow_write: false,
            interactive_approvals: false,
        };
        (workspace, options)
    }

    #[test]
    fn catalog_combines_core_commands_and_provider_models() {
        let (_workspace, options) = options();
        let catalog = command_catalog(&options).unwrap();
        assert!(
            catalog
                .commands
                .iter()
                .any(|command| command.name == "model")
        );
        assert_eq!(catalog.models[0].id, "gpt-5.6-luna");
    }

    #[test]
    fn model_command_creates_session_and_persists_selection() {
        let (_workspace, mut options) = options();
        let execution = execute_command(
            &options,
            &CommandInvocation {
                name: "model".into(),
                arguments: "gpt-5.6-terra high fast".into(),
            },
        )
        .unwrap();
        options.session_id = Some(execution.session_id.clone());
        let state = phi_core::session::Session::open(
            &options.workspace.join(".phi/sessions"),
            &execution.session_id,
        )
        .unwrap()
        .load_state()
        .unwrap();
        let state: serde_json::Value = serde_json::from_str(&state).unwrap();
        assert_eq!(state["model"], "gpt-5.6-terra");
        assert_eq!(state["reasoning"], "high");
        assert_eq!(state["service_tier"], "fast");

        let session = phi_core::session::Session::open(
            &options.workspace.join(".phi/sessions"),
            &execution.session_id,
        )
        .unwrap();
        let config = load_config(&options.config_path).unwrap();
        let sources = session.sources(&config.prompt).unwrap();
        let capabilities = capabilities(&sources);
        let mut policy = phi_steel::Policy::load_with_state(
            &sources.policy,
            &sources.provider,
            &sources.prompt,
            &sources.compaction,
            &policy_config(&config, &capabilities, session.id()),
            Some(session.load_state().unwrap()),
        )
        .unwrap();
        let output = policy
            .on_event(&Event::UserMessage {
                content: "hello".into(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, headers, .. }
                if body["model"] == "gpt-5.6-terra"
                    && body["reasoning"]["effort"] == "high"
                    && body["service_tier"] == "priority"
                    && body["prompt_cache_key"] == execution.session_id
                    && headers["session_id"] == execution.session_id
                    && body["tools"].as_array().unwrap().len() == 4
                    && body["tools"][0]["name"] == "read_file"
                    && body["instructions"]
                        .as_str().unwrap().contains("Answer ordinary requests")
        ));
    }

    #[test]
    fn reads_provider_reported_token_counts() {
        assert_eq!(number_u64(&serde_json::json!(378.0)), Some(378));
        assert_eq!(number_u64(&serde_json::Value::Null), None);
    }
}
