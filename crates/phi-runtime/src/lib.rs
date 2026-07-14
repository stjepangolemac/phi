use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use phi_protocol::{Effect, Event, StreamRule, ToolCall, ToolExecution, ToolResult};
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use phi_core::process::ShellSessions as ProcessManager;
pub use phi_protocol::{CommandInvocation, CommandSpec, ModelSpec, PickerOptionSpec};

#[derive(Clone)]
pub struct RunOptions {
    pub workspace: PathBuf,
    pub config_path: PathBuf,
    pub session_id: Option<String>,
    pub allow_shell: bool,
    pub allow_write: bool,
    pub interactive_approvals: bool,
    pub processes: Arc<phi_core::process::ShellSessions>,
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
        call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolOutput {
        call_id: String,
        name: String,
        content: String,
    },
    ToolCompleted {
        call_id: String,
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
    pub role: String,
    pub catalog: CommandCatalog,
}

impl Handle {
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
}

#[derive(Clone, Deserialize)]
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
const SKILLS_PLUGIN: &str = include_str!("../../../policy/tools/skills.scm");
const CODEX_PATCH_PLUGIN: &str = include_str!("../../../policy/tools/codex-patch.scm");
const PROMPT_PLUGIN: &str = include_str!("../../../policy/prompts/simple.scm");
const COMPACTION_PLUGIN: &str = include_str!("../../../policy/compaction/simple.scm");

pub fn initialize_home() -> Result<phi_core::home::PhiHome> {
    let home = phi_core::home::PhiHome::discover()?;
    initialize_at(&home)?;
    Ok(home)
}

pub fn initialize_at(home: &phi_core::home::PhiHome) -> Result<()> {
    std::fs::create_dir_all(&home.root)?;
    std::fs::create_dir_all(home.skills())?;
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
    write_builtin(&builtins, "skills", SKILLS_PLUGIN)?;
    write_builtin(&builtins, "codex-patch", CODEX_PATCH_PLUGIN)?;
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
    let _config = load_config(&options.config_path)?;
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
    let capabilities = capabilities(&sources, &home);
    let skills = phi_core::skill::discover(&home.skills(), &workspace)?;
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.policy,
        &entrypoints(&sources),
        &sources.main,
        &policy_config(
            &capabilities,
            options.session_id.as_deref().unwrap_or("catalog"),
            &load_user_state(&home)?,
            &skills,
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
    let _config = load_config(&options.config_path)?;
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
    let capabilities = capabilities(&sources, &home);
    let skills = phi_core::skill::discover(&home.skills(), &workspace)?;
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.policy,
        &entrypoints(&sources),
        &sources.main,
        &policy_config(
            &capabilities,
            session.id(),
            &load_user_state(&home)?,
            &skills,
        ),
        saved_state,
    )?;
    let initial_catalog = catalog(&mut policy)?;
    let role = if invocation.name == "ps" {
        "processes"
    } else {
        "note"
    };
    let content = match invocation.name.as_str() {
        "help" => help(&initial_catalog),
        "ps" => process_list(&options.processes, &workspace)?,
        "stop" => {
            if !invocation.arguments.is_empty() {
                bail!("usage: /stop");
            }
            let stopped = options.processes.stop_all();
            format!(
                "Stopped {stopped} background process{}.",
                if stopped == 1 { "" } else { "es" }
            )
        }
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
            policy.run_command(name, &invocation.arguments)?
        }
    };
    session.save_state(policy.state())?;
    Ok(CommandExecution {
        session_id: session.id().into(),
        content,
        role: role.into(),
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
        CommandSpec {
            name: "ps".into(),
            usage: "/ps".into(),
            description: "Show managed background processes.".into(),
            source: "core".into(),
        },
        CommandSpec {
            name: "stop".into(),
            usage: "/stop".into(),
            description: "Stop all background processes.".into(),
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

fn process_list(processes: &phi_core::process::ShellSessions, workspace: &Path) -> Result<String> {
    let processes = processes.list()?;
    if processes.is_empty() {
        return Ok("## Background processes\n\nNo background processes.".into());
    }
    let items = processes
        .into_iter()
        .map(|process| {
            let workdir = Path::new(&process.workdir)
                .strip_prefix(workspace)
                .unwrap_or_else(|_| Path::new(&process.workdir));
            let workdir = if workdir.as_os_str().is_empty() {
                ".".into()
            } else {
                workdir.display().to_string()
            };
            let status = if process.status == "running" {
                format!("Running for {}", format_elapsed(process.elapsed_ms))
            } else if let Some(code) = process.exit_code {
                format!("Finished · exit {code}")
            } else {
                "Finished".into()
            };
            let workdir = if workdir == "." {
                String::new()
            } else {
                format!(" · `{workdir}`")
            };
            format!(
                "• **{status}**{workdir}\n\n```bash\n{}\n```",
                process.command
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(format!("## Background processes\n\n{items}"))
}

fn format_elapsed(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
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
    capabilities: &phi_core::capability::Registry,
    session_id: &str,
    user_state: &UserState,
    skills: &[phi_core::skill::SkillSpec],
) -> String {
    let mut tools = capabilities.specs();
    tools.push(phi_core::capability::exec_command_spec());
    tools.push(phi_core::capability::list_processes_spec());
    tools.push(phi_core::capability::terminate_process_spec());
    tools.push(phi_core::capability::write_stdin_spec());
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let mut value = serde_json::json!({
        "session_id": session_id,
        "skills": skills,
        "tools": tools,
    });
    if let Some(model) = &user_state.model {
        value["model"] = model.id.clone().into();
        value["reasoning"] = model.reasoning.clone().into();
        value["service_tier"] = model.service_tier.clone().into();
    }
    value.to_string()
}

fn capabilities(
    sources: &phi_core::session::ComposedSources,
    home: &phi_core::home::PhiHome,
) -> phi_core::capability::Registry {
    let mut capabilities = phi_core::capability::Registry::default();
    capabilities.register(phi_core::capability::ReadFile);
    capabilities.register_hidden(phi_core::skill::LoadSkill {
        personal_root: home.skills(),
    });
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
    let capabilities = Arc::new(capabilities(&sources, &home));
    let skills = phi_core::skill::discover(&home.skills(), &workspace)?;
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
            &capabilities,
            session.id(),
            &load_user_state(&home)?,
            &skills,
        ),
        saved_state,
    )?;
    let file_editor_tool = policy.file_editor_tool_name()?;
    let selected_model = state_string(policy.state(), "model")?;
    let tool_routes = policy.resolved_tool_routes(&selected_model)?;
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
    let permissions = phi_core::permissions::Permissions {
        allow_shell: options.allow_shell,
        allow_write: options.allow_write,
    };
    let shell_sessions = Arc::clone(&options.processes);

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
            Effect::RunTools { calls } => {
                let executor = ToolBatchExecutor {
                    workspace: &workspace,
                    capabilities: Arc::clone(&capabilities),
                    config: Arc::new(config.clone()),
                    file_editor_tool: &file_editor_tool,
                    events,
                    cancellation,
                };
                let results = execute_tool_calls(
                    calls,
                    &executor,
                    &permissions,
                    options.interactive_approvals,
                    &mut commands,
                    &mut policy,
                    &shell_sessions,
                )
                .await?;
                event = Event::ToolsCompleted { results };
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
                            emit_stream_events(events, provider_event, &stream)
                        } else {
                            false
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

struct PendingToolCall {
    index: usize,
    call: ToolCall,
}

struct RawToolResult {
    index: usize,
    call_id: String,
    name: String,
    result: RawToolOutput,
}

enum RawToolOutput {
    Value {
        result: serde_json::Value,
        display: Option<serde_json::Value>,
    },
    Http {
        implementation: String,
        event: Event,
    },
}

struct ToolBatchExecutor<'a> {
    workspace: &'a Path,
    capabilities: Arc<phi_core::capability::Registry>,
    config: Arc<Config>,
    file_editor_tool: &'a str,
    events: &'a mpsc::UnboundedSender<RuntimeEvent>,
    cancellation: &'a CancellationToken,
}

async fn execute_tool_calls(
    calls: Vec<ToolCall>,
    executor: &ToolBatchExecutor<'_>,
    permissions: &phi_core::permissions::Permissions,
    interactive_approvals: bool,
    commands: &mut mpsc::UnboundedReceiver<RuntimeCommand>,
    policy: &mut phi_steel::Policy,
    shell_sessions: &Arc<phi_core::process::ShellSessions>,
) -> Result<Vec<ToolResult>> {
    let mut completed = Vec::new();
    let mut parallel = Vec::new();
    for (index, call) in calls.into_iter().enumerate() {
        let parallel_safe =
            tool_call_parallel_safe(&call, &executor.capabilities, executor.file_editor_tool);
        if !parallel_safe {
            flush_parallel_calls(
                executor,
                &mut parallel,
                policy,
                &mut completed,
                shell_sessions,
            )
            .await?;
        }
        send(
            executor.events,
            RuntimeEvent::ToolStarted {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
        )?;
        let approved = match permissions.authorize_tool(&call.name) {
            Ok(()) => true,
            Err(_) if interactive_approvals => {
                send(
                    executor.events,
                    RuntimeEvent::ApprovalRequested {
                        name: call.name.clone(),
                    },
                )?;
                matches!(
                    cancellable(executor.cancellation, commands.recv()).await?,
                    Some(RuntimeCommand::ApproveOnce)
                )
            }
            Err(_) => false,
        };
        if approved {
            let pending = PendingToolCall { index, call };
            if parallel_safe {
                parallel.push(pending);
            } else {
                let raw = execute_serial_call(executor, pending, policy, shell_sessions).await;
                finish_tool_call(raw, policy, executor.events, &mut completed)?;
            }
        } else {
            let result = serde_json::json!({
                "error": format!("{} approval denied", call.name)
            });
            send(
                executor.events,
                RuntimeEvent::ToolCompleted {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    result: result.clone(),
                },
            )?;
            completed.push((
                index,
                ToolResult {
                    call_id: call.call_id,
                    name: call.name,
                    result,
                },
            ));
        }
    }
    flush_parallel_calls(
        executor,
        &mut parallel,
        policy,
        &mut completed,
        shell_sessions,
    )
    .await?;
    completed.sort_by_key(|(index, _)| *index);
    Ok(completed.into_iter().map(|(_, result)| result).collect())
}

fn tool_call_parallel_safe(
    call: &ToolCall,
    capabilities: &phi_core::capability::Registry,
    file_editor_tool: &str,
) -> bool {
    match &call.execution {
        ToolExecution::Http { parallel, .. } => *parallel,
        ToolExecution::Direct => {
            call.name == "exec_command"
                || (call.name != "write_stdin"
                    && call.name != "list_processes"
                    && call.name != file_editor_tool
                    && capabilities.parallel_safe(&call.name))
        }
    }
}

async fn flush_parallel_calls(
    executor: &ToolBatchExecutor<'_>,
    calls: &mut Vec<PendingToolCall>,
    policy: &mut phi_steel::Policy,
    completed: &mut Vec<(usize, ToolResult)>,
    shell_sessions: &Arc<phi_core::process::ShellSessions>,
) -> Result<()> {
    let mut tasks = tokio::task::JoinSet::new();
    for call in calls.drain(..) {
        let workspace = executor.workspace.to_owned();
        let capabilities = Arc::clone(&executor.capabilities);
        let config = Arc::clone(&executor.config);
        let shell_sessions = Arc::clone(shell_sessions);
        let events = executor.events.clone();
        tasks.spawn_local(async move {
            execute_parallel_call(
                call,
                workspace,
                capabilities,
                config,
                shell_sessions,
                events,
            )
            .await
        });
    }
    while !tasks.is_empty() {
        let raw = cancellable(executor.cancellation, tasks.join_next())
            .await?
            .context("parallel tool task missing")?
            .context("parallel tool task failed")?;
        finish_tool_call(raw, policy, executor.events, completed)?;
    }
    Ok(())
}

async fn execute_parallel_call(
    pending: PendingToolCall,
    workspace: PathBuf,
    capabilities: Arc<phi_core::capability::Registry>,
    config: Arc<Config>,
    shell_sessions: Arc<phi_core::process::ShellSessions>,
    events: mpsc::UnboundedSender<RuntimeEvent>,
) -> RawToolResult {
    let PendingToolCall { index, call } = pending;
    let ToolCall {
        call_id,
        name,
        arguments,
        execution,
    } = call;
    if name == "exec_command" {
        let event_name = name.clone();
        let event_call_id = call_id.clone();
        let result = shell_sessions
            .exec(&workspace, &arguments, move |content| {
                let _ = events.send(RuntimeEvent::ToolOutput {
                    call_id: event_call_id.clone(),
                    name: event_name.clone(),
                    content: content.to_owned(),
                });
            })
            .await
            .unwrap_or_else(|error| serde_json::json!({ "error": error.to_string() }));
        return RawToolResult {
            index,
            call_id,
            name,
            result: RawToolOutput::Value {
                result,
                display: None,
            },
        };
    }
    let result = match execution {
        ToolExecution::Direct => {
            let tool_name = name.clone();
            let result = tokio::task::spawn_blocking(move || {
                capabilities.execute(&workspace, &tool_name, arguments)
            })
            .await
            .map_err(|error| anyhow::anyhow!(error))
            .and_then(|result| result)
            .unwrap_or_else(|error| serde_json::json!({ "error": error.to_string() }));
            RawToolOutput::Value {
                result,
                display: None,
            }
        }
        ToolExecution::Http {
            implementation,
            url,
            secret,
            headers,
            body,
            timeout_ms,
            ..
        } => {
            let event = phi_core::http::post_sse(
                phi_core::http::SseRequest {
                    allowed_origins: &config.allowed_http_origins,
                    secrets: &config.secrets,
                    url: &url,
                    secret_name: &secret,
                    headers: &headers,
                    body,
                    timeout_ms,
                },
                |_| false,
            )
            .await
            .unwrap_or_else(|error| Event::HttpCompleted {
                success: false,
                status: 0,
                events: Vec::new(),
                error: error.to_string(),
            });
            RawToolOutput::Http {
                implementation,
                event,
            }
        }
    };
    RawToolResult {
        index,
        call_id,
        name,
        result,
    }
}

async fn execute_serial_call(
    executor: &ToolBatchExecutor<'_>,
    pending: PendingToolCall,
    policy: &mut phi_steel::Policy,
    shell_sessions: &Arc<phi_core::process::ShellSessions>,
) -> RawToolResult {
    let index = pending.index;
    let call = pending.call;
    if matches!(
        call.name.as_str(),
        "exec_command" | "write_stdin" | "list_processes" | "terminate_process"
    ) {
        let name = call.name.clone();
        let call_id = call.call_id.clone();
        let events = executor.events.clone();
        let result = cancellable(executor.cancellation, async {
            let emit = move |content: &str| {
                let _ = events.send(RuntimeEvent::ToolOutput {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    content: content.to_owned(),
                });
            };
            if call.name == "exec_command" {
                shell_sessions
                    .exec(executor.workspace, &call.arguments, emit)
                    .await
            } else if call.name == "write_stdin" {
                shell_sessions.write_stdin(&call.arguments, emit).await
            } else if call.name == "terminate_process" {
                shell_sessions.terminate(&call.arguments).await
            } else {
                shell_sessions
                    .list()
                    .map(|processes| serde_json::json!({ "processes": processes }))
            }
        })
        .await
        .and_then(|result| result)
        .unwrap_or_else(|error| serde_json::json!({ "error": error.to_string() }));
        return RawToolResult {
            index,
            call_id: call.call_id,
            name: call.name,
            result: RawToolOutput::Value {
                result,
                display: None,
            },
        };
    }
    if call.name == executor.file_editor_tool {
        let (result, display) =
            execute_file_edit(executor.workspace, policy, &call.name, &call.arguments)
                .map(|(result, display)| (result, Some(display)))
                .unwrap_or_else(|error| (serde_json::json!({ "error": error.to_string() }), None));
        return RawToolResult {
            index,
            call_id: call.call_id,
            name: call.name,
            result: RawToolOutput::Value { result, display },
        };
    }
    execute_parallel_call(
        PendingToolCall { index, call },
        executor.workspace.to_owned(),
        Arc::clone(&executor.capabilities),
        Arc::clone(&executor.config),
        Arc::clone(shell_sessions),
        executor.events.clone(),
    )
    .await
}

fn finish_tool_call(
    raw: RawToolResult,
    policy: &mut phi_steel::Policy,
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    completed: &mut Vec<(usize, ToolResult)>,
) -> Result<()> {
    let (result, display) = match raw.result {
        RawToolOutput::Value { result, display } => (result, display),
        RawToolOutput::Http {
            implementation,
            event:
                Event::HttpCompleted {
                    success: true,
                    events,
                    ..
                },
        } => (
            policy
                .complete_callable_tool(&implementation, &events)
                .unwrap_or_else(|error| serde_json::json!({ "error": error.to_string() })),
            None,
        ),
        RawToolOutput::Http {
            event: Event::HttpCompleted { error, .. },
            ..
        } => (serde_json::json!({ "error": error }), None),
        RawToolOutput::Http { .. } => (
            serde_json::json!({ "error": "invalid HTTP tool result" }),
            None,
        ),
    };
    send(
        events,
        RuntimeEvent::ToolCompleted {
            call_id: raw.call_id.clone(),
            name: raw.name.clone(),
            result: display.unwrap_or_else(|| result.clone()),
        },
    )?;
    completed.push((
        raw.index,
        ToolResult {
            call_id: raw.call_id,
            name: raw.name,
            result,
        },
    ));
    Ok(())
}

fn emit_stream_events(
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    provider_event: &serde_json::Value,
    rules: &[StreamRule],
) -> bool {
    let mut emitted = false;
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
                call_id: String::new(),
                name: rule.name.clone(),
                arguments: value,
            }),
            "tool_completed" => Some(RuntimeEvent::ToolCompleted {
                call_id: String::new(),
                name: rule.name.clone(),
                result: value,
            }),
            _ => None,
        };
        if let Some(event) = event {
            let _ = events.send(event);
            emitted = true;
        }
    }
    emitted
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

fn execute_file_edit(
    workspace: &Path,
    policy: &mut phi_steel::Policy,
    name: &str,
    arguments: &serde_json::Value,
) -> Result<(serde_json::Value, serde_json::Value)> {
    let preparation: phi_core::file_edit::EditPreparation =
        serde_json::from_value(policy.prepare_file_edit(name, arguments)?)?;
    let snapshots = phi_core::file_edit::snapshots(workspace, &preparation.targets)?;
    let changes: Vec<phi_core::file_edit::FileChange> = serde_json::from_value(
        policy.propose_file_edit(name, &preparation.plan, &serde_json::to_value(&snapshots)?)?,
    )?;
    let summaries = changes.iter().map(file_change_summary).collect::<Vec<_>>();
    let display = changes
        .iter()
        .map(|change| file_change_display(change, &snapshots))
        .collect::<Result<Vec<_>>>()?;
    phi_core::file_edit::apply(workspace, &snapshots, &changes)?;
    Ok((
        serde_json::json!({ "changes": summaries }),
        serde_json::json!({ "changes": display }),
    ))
}

fn file_change_summary(change: &phi_core::file_edit::FileChange) -> serde_json::Value {
    match change {
        phi_core::file_edit::FileChange::Create { path, .. } => {
            serde_json::json!({ "operation": "create", "path": path })
        }
        phi_core::file_edit::FileChange::Replace { path, .. } => {
            serde_json::json!({ "operation": "replace", "path": path })
        }
        phi_core::file_edit::FileChange::Delete { path } => {
            serde_json::json!({ "operation": "delete", "path": path })
        }
        phi_core::file_edit::FileChange::Move {
            path, destination, ..
        } => serde_json::json!({
            "operation": "move",
            "path": path,
            "destination": destination
        }),
    }
}

fn file_change_display(
    change: &phi_core::file_edit::FileChange,
    snapshots: &[phi_core::file_edit::FileSnapshot],
) -> Result<serde_json::Value> {
    let mut summary = file_change_summary(change);
    let (old_path, new_path, old, new) = match change {
        phi_core::file_edit::FileChange::Create { path, content } => {
            ("/dev/null", path.as_str(), "", content.as_str())
        }
        phi_core::file_edit::FileChange::Replace { path, content } => (
            path.as_str(),
            path.as_str(),
            snapshot_content(snapshots, path)?,
            content.as_str(),
        ),
        phi_core::file_edit::FileChange::Delete { path } => (
            path.as_str(),
            "/dev/null",
            snapshot_content(snapshots, path)?,
            "",
        ),
        phi_core::file_edit::FileChange::Move {
            path,
            destination,
            content,
        } => (
            path.as_str(),
            destination.as_str(),
            snapshot_content(snapshots, path)?,
            content.as_str(),
        ),
    };
    summary["diff"] = TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(1)
        .header(old_path, new_path)
        .to_string()
        .into();
    Ok(summary)
}

fn snapshot_content<'a>(
    snapshots: &'a [phi_core::file_edit::FileSnapshot],
    path: &str,
) -> Result<&'a str> {
    snapshots
        .iter()
        .find(|snapshot| snapshot.path == path)
        .map(|snapshot| snapshot.content.as_str())
        .with_context(|| format!("missing file snapshot: {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Condvar, Mutex},
        time::Duration,
    };

    struct ConcurrentTool {
        gate: Arc<(Mutex<usize>, Condvar)>,
    }

    impl phi_core::capability::Tool for ConcurrentTool {
        fn spec(&self) -> phi_protocol::ToolSpec {
            phi_protocol::ToolSpec {
                name: "concurrent_test".into(),
                description: String::new(),
                parameters: serde_json::json!({ "type": "object" }),
            }
        }

        fn execute(
            &self,
            _workspace: &Path,
            arguments: serde_json::Value,
        ) -> Result<serde_json::Value> {
            let (lock, ready) = &*self.gate;
            let mut count = lock.lock().unwrap();
            *count += 1;
            ready.notify_all();
            while *count < 2 {
                let (next, timeout) = ready.wait_timeout(count, Duration::from_secs(1)).unwrap();
                count = next;
                if timeout.timed_out() {
                    bail!("tool calls did not overlap");
                }
            }
            let id = arguments["id"].as_str().unwrap();
            if id == "first" {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(serde_json::json!({ "concurrent": true, "id": id }))
        }

        fn parallel_safe(&self) -> bool {
            true
        }
    }

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
            processes: Arc::new(phi_core::process::ShellSessions::default()),
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
        assert!(catalog.commands.iter().any(|command| command.name == "ps"));
        assert!(
            catalog
                .commands
                .iter()
                .any(|command| command.name == "stop")
        );
        assert_eq!(catalog.models[0].id, "openai/gpt-5.6-luna");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn process_commands_use_the_shared_registry() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (workspace, mut options) = options();
                std::fs::create_dir(workspace.path().join("nested")).unwrap();
                options
                    .processes
                    .exec(
                        workspace.path(),
                        &serde_json::json!({
                            "cmd": "trap 'printf stopped > stop-marker; exit 0' INT; printf ready; : > armed; sleep 10",
                            "yield_time_ms": 250
                        }),
                        |_| {},
                    )
                    .await
                    .unwrap();
                options
                    .processes
                    .exec(
                        workspace.path(),
                        &serde_json::json!({
                            "cmd": "sleep 10",
                            "workdir": "nested",
                            "yield_time_ms": 0
                        }),
                        |_| {},
                    )
                    .await
                    .unwrap();
                let listed = execute_command(
                    &options,
                    &CommandInvocation {
                        name: "ps".into(),
                        arguments: String::new(),
                    },
                )
                .unwrap();
                assert_eq!(listed.role, "processes");
                assert!(listed.content.starts_with("## Background processes"));
                assert!(listed.content.contains("\n\n• **Running for"));
                assert!(listed.content.contains("Running for"));
                assert!(!listed.content.contains("session 1"));
                assert!(listed.content.contains("printf ready"));
                assert!(listed.content.contains("`nested`"));
                assert!(!listed.content.contains("cwd"));
                assert!(!listed.content.contains("`.`"));
                assert_eq!(listed.content.matches("ready").count(), 1);
                options.session_id = Some(listed.session_id);

                let stopped = execute_command(
                    &options,
                    &CommandInvocation {
                        name: "stop".into(),
                        arguments: String::new(),
                    },
                )
                .unwrap();
                assert_eq!(stopped.content, "Stopped 2 background processes.");
                assert!(options.processes.list().unwrap().is_empty());
                assert_eq!(
                    std::fs::read_to_string(workspace.path().join("stop-marker")).unwrap(),
                    "stopped"
                );
            })
            .await;
    }

    #[test]
    fn discovers_manually_copied_skills() {
        let (_workspace, options) = options();
        let home = home_for_config(&options.config_path).unwrap();
        let skill = home.skills().join("review");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: review\ndescription: Review code.\n---\n\nBe precise.",
        )
        .unwrap();

        let catalog = command_catalog(&options).unwrap();
        assert!(
            catalog
                .commands
                .iter()
                .any(|command| command.name == "skills")
        );
        let execution = execute_command(
            &options,
            &CommandInvocation {
                name: "skills".into(),
                arguments: String::new(),
            },
        )
        .unwrap();
        assert_eq!(execution.content, "- review: Review code.");
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
        let home = home_for_config(&options.config_path).unwrap();
        let sources = session.composed_sources().unwrap().unwrap();
        let capabilities = capabilities(&sources, &home);
        let skills = phi_core::skill::discover(&home.skills(), &options.workspace).unwrap();
        let plugins = entrypoints(&sources);
        let mut policy = phi_steel::Policy::load_with_state(
            &sources.policy,
            &plugins,
            &sources.main,
            &policy_config(&capabilities, session.id(), &UserState::default(), &skills),
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
                    && body["tools"].as_array().unwrap().len() == 8
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "read_file")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "exec_command")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "write_stdin")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "list_processes")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "terminate_process")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "patch")
                    && !body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "replace_file")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["type"] == "web_search")
                    && body["instructions"]
                        .as_str().unwrap().contains("running inside a Phi harness")
        ));
    }

    #[test]
    fn reads_provider_reported_token_counts() {
        assert_eq!(number_u64(&serde_json::json!(378.0)), Some(378));
        assert_eq!(number_u64(&serde_json::Value::Null), None);
    }

    #[test]
    fn selected_steel_editor_applies_a_patch_through_rust_core() {
        let (workspace, options) = options();
        std::fs::create_dir(workspace.path().join("src")).unwrap();
        std::fs::write(
            workspace.path().join("src/main.rs"),
            "fn main() {\n    old();\n}\n",
        )
        .unwrap();
        std::fs::write(workspace.path().join("src/old.rs"), "remove me\n").unwrap();
        std::fs::write(workspace.path().join("src/move.rs"), "pub fn before() {}\n").unwrap();
        let home = home_for_config(&options.config_path).unwrap();
        let sources = resolve_sources(&home, workspace.path()).unwrap();
        let capabilities = capabilities(&sources, &home);
        let mut policy = phi_steel::Policy::load_with_state(
            &sources.policy,
            &entrypoints(&sources),
            &sources.main,
            &policy_config(&capabilities, "test", &load_user_state(&home).unwrap(), &[]),
            None,
        )
        .unwrap();

        let (result, display) = execute_file_edit(
            workspace.path(),
            &mut policy,
            "patch",
            &serde_json::json!({
                "patch": concat!(
                    "*** Begin Patch\n",
                    "*** Update File: src/main.rs\n",
                    "@@ fn main() {\n",
                    "-    old();\n",
                    "+    new();\n",
                    "*** Add File: src/lib.rs\n",
                    "+pub fn added() {}\n",
                    "*** Delete File: src/old.rs\n",
                    "*** Update File: src/move.rs\n",
                    "*** Move to: src/moved.rs\n",
                    "@@\n",
                    "-pub fn before() {}\n",
                    "+pub fn after() {}\n",
                    "*** End Patch\n"
                )
            }),
        )
        .unwrap();

        assert_eq!(result["changes"].as_array().unwrap().len(), 4);
        assert!(result["changes"][0].get("diff").is_none());
        assert!(
            display["changes"][0]["diff"]
                .as_str()
                .unwrap()
                .contains("-    old();\n+    new();")
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("src/main.rs")).unwrap(),
            "fn main() {\n    new();\n}\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("src/lib.rs")).unwrap(),
            "pub fn added() {}\n"
        );
        assert!(!workspace.path().join("src/old.rs").exists());
        assert!(!workspace.path().join("src/move.rs").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("src/moved.rs")).unwrap(),
            "pub fn after() {}\n"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executes_parallel_safe_calls_concurrently() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (workspace, options) = options();
                let home = home_for_config(&options.config_path).unwrap();
                let sources = resolve_sources(&home, workspace.path()).unwrap();
                let mut policy = phi_steel::Policy::load_with_state(
                    &sources.policy,
                    &entrypoints(&sources),
                    &sources.main,
                    &policy_config(
                        &capabilities(&sources, &home),
                        "test",
                        &load_user_state(&home).unwrap(),
                        &[],
                    ),
                    None,
                )
                .unwrap();
                let gate = Arc::new((Mutex::new(0), Condvar::new()));
                let mut registry = phi_core::capability::Registry::default();
                registry.register(ConcurrentTool { gate });
                let registry = Arc::new(registry);
                let config = Arc::new(Config {
                    allowed_programs: HashSet::new(),
                    allowed_http_origins: HashSet::new(),
                    secrets: BTreeMap::new(),
                });
                let (event_tx, _event_rx) = mpsc::unbounded_channel();
                let (_command_tx, mut command_rx) = mpsc::unbounded_channel();
                let calls = ["first", "second"]
                    .into_iter()
                    .map(|call_id| ToolCall {
                        call_id: call_id.into(),
                        name: "concurrent_test".into(),
                        arguments: serde_json::json!({ "id": call_id }),
                        execution: ToolExecution::Direct,
                    })
                    .collect();
                let cancellation = CancellationToken::new();
                let executor = ToolBatchExecutor {
                    workspace: workspace.path(),
                    capabilities: registry,
                    config,
                    file_editor_tool: "patch",
                    events: &event_tx,
                    cancellation: &cancellation,
                };
                let results = execute_tool_calls(
                    calls,
                    &executor,
                    &phi_core::permissions::Permissions {
                        allow_shell: true,
                        allow_write: true,
                    },
                    false,
                    &mut command_rx,
                    &mut policy,
                    &Arc::new(phi_core::process::ShellSessions::default()),
                )
                .await
                .unwrap();
                assert_eq!(results.len(), 2);
                assert_eq!(results[0].call_id, "first");
                assert_eq!(results[1].call_id, "second");
                assert!(
                    results
                        .iter()
                        .all(|result| result.result["concurrent"] == true)
                );
            })
            .await;
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
