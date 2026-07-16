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

mod workflow;

pub use workflow::WorkflowTasks;

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
    pub full_access: bool,
    pub processes: Arc<phi_core::process::ShellSessions>,
    pub workflows: Arc<WorkflowTasks>,
    pub output_schema: Option<serde_json::Value>,
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
    CatalogUpdated {
        catalog: CommandCatalog,
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

#[derive(Debug, Clone, Serialize)]
pub struct CommandCatalog {
    pub commands: Vec<CommandSpec>,
    pub models: Vec<ModelSpec>,
    pub selected_model: Option<String>,
    pub selected_reasoning: Option<String>,
    pub selected_service_tier: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HarnessStatus {
    pub version: String,
    pub workspace: String,
    pub home: String,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub service_tier: Option<String>,
    pub config: SchemeConfigStatus,
    pub plugins: Vec<PluginStatus>,
    pub composition: phi_steel::CompositionStatus,
}

#[derive(Clone, Debug, Serialize)]
pub struct SchemeConfigStatus {
    pub path: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginStatus {
    pub name: String,
    pub source: String,
    pub revision: String,
}

#[derive(Debug)]
pub struct CommandExecution {
    pub session_id: String,
    pub content: String,
    pub role: String,
    pub catalog: CommandCatalog,
    pub action: CommandAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandAction {
    Display,
    NewSession,
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

const DEFAULT_SCHEME_CONFIG: &str = include_str!("../../../config.scm");
const OFFICIAL_PLUGINS: &[(&str, &str)] = &[
    ("responses", "policy/providers/responses.scm"),
    ("openai", "policy/providers/openai.scm"),
    ("openrouter", "policy/providers/openrouter.scm"),
    ("openai-web-search", "policy/tools/openai-web-search.scm"),
    (
        "openrouter-web-search",
        "policy/tools/openrouter-web-search.scm",
    ),
    ("skills", "policy/tools/skills.scm"),
    ("dynamic-workflows", "policy/tools/dynamic-workflows"),
    ("codex-patch", "policy/tools/codex-patch.scm"),
    ("simple-prompt", "policy/prompts/simple.scm"),
    ("simple-compaction", "policy/compaction/simple.scm"),
    ("compaction-structured", "policy/compaction/structured.scm"),
];
const PHI_HARNESS_SKILL: &[(&str, &str)] = &[
    (
        "SKILL.md",
        include_str!("../../../skills/phi-harness/SKILL.md"),
    ),
    (
        "references/architecture.md",
        include_str!("../../../skills/phi-harness/references/architecture.md"),
    ),
    (
        "references/configuration.md",
        include_str!("../../../skills/phi-harness/references/configuration.md"),
    ),
    (
        "references/extensions.md",
        include_str!("../../../skills/phi-harness/references/extensions.md"),
    ),
    (
        "references/operations.md",
        include_str!("../../../skills/phi-harness/references/operations.md"),
    ),
];

pub fn initialize_home() -> Result<phi_core::home::PhiHome> {
    let home = phi_core::home::PhiHome::discover()?;
    initialize_at(&home)?;
    Ok(home)
}

pub fn initialize_at(home: &phi_core::home::PhiHome) -> Result<()> {
    std::fs::create_dir_all(&home.root)?;
    std::fs::create_dir_all(home.skills())?;
    write_if_missing(&home.config(), DEFAULT_CONFIG)?;
    initialize_scheme_config(home)?;
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
    let repository = repository_root();
    for (name, relative) in OFFICIAL_PLUGINS {
        copy_official_plugin(home, &repository, name, relative)?;
    }
    for (relative, content) in PHI_HARNESS_SKILL {
        write_bundled(
            &home.builtin_skills().join("phi-harness").join(relative),
            content,
        )?;
    }
    Ok(())
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn copy_official_plugin(
    home: &phi_core::home::PhiHome,
    repository: &Path,
    name: &str,
    relative: &str,
) -> Result<()> {
    let source = repository.join(relative);
    if !source.exists() {
        bail!(
            "official plugin source is missing: {} (the repository used to install phi must remain available)",
            source.display()
        );
    }
    let plugin = home.builtins().join("plugins").join(name);
    if source.is_dir() {
        if plugin.exists() {
            std::fs::remove_dir_all(&plugin)?;
        }
        return copy_tree(&source, &plugin);
    }
    write_bundled(
        &plugin.join("plugin.json"),
        &serde_json::to_string_pretty(&serde_json::json!({
            "name": name,
            "version": env!("CARGO_PKG_VERSION"),
            "entrypoint": "main.scm"
        }))?,
    )?;
    if let Some(parent) = plugin.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&source, plugin.join("main.scm"))
        .with_context(|| format!("copy official plugin {name} from {}", source.display()))?;
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else {
            std::fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
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

fn initialize_scheme_config(home: &phi_core::home::PhiHome) -> Result<()> {
    let target = home.scheme_config();
    if target.exists() {
        return Ok(());
    }
    let legacy = home.root.join("main.scm");
    if legacy.is_file() {
        let (agent, _) = DEFAULT_SCHEME_CONFIG
            .split_once("(load-plugin!")
            .context("default config has no plugin composition")?;
        let composition = std::fs::read_to_string(legacy)?;
        return write_if_missing(&target, &format!("{}{}", agent, composition));
    }
    write_if_missing(&target, DEFAULT_SCHEME_CONFIG)
}

fn home_for_config(path: &Path) -> Result<phi_core::home::PhiHome> {
    Ok(phi_core::home::PhiHome {
        root: path.parent().context("config has no parent")?.to_owned(),
    })
}

fn resolve_sources(
    home: &phi_core::home::PhiHome,
    _workspace: &Path,
) -> Result<phi_core::session::ComposedSources> {
    let config = home.scheme_config();
    let lock = phi_core::plugin::read_lock(home)?;
    let mut plugins = Vec::new();
    for name in phi_steel::composition_plugins(&config)? {
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
    Ok(phi_core::session::ComposedSources { config, plugins })
}

fn entrypoints(sources: &phi_core::session::ComposedSources) -> Vec<PathBuf> {
    sources
        .plugins
        .iter()
        .map(|plugin| plugin.entrypoint.clone())
        .collect()
}

fn reload_composition(
    home: &phi_core::home::PhiHome,
    config_path: &Path,
    workspace: &Path,
    session: &phi_core::session::Session,
    state: Option<String>,
    full_access: bool,
) -> Result<(phi_steel::Policy, CommandCatalog)> {
    let _config = load_config(config_path)?;
    let sources = resolve_sources(home, workspace)?;
    let capabilities = capabilities(home, full_access);
    let skills = discover_skills(home, workspace)?;
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.config,
        &entrypoints(&sources),
        &policy_config(
            &capabilities,
            session.id(),
            &load_user_state(home)?,
            &skills,
        ),
        state,
    )?;
    let catalog = catalog(&mut policy)?;
    session.replace_composition(&sources.config, &sources.plugins)?;
    Ok((policy, catalog))
}

pub fn check_scheme_config(home: &phi_core::home::PhiHome, workspace: &Path) -> Result<()> {
    let sources = resolve_sources(home, workspace)?;
    phi_steel::check(&sources.config, &entrypoints(&sources))?;
    phi_steel::replay_smoke(&sources.config, &entrypoints(&sources))
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
    let capabilities = capabilities(&home, options.full_access);
    let skills = discover_skills(&home, &workspace)?;
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.config,
        &entrypoints(&sources),
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

pub fn harness_status(options: &RunOptions) -> Result<HarnessStatus> {
    let workspace = options.workspace.canonicalize()?;
    let _config = load_config(&options.config_path)?;
    let home = home_for_config(&options.config_path)?;
    let sources = resolve_sources(&home, &workspace)?;
    let capabilities = capabilities(&home, options.full_access);
    let skills = discover_skills(&home, &workspace)?;
    let user_state = load_user_state(&home)?;
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.config,
        &entrypoints(&sources),
        &policy_config(&capabilities, "status", &user_state, &skills),
        None,
    )?;
    let composition = policy.composition_status()?;
    let lock = phi_core::plugin::read_lock(&home)?;
    let plugins = sources
        .plugins
        .iter()
        .map(|plugin| {
            lock.plugins
                .iter()
                .find(|locked| locked.name == plugin.name)
                .map_or_else(
                    || PluginStatus {
                        name: plugin.name.clone(),
                        source: "builtin".into(),
                        revision: env!("CARGO_PKG_VERSION").into(),
                    },
                    |locked| PluginStatus {
                        name: plugin.name.clone(),
                        source: "git".into(),
                        revision: locked.commit.clone(),
                    },
                )
        })
        .collect();
    let selection = user_state.model;

    Ok(HarnessStatus {
        version: env!("CARGO_PKG_VERSION").into(),
        workspace: workspace.display().to_string(),
        home: home.root.display().to_string(),
        model: selection.as_ref().map(|model| model.id.clone()),
        reasoning: selection.as_ref().map(|model| model.reasoning.clone()),
        service_tier: selection.as_ref().map(|model| model.service_tier.clone()),
        config: SchemeConfigStatus {
            path: sources.config.display().to_string(),
        },
        plugins,
        composition,
    })
}

pub fn execute_command(
    options: &RunOptions,
    invocation: &CommandInvocation,
) -> Result<CommandExecution> {
    let workspace = options.workspace.canonicalize()?;
    let _config = load_config(&options.config_path)?;
    let home = home_for_config(&options.config_path)?;
    let sessions = workspace.join(".phi/sessions");
    if invocation.name == "new" {
        if !invocation.arguments.is_empty() {
            bail!("usage: /new");
        }
        let current = resolve_sources(&home, &workspace)?;
        let session = phi_core::session::Session::create_composed(
            &sessions,
            &current.config,
            &current.plugins,
        )?;
        let sources = session
            .composed_sources()?
            .context("missing composition snapshot")?;
        let capabilities = capabilities(&home, options.full_access);
        let skills = discover_skills(&home, &workspace)?;
        let mut policy = phi_steel::Policy::load_with_state(
            &sources.config,
            &entrypoints(&sources),
            &policy_config(
                &capabilities,
                session.id(),
                &load_user_state(&home)?,
                &skills,
            ),
            None,
        )?;
        session.save_state(policy.state())?;
        return Ok(CommandExecution {
            session_id: session.id().into(),
            content: "Started a new chat.".into(),
            role: "note".into(),
            catalog: catalog(&mut policy)?,
            action: CommandAction::NewSession,
        });
    }
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
                &current.config,
                &current.plugins,
            )?;
            let sources = session
                .composed_sources()?
                .context("missing composition snapshot")?;
            (session, sources, None)
        }
    };
    if invocation.name == "reload" {
        if !invocation.arguments.is_empty() {
            bail!("usage: /reload");
        }
        let (mut policy, reloaded_catalog) = reload_composition(
            &home,
            &options.config_path,
            &workspace,
            &session,
            saved_state,
            options.full_access,
        )?;
        session.save_state(policy.state())?;
        return Ok(CommandExecution {
            session_id: session.id().into(),
            content: format!(
                "Reloaded configuration · {} models · {} commands",
                reloaded_catalog.models.len(),
                reloaded_catalog.commands.len()
            ),
            role: "note".into(),
            catalog: catalog(&mut policy)?,
            action: CommandAction::Display,
        });
    }
    let capabilities = capabilities(&home, options.full_access);
    let skills = discover_skills(&home, &workspace)?;
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.config,
        &entrypoints(&sources),
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
        action: CommandAction::Display,
    })
}

fn catalog(policy: &mut phi_steel::Policy) -> Result<CommandCatalog> {
    let mut commands = vec![
        CommandSpec {
            name: "compact".into(),
            usage: "/compact".into(),
            description: "Compact the current conversation now.".into(),
            source: "core".into(),
        },
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
            name: "new".into(),
            usage: "/new".into(),
            description: "Start a new chat.".into(),
            source: "core".into(),
        },
        CommandSpec {
            name: "ps".into(),
            usage: "/ps".into(),
            description: "Show managed background processes.".into(),
            source: "core".into(),
        },
        CommandSpec {
            name: "reload".into(),
            usage: "/reload".into(),
            description: "Reload configuration and plugins.".into(),
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
    policy_config_with_schema(capabilities, session_id, user_state, skills, None)
}

fn policy_config_with_schema(
    capabilities: &phi_core::capability::Registry,
    session_id: &str,
    user_state: &UserState,
    skills: &[phi_core::skill::SkillSpec],
    output_schema: Option<&serde_json::Value>,
) -> String {
    let mut tools = capabilities.specs();
    tools.push(phi_core::capability::exec_command_spec());
    tools.push(phi_core::capability::list_processes_spec());
    tools.push(phi_core::capability::reload_config_spec());
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
    if let Some(schema) = output_schema {
        value["output_schema"] = schema.clone();
    }
    value.to_string()
}

fn capabilities(
    home: &phi_core::home::PhiHome,
    full_access: bool,
) -> phi_core::capability::Registry {
    let mut capabilities = phi_core::capability::Registry::default();
    capabilities.register(phi_core::capability::ReadFile {
        full_access,
        additional_root: Some(home.root.clone()),
    });
    capabilities.register_hidden(phi_core::skill::LoadSkill {
        system_root: home.builtin_skills(),
        personal_root: home.skills(),
    });
    capabilities
}

fn discover_skills(
    home: &phi_core::home::PhiHome,
    workspace: &Path,
) -> Result<Vec<phi_core::skill::SkillSpec>> {
    phi_core::skill::discover(&home.builtin_skills(), &home.skills(), workspace)
}

pub fn start(options: RunOptions, prompt: String) -> Handle {
    start_event(options, Event::UserMessage { content: prompt })
}

pub fn compact(options: RunOptions) -> Handle {
    start_event(options, Event::CompactRequested)
}

fn start_event(options: RunOptions, initial_event: Event) -> Handle {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let cancellation = CancellationToken::new();
    let run_cancellation = cancellation.clone();
    tokio::task::spawn_local(async move {
        if let Err(error) = run(
            options,
            initial_event,
            &event_tx,
            command_rx,
            &run_cancellation,
        )
        .await
        {
            let _ = event_tx.send(RuntimeEvent::Error {
                message: runtime_error(&error),
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
    initial_event: Event,
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    mut commands: mpsc::UnboundedReceiver<RuntimeCommand>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let workspace = options.workspace.canonicalize()?;
    let mut config = load_config(&options.config_path)?;
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
                &current.config,
                &current.plugins,
            )?;
            let sources = session
                .composed_sources()?
                .context("missing composition snapshot")?;
            (session, sources, None)
        }
    };
    let mut plugin_roots = sources
        .plugins
        .iter()
        .map(|plugin| (plugin.name.clone(), plugin.root.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    let mut capabilities = Arc::new(capabilities(&home, options.full_access));
    let skills = discover_skills(&home, &workspace)?;
    send(
        events,
        RuntimeEvent::Session {
            id: session.id().into(),
        },
    )?;
    if let Event::UserMessage { content } = &initial_event {
        send(
            events,
            RuntimeEvent::UserMessage {
                content: content.clone(),
            },
        )?;
    }
    let mut policy = phi_steel::Policy::load_with_state(
        &sources.config,
        &entrypoints(&sources),
        &policy_config_with_schema(
            &capabilities,
            session.id(),
            &load_user_state(&home)?,
            &skills,
            options.output_schema.as_ref(),
        ),
        saved_state,
    )?;
    let mut file_editor_tool = policy.file_editor_tool_name()?;
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
    let mut event = initial_event;
    let mut activity = "ready".to_owned();
    let permissions = phi_core::permissions::Permissions {
        allow_shell: options.allow_shell,
        allow_write: options.allow_write,
    };
    let shell_sessions = Arc::clone(&options.processes);

    loop {
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
                    home: &home,
                    config_path: &options.config_path,
                    session: &session,
                    capabilities: Arc::clone(&capabilities),
                    config: Arc::new(config.clone()),
                    file_editor_tool: &file_editor_tool,
                    events,
                    cancellation,
                    full_access: options.full_access,
                    workflows: Arc::clone(&options.workflows),
                    plugin_roots: &plugin_roots,
                };
                let (results, reloaded) = execute_tool_calls(
                    calls,
                    &executor,
                    &permissions,
                    options.interactive_approvals,
                    &mut commands,
                    &mut policy,
                    &shell_sessions,
                )
                .await?;
                if reloaded {
                    let state = policy.state().to_owned();
                    let sources = session
                        .composed_sources()?
                        .context("session has no composition snapshot")?;
                    plugin_roots = sources
                        .plugins
                        .iter()
                        .map(|plugin| (plugin.name.clone(), plugin.root.clone()))
                        .collect();
                    config = load_config(&options.config_path)?;
                    capabilities = Arc::new(crate::capabilities(&home, options.full_access));
                    let skills = discover_skills(&home, &workspace)?;
                    policy = phi_steel::Policy::load_with_state(
                        &sources.config,
                        &entrypoints(&sources),
                        &policy_config_with_schema(
                            &capabilities,
                            session.id(),
                            &load_user_state(&home)?,
                            &skills,
                            options.output_schema.as_ref(),
                        ),
                        Some(state),
                    )?;
                    file_editor_tool = policy.file_editor_tool_name()?;
                }
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
    home: &'a phi_core::home::PhiHome,
    config_path: &'a Path,
    session: &'a phi_core::session::Session,
    capabilities: Arc<phi_core::capability::Registry>,
    config: Arc<Config>,
    file_editor_tool: &'a str,
    events: &'a mpsc::UnboundedSender<RuntimeEvent>,
    cancellation: &'a CancellationToken,
    full_access: bool,
    workflows: Arc<WorkflowTasks>,
    plugin_roots: &'a std::collections::HashMap<String, PathBuf>,
}

async fn execute_tool_calls(
    calls: Vec<ToolCall>,
    executor: &ToolBatchExecutor<'_>,
    permissions: &phi_core::permissions::Permissions,
    interactive_approvals: bool,
    commands: &mut mpsc::UnboundedReceiver<RuntimeCommand>,
    policy: &mut phi_steel::Policy,
    shell_sessions: &Arc<phi_core::process::ShellSessions>,
) -> Result<(Vec<ToolResult>, bool)> {
    let mut completed = Vec::new();
    let mut parallel = Vec::new();
    let mut reloaded = false;
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
        if approved && call.name == "reload_config" {
            let result = reload_composition(
                executor.home,
                executor.config_path,
                executor.workspace,
                executor.session,
                Some(policy.state().to_owned()),
                executor.full_access,
            )
            .map(|(_, catalog)| {
                let _ = executor.events.send(RuntimeEvent::CatalogUpdated {
                    catalog: catalog.clone(),
                });
                reloaded = true;
                serde_json::json!({
                    "reloaded": true,
                    "models": catalog.models.len(),
                    "commands": catalog.commands.len()
                })
            })
            .unwrap_or_else(|error| serde_json::json!({ "error": runtime_error(&error) }));
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
        } else if approved {
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
    Ok((
        completed.into_iter().map(|(_, result)| result).collect(),
        reloaded,
    ))
}

fn tool_call_parallel_safe(
    call: &ToolCall,
    capabilities: &phi_core::capability::Registry,
    file_editor_tool: &str,
) -> bool {
    if matches!(call.name.as_str(), "Workflow" | "TaskOutput" | "TaskStop") {
        return false;
    }
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
        let full_access = executor.full_access;
        tasks.spawn_local(async move {
            execute_parallel_call(
                call,
                workspace,
                capabilities,
                config,
                shell_sessions,
                events,
                full_access,
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
    full_access: bool,
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
            .exec_with_access(&workspace, &arguments, full_access, move |content| {
                let _ = events.send(RuntimeEvent::ToolOutput {
                    call_id: event_call_id.clone(),
                    name: event_name.clone(),
                    content: content.to_owned(),
                });
            })
            .await
            .unwrap_or_else(|error| serde_json::json!({ "error": runtime_error(&error) }));
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
            .unwrap_or_else(|error| serde_json::json!({ "error": runtime_error(&error) }));
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
                error: runtime_error(&error),
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
    if matches!(call.name.as_str(), "Workflow" | "TaskOutput" | "TaskStop") {
        let result = cancellable(executor.cancellation, async {
            match call.name.as_str() {
                "Workflow" => {
                    executor
                        .workflows
                        .launch(
                            executor.workspace,
                            &executor.home.root,
                            executor.session.dir(),
                            executor.plugin_roots,
                            &call.arguments,
                        )
                        .await
                }
                "TaskOutput" => {
                    executor
                        .workflows
                        .output(executor.session.dir(), &call.arguments)
                        .await
                }
                "TaskStop" => {
                    executor
                        .workflows
                        .stop(executor.session.dir(), &call.arguments)
                        .await
                }
                _ => unreachable!(),
            }
        })
        .await
        .and_then(|result| result)
        .unwrap_or_else(|error| serde_json::json!({ "error": runtime_error(&error) }));
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
                    .exec_with_access(
                        executor.workspace,
                        &call.arguments,
                        executor.full_access,
                        emit,
                    )
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
        .unwrap_or_else(|error| serde_json::json!({ "error": runtime_error(&error) }));
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
        let (result, display) = execute_file_edit(
            executor.workspace,
            &executor.home.root,
            policy,
            &call.name,
            &call.arguments,
            executor.full_access,
        )
        .map(|(result, display)| (result, Some(display)))
        .unwrap_or_else(|error| (serde_json::json!({ "error": runtime_error(&error) }), None));
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
        executor.full_access,
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
                .unwrap_or_else(|error| serde_json::json!({ "error": runtime_error(&error) })),
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

fn runtime_error(error: &anyhow::Error) -> String {
    phi_steel::user_error_message(error).unwrap_or_else(|| format!("{error:#}"))
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
    config_root: &Path,
    policy: &mut phi_steel::Policy,
    name: &str,
    arguments: &serde_json::Value,
    full_access: bool,
) -> Result<(serde_json::Value, serde_json::Value)> {
    let preparation: phi_core::file_edit::EditPreparation =
        serde_json::from_value(policy.prepare_file_edit(name, arguments)?)?;
    let snapshots = phi_core::file_edit::snapshots(
        workspace,
        &preparation.targets,
        full_access,
        Some(config_root),
    )?;
    let changes: Vec<phi_core::file_edit::FileChange> = serde_json::from_value(
        policy.propose_file_edit(name, &preparation.plan, &serde_json::to_value(&snapshots)?)?,
    )?;
    let summaries = changes.iter().map(file_change_summary).collect::<Vec<_>>();
    let display = changes
        .iter()
        .map(|change| file_change_display(change, &snapshots))
        .collect::<Result<Vec<_>>>()?;
    phi_core::file_edit::apply(
        workspace,
        &snapshots,
        &changes,
        full_access,
        Some(config_root),
    )?;
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
            full_access: false,
            processes: Arc::new(phi_core::process::ShellSessions::default()),
            workflows: Arc::new(WorkflowTasks::default()),
            output_schema: None,
        };
        (workspace, options)
    }

    fn add_reloaded_model(home: &phi_core::home::PhiHome) {
        let mut main = std::fs::read_to_string(home.scheme_config()).unwrap();
        main.push_str(
            r#"
(register-model!
  "openrouter"
  (hash 'id "example/reloaded"
        'label "example/reloaded"
        'description "Added during a running conversation."
        'context_window 100000
        'compaction_token_limit 90000))
"#,
        );
        std::fs::write(home.scheme_config(), main).unwrap();
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
        assert!(catalog.commands.iter().any(|command| command.name == "new"));
        assert!(
            catalog
                .commands
                .iter()
                .any(|command| command.name == "compact")
        );
        assert!(catalog.commands.iter().any(|command| command.name == "ps"));
        assert!(
            catalog
                .commands
                .iter()
                .any(|command| command.name == "reload")
        );
        assert!(
            catalog
                .commands
                .iter()
                .any(|command| command.name == "stop")
        );
        assert_eq!(catalog.models[0].id, "openai/gpt-5.6-luna");
    }

    #[test]
    fn new_command_creates_a_fresh_session_and_preserves_the_old_one() {
        let (workspace, mut options) = options();
        let created = execute_command(
            &options,
            &CommandInvocation {
                name: "help".into(),
                arguments: String::new(),
            },
        )
        .unwrap();
        options.session_id = Some(created.session_id.clone());

        let fresh = execute_command(
            &options,
            &CommandInvocation {
                name: "new".into(),
                arguments: String::new(),
            },
        )
        .unwrap();

        assert_ne!(fresh.session_id, created.session_id);
        assert_eq!(fresh.action, CommandAction::NewSession);
        assert_eq!(fresh.content, "Started a new chat.");
        assert!(
            fresh
                .catalog
                .commands
                .iter()
                .any(|command| command.name == "new")
        );
        phi_core::session::Session::open(
            &workspace.path().join(".phi/sessions"),
            &created.session_id,
        )
        .unwrap();
        let fresh_session = phi_core::session::Session::open(
            &workspace.path().join(".phi/sessions"),
            &fresh.session_id,
        )
        .unwrap();
        fresh_session.load_state().unwrap();
    }

    #[test]
    fn new_command_rejects_arguments() {
        let (_workspace, options) = options();
        let error = execute_command(
            &options,
            &CommandInvocation {
                name: "new".into(),
                arguments: "now".into(),
            },
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "usage: /new");
    }

    #[test]
    fn reload_adds_an_openrouter_model_without_restarting_the_session() {
        let (_workspace, mut options) = options();
        let created = execute_command(
            &options,
            &CommandInvocation {
                name: "help".into(),
                arguments: String::new(),
            },
        )
        .unwrap();
        options.session_id = Some(created.session_id.clone());
        let home = home_for_config(&options.config_path).unwrap();
        add_reloaded_model(&home);

        assert!(
            !command_catalog(&options)
                .unwrap()
                .models
                .iter()
                .any(|model| model.id == "openrouter/example/reloaded")
        );
        let reloaded = execute_command(
            &options,
            &CommandInvocation {
                name: "reload".into(),
                arguments: String::new(),
            },
        )
        .unwrap();

        assert_eq!(reloaded.session_id, created.session_id);
        assert!(
            reloaded
                .catalog
                .models
                .iter()
                .any(|model| model.id == "openrouter/example/reloaded")
        );
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
    fn invalid_reload_keeps_the_previous_session_composition() {
        let (_workspace, mut options) = options();
        let created = execute_command(
            &options,
            &CommandInvocation {
                name: "help".into(),
                arguments: String::new(),
            },
        )
        .unwrap();
        options.session_id = Some(created.session_id);
        let home = home_for_config(&options.config_path).unwrap();
        std::fs::write(home.scheme_config(), "(this is not valid configuration").unwrap();

        assert!(
            execute_command(
                &options,
                &CommandInvocation {
                    name: "reload".into(),
                    arguments: String::new(),
                },
            )
            .is_err()
        );
        assert_eq!(
            command_catalog(&options).unwrap().models[0].id,
            "openai/gpt-5.6-luna"
        );
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
        assert!(execution.content.contains("- review: Review code."));
        assert!(execution.content.contains("- phi-harness:"));
    }

    #[test]
    fn reports_the_resolved_harness_status() {
        let (workspace, options) = options();
        let status = harness_status(&options).unwrap();

        assert_eq!(
            status.workspace,
            workspace
                .path()
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
        assert_eq!(status.model.as_deref(), Some("openai/gpt-5.6-luna"));
        assert_eq!(
            status.config.path,
            options
                .config_path
                .with_file_name("config.scm")
                .display()
                .to_string()
        );
        assert_eq!(status.composition.prompt_builder, "simple");
        assert_eq!(status.composition.file_editor, "codex-patch");
        assert_eq!(status.composition.compactor, "structured");
        assert!(
            status
                .plugins
                .iter()
                .all(|plugin| plugin.source == "builtin")
        );
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
        let capabilities = capabilities(&home, false);
        let skills = discover_skills(&home, &options.workspace).unwrap();
        let plugins = entrypoints(&sources);
        let mut policy = phi_steel::Policy::load_with_state(
            &sources.config,
            &plugins,
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
                    && body["tools"].as_array().unwrap().len() == 12
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
                        .any(|tool| tool["name"] == "reload_config")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "load_skill")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "patch")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "Workflow" && tool["strict"] == false)
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "TaskOutput"
                            && tool["strict"] == true
                            && tool["parameters"]["required"]
                                == serde_json::json!(["task_id", "wait_ms"]))
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "TaskStop")
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
        let capabilities = capabilities(&home, false);
        let mut policy = phi_steel::Policy::load_with_state(
            &sources.config,
            &entrypoints(&sources),
            &policy_config(&capabilities, "test", &load_user_state(&home).unwrap(), &[]),
            None,
        )
        .unwrap();

        let (result, display) = execute_file_edit(
            workspace.path(),
            &home.root,
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
            false,
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

    #[test]
    fn unrestricted_editor_applies_a_patch_outside_the_workspace() {
        let (workspace, options) = options();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("outside.txt");
        std::fs::write(&path, "old\n").unwrap();
        let home = home_for_config(&options.config_path).unwrap();
        let sources = resolve_sources(&home, workspace.path()).unwrap();
        let capabilities = capabilities(&home, true);
        let mut policy = phi_steel::Policy::load_with_state(
            &sources.config,
            &entrypoints(&sources),
            &policy_config(&capabilities, "test", &load_user_state(&home).unwrap(), &[]),
            None,
        )
        .unwrap();
        execute_file_edit(
            workspace.path(),
            &home.root,
            &mut policy,
            "patch",
            &serde_json::json!({
                "patch": format!(
                    "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch\n",
                    path.display()
                )
            }),
            true,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new\n");
    }

    #[test]
    fn editor_can_reconfigure_phi_home_without_full_access() {
        let (workspace, options) = options();
        let home = home_for_config(&options.config_path).unwrap();
        let sources = resolve_sources(&home, workspace.path()).unwrap();
        let capabilities = capabilities(&home, false);
        let mut policy = phi_steel::Policy::load_with_state(
            &sources.config,
            &entrypoints(&sources),
            &policy_config(&capabilities, "test", &load_user_state(&home).unwrap(), &[]),
            None,
        )
        .unwrap();
        let path = home.root.join("reconfigured.scm");

        execute_file_edit(
            workspace.path(),
            &home.root,
            &mut policy,
            "patch",
            &serde_json::json!({
                "patch": format!(
                    "*** Begin Patch\n*** Add File: {}\n+(configured #t)\n*** End Patch\n",
                    path.display()
                )
            }),
            false,
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "(configured #t)\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executes_parallel_safe_calls_concurrently() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (workspace, options) = options();
                let home = home_for_config(&options.config_path).unwrap();
                let sources = resolve_sources(&home, workspace.path()).unwrap();
                let session = phi_core::session::Session::create_composed(
                    &workspace.path().join(".phi/sessions"),
                    &sources.config,
                    &sources.plugins,
                )
                .unwrap();
                let mut policy = phi_steel::Policy::load_with_state(
                    &sources.config,
                    &entrypoints(&sources),
                    &policy_config(
                        &capabilities(&home, false),
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
                let plugin_roots = std::collections::HashMap::new();
                let executor = ToolBatchExecutor {
                    workspace: workspace.path(),
                    home: &home,
                    config_path: &options.config_path,
                    session: &session,
                    capabilities: registry,
                    config,
                    file_editor_tool: "patch",
                    events: &event_tx,
                    cancellation: &cancellation,
                    full_access: false,
                    workflows: Arc::new(WorkflowTasks::default()),
                    plugin_roots: &plugin_roots,
                };
                let (results, reloaded) = execute_tool_calls(
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
                assert!(!reloaded);
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

    #[tokio::test(flavor = "current_thread")]
    async fn agent_reload_tool_updates_the_current_session_catalog() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (workspace, options) = options();
                let home = home_for_config(&options.config_path).unwrap();
                let sources = resolve_sources(&home, workspace.path()).unwrap();
                let session = phi_core::session::Session::create_composed(
                    &workspace.path().join(".phi/sessions"),
                    &sources.config,
                                        &sources.plugins,
                )
                .unwrap();
                let registry = Arc::new(capabilities(&home, false));
                let mut policy = phi_steel::Policy::load_with_state(
                    &sources.config,
                    &entrypoints(&sources),
                    &policy_config(
                        &registry,
                        session.id(),
                        &load_user_state(&home).unwrap(),
                        &[],
                    ),
                    None,
                )
                .unwrap();
                session.save_state(policy.state()).unwrap();
                add_reloaded_model(&home);
                let (event_tx, mut event_rx) = mpsc::unbounded_channel();
                let (_command_tx, mut command_rx) = mpsc::unbounded_channel();
                let cancellation = CancellationToken::new();
                let plugin_roots = std::collections::HashMap::new();
                let executor = ToolBatchExecutor {
                    workspace: workspace.path(),
                    home: &home,
                    config_path: &options.config_path,
                    session: &session,
                    capabilities: registry,
                    config: Arc::new(load_config(&options.config_path).unwrap()),
                    file_editor_tool: "patch",
                    events: &event_tx,
                    cancellation: &cancellation,
                    full_access: false,
                    workflows: Arc::new(WorkflowTasks::default()),
                    plugin_roots: &plugin_roots,
                };
                let (results, reloaded) = execute_tool_calls(
                    vec![ToolCall {
                        call_id: "reload".into(),
                        name: "reload_config".into(),
                        arguments: serde_json::json!({}),
                        execution: ToolExecution::Direct,
                    }],
                    &executor,
                    &phi_core::permissions::Permissions {
                        allow_shell: false,
                        allow_write: false,
                    },
                    false,
                    &mut command_rx,
                    &mut policy,
                    &Arc::new(phi_core::process::ShellSessions::default()),
                )
                .await
                .unwrap();

                assert!(reloaded);
                assert_eq!(results[0].result["reloaded"], true);
                let mut updated = false;
                while let Ok(event) = event_rx.try_recv() {
                    updated |= matches!(
                        event,
                        RuntimeEvent::CatalogUpdated { catalog }
                            if catalog.models.iter().any(|model| model.id == "openrouter/example/reloaded")
                    );
                }
                assert!(updated);
            })
            .await;
    }

    #[test]
    fn migrates_legacy_main_into_the_single_scheme_config() {
        let root = tempfile::tempdir().unwrap();
        let home = phi_core::home::PhiHome {
            root: root.path().join("home"),
        };
        std::fs::create_dir_all(&home.root).unwrap();
        std::fs::write(home.root.join("main.scm"), "(load-plugin! \"legacy\")\n").unwrap();

        initialize_at(&home).unwrap();

        let migrated = std::fs::read_to_string(home.scheme_config()).unwrap();
        assert!(migrated.contains("(define (on-event"));
        assert!(migrated.ends_with("(load-plugin! \"legacy\")\n"));
    }

    #[test]
    fn refreshes_versioned_builtins_without_overwriting_user_config() {
        let root = tempfile::tempdir().unwrap();
        let home = phi_core::home::PhiHome {
            root: root.path().join("home"),
        };
        initialize_at(&home).unwrap();
        std::fs::write(home.builtins().join("plugins/responses/main.scm"), "stale").unwrap();
        std::fs::write(home.scheme_config(), "user composition").unwrap();
        initialize_at(&home).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.builtins().join("plugins/responses/main.scm")).unwrap(),
            std::fs::read_to_string(repository_root().join("policy/providers/responses.scm"))
                .unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(
                home.builtins()
                    .join("plugins/compaction-structured/main.scm")
            )
            .unwrap(),
            std::fs::read_to_string(repository_root().join("policy/compaction/structured.scm"))
                .unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(home.scheme_config()).unwrap(),
            "user composition"
        );
        assert_eq!(
            std::fs::read_to_string(home.builtin_skills().join("phi-harness/SKILL.md")).unwrap(),
            PHI_HARNESS_SKILL[0].1
        );
    }
}
