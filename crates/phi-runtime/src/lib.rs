use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use phi_protocol::{Effect, Event, StreamRule};
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
    ActivityChanged {
        activity: String,
    },
    ToolRouteSelected {
        capability: String,
        implementation: String,
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
    allowed_programs: HashSet<String>,
    allowed_http_origins: HashSet<String>,
    secrets: BTreeMap<String, phi_core::http::SecretConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct UserState {
    model: Option<ModelSelection>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ModelSelection {
    provider: String,
    id: String,
    reasoning: String,
    service_tier: String,
}

const DEFAULT_CONFIG: &str = r#"{
  "allowed_programs": ["cargo", "find", "git", "ls", "pwd", "rg", "sed"],
  "allowed_http_origins": [
    "https://auth.openai.com",
    "https://chatgpt.com",
    "https://openrouter.ai"
  ],
  "secrets": {
    "openai_chatgpt": {
      "path": "~/.codex/auth.json",
      "bearer_pointer": "/tokens/access_token",
      "headers": { "chatgpt-account-id": "/tokens/account_id" },
      "refresh": {
        "url": "https://auth.openai.com/oauth/token",
        "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
        "refresh_pointer": "/tokens/refresh_token"
      }
    },
    "openrouter": {
      "path": "~/.phi/secrets/openrouter.json",
      "bearer_pointer": "/api_key"
    }
  }
}"#;

const DEFAULT_MAIN: &str = include_str!("../../../main.scm");
const DEFAULT_AGENT: &str = include_str!("../../../policy/agent.scm");
const RESPONSES_PLUGIN: &str = include_str!("../../../policy/providers/responses.scm");
const OPENAI_PLUGIN: &str = include_str!("../../../policy/providers/openai.scm");
const OPENROUTER_PLUGIN: &str = include_str!("../../../policy/providers/openrouter.scm");
const OPENAI_WEB_SEARCH_PLUGIN: &str = include_str!("../../../policy/tools/openai-web-search.scm");
const OPENROUTER_WEB_SEARCH_PLUGIN: &str =
    include_str!("../../../policy/tools/openrouter-web-search.scm");
const PROMPT_PLUGIN: &str = include_str!("../../../policy/prompts/simple.scm");
const COMPACTION_PLUGIN: &str = include_str!("../../../policy/compaction/simple.scm");

pub fn initialize_home() -> Result<phi_core::home::PhiHome> {
    let home = phi_core::home::PhiHome::discover()?;
    initialize_at(&home)?;
    Ok(home)
}

pub fn initialize_at(home: &phi_core::home::PhiHome) -> Result<()> {
    std::fs::create_dir_all(&home.root)?;
    write_if_missing(&home.config(), DEFAULT_CONFIG)?;
    write_if_missing(&home.main(), DEFAULT_MAIN)?;
    write_if_missing(
        &home.state(),
        r#"{
  "model": {
    "provider": "openai",
    "id": "openai/gpt-5.6-luna",
    "reasoning": "low",
    "service_tier": "default"
  }
}
"#,
    )?;
    write_if_missing(&home.plugin_lock(), "{\n  \"plugins\": []\n}\n")?;
    let builtins = home.builtins();
    write_bundled(&builtins.join("agent.scm"), DEFAULT_AGENT)?;
    write_builtin(&builtins, "responses", RESPONSES_PLUGIN)?;
    write_builtin(&builtins, "openai", OPENAI_PLUGIN)?;
    write_builtin(&builtins, "openrouter", OPENROUTER_PLUGIN)?;
    write_builtin(&builtins, "openai-web-search", OPENAI_WEB_SEARCH_PLUGIN)?;
    write_builtin(
        &builtins,
        "openrouter-web-search",
        OPENROUTER_WEB_SEARCH_PLUGIN,
    )?;
    write_builtin(&builtins, "simple-prompt", PROMPT_PLUGIN)?;
    write_builtin(&builtins, "simple-compaction", COMPACTION_PLUGIN)?;
    Ok(())
}

fn write_builtin(root: &Path, name: &str, source: &str) -> Result<()> {
    let plugin = root.join("plugins").join(name);
    write_bundled(
        &plugin.join("plugin.json"),
        &serde_json::to_string_pretty(&serde_json::json!({
            "name": name,
            "version": env!("CARGO_PKG_VERSION"),
            "entrypoint": "main.scm"
        }))?,
    )?;
    write_bundled(&plugin.join("main.scm"), source)
}

fn write_bundled(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

fn write_if_missing(path: &Path, content: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

fn home_for_config(path: &Path) -> Result<phi_core::home::PhiHome> {
    Ok(phi_core::home::PhiHome {
        root: path.parent().context("config has no parent")?.to_owned(),
    })
}

fn resolve_sources(
    home: &phi_core::home::PhiHome,
    workspace: &Path,
) -> Result<phi_core::session::ComposedSources> {
    let main = home.main();
    let lock = phi_core::plugin::read_lock(home)?;
    let mut plugins = Vec::new();
    for name in phi_steel::composition_plugins(&main)? {
        let (root, manifest) =
            if let Some(locked) = lock.plugins.iter().find(|item| item.name == name) {
                let root = phi_core::plugin::install_root(home, &name, &locked.commit);
                let manifest = phi_core::plugin::read_manifest(&root)?;
                (root, manifest)
            } else {
                let root = home.builtins().join("plugins").join(&name);
                if !root.is_dir() {
                    bail!("plugin is not installed: {name}");
                }
                let manifest = phi_core::plugin::read_manifest(&root)?;
                (root, manifest)
            };
        if manifest.name != name {
            bail!("plugin manifest name does not match composition: {name}");
        }
        let entrypoint = root.join(&manifest.entrypoint);
        if !entrypoint.is_file() {
            bail!("plugin entrypoint is missing: {name}");
        }
        plugins.push(phi_core::session::PluginSource {
            name,
            root,
            entrypoint,
        });
    }
    let fallback = home.builtins().join("agent.scm");
    let policy = phi_core::policy_store::active(&workspace.join(".phi/policies"), &fallback)?;
    Ok(phi_core::session::ComposedSources {
        policy,
        main,
        plugins,
    })
}

fn entrypoints(sources: &phi_core::session::ComposedSources) -> Vec<PathBuf> {
    sources
        .plugins
        .iter()
        .map(|plugin| plugin.entrypoint.clone())
        .collect()
}

pub fn check_policy(home: &phi_core::home::PhiHome, workspace: &Path) -> Result<()> {
    let sources = resolve_sources(home, workspace)?;
    phi_steel::check(&sources.policy, &entrypoints(&sources), &sources.main)
}

pub fn check_policy_candidate(
    home: &phi_core::home::PhiHome,
    workspace: &Path,
    candidate: &Path,
) -> Result<()> {
    let sources = resolve_sources(home, workspace)?;
    let plugins = entrypoints(&sources);
    phi_steel::check(candidate, &plugins, &sources.main)?;
    phi_steel::replay_smoke(candidate, &plugins, &sources.main)
}

fn load_user_state(home: &phi_core::home::PhiHome) -> Result<UserState> {
    if !home.state().is_file() {
        return Ok(UserState::default());
    }
    serde_json::from_slice(&std::fs::read(home.state())?).context("read user state")
}

fn save_user_state(home: &phi_core::home::PhiHome, state: &UserState) -> Result<()> {
    std::fs::write(home.state(), serde_json::to_vec_pretty(state)?)?;
    Ok(())
}

pub fn command_catalog(options: &RunOptions) -> Result<CommandCatalog> {
    let workspace = options.workspace.canonicalize()?;
    let config = load_config(&options.config_path)?;
    let home = home_for_config(&options.config_path)?;
    let (sources, state) = match &options.session_id {
        Some(id) => {
            let session = phi_core::session::Session::open(&workspace.join(".phi/sessions"), id)?;
            let sources = session
                .composed_sources()?
                .context("session has no composition snapshot")?;
            (sources, Some(session.load_state()?))
        }
        None => (resolve_sources(&home, &workspace)?, None),
    };
    let capabilities = capabilities(&sources);
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.policy,
        &entrypoints(&sources),
        &sources.main,
        &policy_config(
            &config,
            &capabilities,
            options.session_id.as_deref().unwrap_or("catalog"),
            &load_user_state(&home)?,
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
    let home = home_for_config(&options.config_path)?;
    let sessions = workspace.join(".phi/sessions");
    let (session, sources, saved_state) = match &options.session_id {
        Some(id) => {
            let session = phi_core::session::Session::open(&sessions, id)?;
            let sources = session
                .composed_sources()?
                .context("session has no composition snapshot")?;
            let state = session.load_state()?;
            (session, sources, Some(state))
        }
        None => {
            let current = resolve_sources(&home, &workspace)?;
            let session = phi_core::session::Session::create_composed(
                &sessions,
                &current.policy,
                &current.main,
                &current.plugins,
            )?;
            let sources = session
                .composed_sources()?
                .context("missing composition snapshot")?;
            (session, sources, None)
        }
    };
    let capabilities = capabilities(&sources);
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.policy,
        &entrypoints(&sources),
        &sources.main,
        &policy_config(
            &config,
            &capabilities,
            session.id(),
            &load_user_state(&home)?,
        ),
        saved_state,
    )?;
    let initial_catalog = catalog(&mut policy)?;
    let content = match invocation.name.as_str() {
        "help" => help(&initial_catalog),
        "model" => model_command(
            &home,
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
            usage: "/model".into(),
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

    let mut models = policy.models()?;
    for model in &mut models {
        if model.model.is_empty() {
            model.model = model.id.clone();
        }
    }
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
    let state: serde_json::Value = serde_json::from_str(policy.state())?;
    let selected_model = state["model"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
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
    home: &phi_core::home::PhiHome,
    session: &phi_core::session::Session,
    policy: &mut phi_steel::Policy,
    catalog: &CommandCatalog,
    requested: &str,
) -> Result<String> {
    let state: serde_json::Value = serde_json::from_str(policy.state())?;
    let current = state["model"]
        .as_str()
        .filter(|value| !value.is_empty())
        .context("no model selected; use /model MODEL")?;
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
    save_user_state(
        home,
        &UserState {
            model: Some(ModelSelection {
                provider: selected.provider.clone(),
                id: selected.id.clone(),
                reasoning: reasoning.into(),
                service_tier: service_tier.into(),
            }),
        },
    )?;
    match output.effects.into_iter().next() {
        Some(Effect::Finish { content }) => Ok(content),
        _ => bail!("model selection did not finish locally"),
    }
}

fn policy_config(
    _config: &Config,
    capabilities: &phi_core::capability::Registry,
    session_id: &str,
    user_state: &UserState,
) -> String {
    let mut tools = capabilities.specs();
    tools.push(phi_core::capability::shell_spec());
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let mut value = serde_json::json!({
        "session_id": session_id,
        "tools": tools,
    });
    if let Some(model) = &user_state.model {
        value["model"] = model.id.clone().into();
        value["reasoning"] = model.reasoning.clone().into();
        value["service_tier"] = model.service_tier.clone().into();
    }
    value.to_string()
}

fn capabilities(sources: &phi_core::session::ComposedSources) -> phi_core::capability::Registry {
    let mut capabilities = phi_core::capability::Registry::default();
    capabilities.register(phi_core::capability::ReadFile);
    capabilities.register(phi_core::capability::ReplaceFile);
    capabilities.register(phi_eval::SubmitPolicyCandidate {
        active_policy: sources.policy.clone(),
        main: sources.main.clone(),
        plugins: entrypoints(sources),
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
    let home = home_for_config(&options.config_path)?;
    let sessions = workspace.join(".phi/sessions");
    let (session, sources, saved_state) = match options.session_id {
        Some(id) => {
            let session = phi_core::session::Session::open(&sessions, &id)?;
            let sources = session
                .composed_sources()?
                .context("session has no composition snapshot")?;
            let state = session.load_state()?;
            (session, sources, Some(state))
        }
        None => {
            let current = resolve_sources(&home, &workspace)?;
            let session = phi_core::session::Session::create_composed(
                &sessions,
                &current.policy,
                &current.main,
                &current.plugins,
            )?;
            let sources = session
                .composed_sources()?
                .context("missing composition snapshot")?;
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
        &entrypoints(&sources),
        &sources.main,
        &policy_config(
            &config,
            &capabilities,
            session.id(),
            &load_user_state(&home)?,
        ),
        saved_state,
    )?;
    let selected_model = state_string(policy.state(), "model")?;
    let tool_routes = policy.resolved_tool_routes(&selected_model)?;
    let tool_capabilities = tool_routes
        .iter()
        .map(|route| (route.implementation.clone(), route.capability.clone()))
        .collect::<BTreeMap<_, _>>();
    for route in tool_routes {
        send(
            events,
            RuntimeEvent::ToolRouteSelected {
                capability: route.capability,
                implementation: route.implementation,
            },
        )?;
    }
    let mut event = Event::UserMessage { content: prompt };
    let mut activity = "ready".to_owned();
    let mut callable_tool = None;
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
        send_context(events, policy.state())?;
        let next_activity = state_activity(policy.state())?;
        if next_activity != activity {
            if activity == "searching"
                && let Some(name) = callable_tool.take()
            {
                send(
                    events,
                    RuntimeEvent::ToolCompleted {
                        name,
                        result: serde_json::json!({}),
                    },
                )?;
            }
            if next_activity == "searching" {
                let implementation = state_string(policy.state(), "pending_tool")?;
                let name = tool_capabilities
                    .get(&implementation)
                    .cloned()
                    .unwrap_or(implementation);
                send(
                    events,
                    RuntimeEvent::ToolStarted {
                        name: name.clone(),
                        arguments: serde_json::json!({}),
                    },
                )?;
                callable_tool = Some(name);
            }
            send(
                events,
                RuntimeEvent::ActivityChanged {
                    activity: next_activity.clone(),
                },
            )?;
            activity = next_activity;
        }
        let stream_output = activity == "working";
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
                stream,
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
                        if stream_output {
                            emit_stream_events(events, provider_event, &stream);
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

fn emit_stream_events(
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    provider_event: &serde_json::Value,
    rules: &[StreamRule],
) {
    for rule in rules {
        if !rule
            .matches
            .iter()
            .all(|(pointer, expected)| provider_event.pointer(pointer) == Some(expected))
        {
            continue;
        }
        let value = provider_event
            .pointer(&rule.value)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let event = match rule.emit.as_str() {
            "model_delta" => value.as_str().map(|content| RuntimeEvent::ModelDelta {
                content: content.into(),
            }),
            "tool_started" => Some(RuntimeEvent::ToolStarted {
                name: rule.name.clone(),
                arguments: value,
            }),
            "tool_completed" => Some(RuntimeEvent::ToolCompleted {
                name: rule.name.clone(),
                result: value,
            }),
            _ => None,
        };
        if let Some(event) = event {
            let _ = events.send(event);
        }
    }
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

fn send_context(events: &mpsc::UnboundedSender<RuntimeEvent>, state: &str) -> Result<()> {
    let state: serde_json::Value = serde_json::from_str(state)?;
    let usage = &state["last_usage"];
    let input_details = &usage["input_tokens_details"];
    send(
        events,
        RuntimeEvent::ContextUpdated {
            estimated_tokens: state["estimated_tokens"].as_f64().unwrap_or_default() as u64,
            token_budget: state["context_window"].as_u64().unwrap_or_default(),
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

fn state_activity(state: &str) -> Result<String> {
    state_string(state, "activity")
}

fn state_string(state: &str, key: &str) -> Result<String> {
    let state: serde_json::Value = serde_json::from_str(state)?;
    Ok(state[key]
        .as_str()
        .with_context(|| format!("state has no {key}"))?
        .into())
}

fn load_config(path: &Path) -> Result<Config> {
    serde_json::from_slice(&std::fs::read(path)?).context("read config")
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
        let home = phi_core::home::PhiHome {
            root: workspace.path().join("home"),
        };
        initialize_at(&home).unwrap();
        let options = RunOptions {
            workspace: workspace.path().into(),
            config_path: home.config(),
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
        assert_eq!(catalog.models[0].id, "openai/gpt-5.6-luna");
    }

    #[test]
    fn provider_stream_rules_emit_generic_tool_events() {
        let rules: Vec<StreamRule> = serde_json::from_value(serde_json::json!([
            {
                "match": { "/type": "response.output_item.added", "/item/type": "web_search_call" },
                "emit": "tool_started",
                "name": "web_search",
                "value": "/item"
            },
            {
                "match": { "/type": "response.output_item.done", "/item/type": "web_search_call" },
                "emit": "tool_completed",
                "name": "web_search",
                "value": "/item"
            }
        ]))
        .unwrap();
        let (events, mut received) = mpsc::unbounded_channel();

        emit_stream_events(
            &events,
            &serde_json::json!({
                "type": "response.output_item.added",
                "item": { "type": "web_search_call" }
            }),
            &rules,
        );
        emit_stream_events(
            &events,
            &serde_json::json!({
                "type": "response.output_item.done",
                "item": { "type": "web_search_call", "action": { "sources": [] } }
            }),
            &rules,
        );

        assert!(matches!(
            received.try_recv().unwrap(),
            RuntimeEvent::ToolStarted { name, .. } if name == "web_search"
        ));
        assert!(matches!(
            received.try_recv().unwrap(),
            RuntimeEvent::ToolCompleted { name, .. } if name == "web_search"
        ));
    }

    #[test]
    fn model_command_creates_session_and_persists_selection() {
        let (_workspace, mut options) = options();
        let execution = execute_command(
            &options,
            &CommandInvocation {
                name: "model".into(),
                arguments: "openai/gpt-5.6-terra high fast".into(),
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
        assert_eq!(state["model"], "openai/gpt-5.6-terra");
        assert_eq!(state["reasoning"], "high");
        assert_eq!(state["service_tier"], "fast");
        assert!(state["messages"].as_array().unwrap().is_empty());
        let global: UserState = serde_json::from_slice(
            &std::fs::read(home_for_config(&options.config_path).unwrap().state()).unwrap(),
        )
        .unwrap();
        assert_eq!(global.model.unwrap().id, "openai/gpt-5.6-terra");

        let session = phi_core::session::Session::open(
            &options.workspace.join(".phi/sessions"),
            &execution.session_id,
        )
        .unwrap();
        let config = load_config(&options.config_path).unwrap();
        let sources = session.composed_sources().unwrap().unwrap();
        let capabilities = capabilities(&sources);
        let plugins = entrypoints(&sources);
        let mut policy = phi_steel::Policy::load_with_state(
            &sources.policy,
            &plugins,
            &sources.main,
            &policy_config(&config, &capabilities, session.id(), &UserState::default()),
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
                    && body["tools"].as_array().unwrap().len() == 5
                    && body["tools"][0]["name"] == "read_file"
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["type"] == "web_search")
                    && body["instructions"]
                        .as_str().unwrap().contains("Answer ordinary requests")
        ));
    }

    #[test]
    fn reads_provider_reported_token_counts() {
        assert_eq!(number_u64(&serde_json::json!(378.0)), Some(378));
        assert_eq!(number_u64(&serde_json::Value::Null), None);
    }

    #[test]
    fn refreshes_versioned_builtins_without_overwriting_user_config() {
        let root = tempfile::tempdir().unwrap();
        let home = phi_core::home::PhiHome {
            root: root.path().join("home"),
        };
        initialize_at(&home).unwrap();
        std::fs::write(home.builtins().join("agent.scm"), "stale").unwrap();
        std::fs::write(home.main(), "user composition").unwrap();
        initialize_at(&home).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.builtins().join("agent.scm")).unwrap(),
            DEFAULT_AGENT
        );
        assert_eq!(
            std::fs::read_to_string(home.main()).unwrap(),
            "user composition"
        );
    }
}
