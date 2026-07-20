use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use include_dir::{Dir, DirEntry, include_dir};
use phi_protocol::{
    Effect, Event, ManagedProcessAction, StreamRule, ToolCall, ToolExecution, ToolResult,
    WorkflowAction,
};
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod observability;
mod workflow;

pub use observability::Observability;
pub use workflow::WorkflowTasks;

pub use phi_core::process::ShellSessions as ProcessManager;
pub use phi_protocol::{CommandInvocation, CommandSpec, ModelSpec, PickerOptionSpec};

macro_rules! observe {
    ($observer:expr, $event:expr, $level:expr, $fields:expr $(,)?) => {
        if let Some(observer) = $observer {
            observer.record($event, $level, $fields);
        }
    };
}

#[derive(Clone)]
pub struct RunOptions {
    pub workspace: PathBuf,
    pub config_path: PathBuf,
    pub session_id: Option<String>,
    pub allow_shell: bool,
    pub allow_write: bool,
    pub interactive_approvals: bool,
    pub full_access: bool,
    pub workspace_only: bool,
    pub processes: Arc<phi_core::process::ShellSessions>,
    pub workflows: Arc<WorkflowTasks>,
    pub output_schema: Option<serde_json::Value>,
    pub observability: Option<Observability>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Session {
        id: String,
    },
    History {
        messages: Vec<serde_json::Value>,
    },
    UserMessage {
        content: String,
    },
    QueuedMessagesInjected {
        contents: Vec<String>,
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
    ContextCompactionStatus {
        job_id: String,
        status: String,
    },
    ToolRouteSelected {
        capability: String,
        implementation: String,
    },
    ModelDelta {
        content: String,
    },
    CommentaryDelta {
        content: String,
    },
    CommentaryStarted,
    ReasoningSummaryDelta {
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
        detail: String,
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
    steering: mpsc::UnboundedSender<Vec<String>>,
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

    pub fn queue_messages(&self, messages: Vec<String>) {
        let _ = self.steering.send(messages);
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
static BUNDLED_PLUGINS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../plugins");
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
const PLANNING_SKILL: &[(&str, &str)] = &[(
    "SKILL.md",
    include_str!("../../../skills/planning/SKILL.md"),
)];

pub fn initialize_home() -> Result<phi_core::home::PhiHome> {
    let home = phi_core::home::PhiHome::discover()?;
    initialize_at(&home)?;
    Ok(home)
}

pub fn initialize_at(home: &phi_core::home::PhiHome) -> Result<()> {
    std::fs::create_dir_all(&home.root)?;
    let initialization_lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(home.root.join(".initialize.lock"))?;
    initialization_lock.lock_exclusive()?;
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
    let official = phi_core::plugin::official_catalog()?;
    initialize_builtins(home, &official)?;
    Ok(())
}

fn initialize_builtins(
    home: &phi_core::home::PhiHome,
    official: &phi_core::plugin::OfficialPluginCatalog,
) -> Result<()> {
    let version_root = home.builtin_version_root();
    std::fs::create_dir_all(&version_root)?;
    remove_stale_staging_directories(&version_root)?;

    let snapshot_id = bundled_snapshot_id(official)?;
    let target = version_root.join(&snapshot_id);
    let backup = version_root.join(format!(".backup-{snapshot_id}"));
    if std::fs::symlink_metadata(&target).is_err() && is_real_directory(&backup)? {
        std::fs::rename(&backup, &target)?;
    } else if is_real_directory(&backup)? {
        std::fs::remove_dir_all(&backup)?;
    }

    if !bundled_snapshot_matches(&target, &snapshot_id, official)? {
        let staged = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(&version_root)?;
        write_bundled_snapshot(staged.path(), &snapshot_id, official)?;
        let staged = staged.keep();
        if std::fs::symlink_metadata(&target).is_ok() {
            std::fs::rename(&target, &backup)?;
        }
        if let Err(error) = std::fs::rename(&staged, &target) {
            if backup.exists() {
                let _ = std::fs::rename(&backup, &target);
            }
            let _ = std::fs::remove_dir_all(&staged);
            return Err(error.into());
        }
        if backup.exists() {
            std::fs::remove_dir_all(&backup)?;
        }
    }

    phi_core::write_atomic(
        &version_root.join("current"),
        snapshot_id.as_bytes(),
        phi_core::AtomicWriteMode::Overwrite,
    )?;
    Ok(())
}

fn is_real_directory(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn remove_stale_staging_directories(version_root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(version_root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry.file_name().to_string_lossy().starts_with(".staging-")
        {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn bundled_snapshot_id(official: &phi_core::plugin::OfficialPluginCatalog) -> Result<String> {
    fn collect<'a>(directory: &'a Dir<'a>, files: &mut Vec<(&'a Path, &'a [u8])>) {
        for entry in directory.entries() {
            match entry {
                DirEntry::Dir(directory) => collect(directory, files),
                DirEntry::File(file) => files.push((file.path(), file.contents())),
            }
        }
    }

    let mut files = Vec::new();
    collect(&BUNDLED_PLUGINS, &mut files);
    files.sort_by(|left, right| left.0.cmp(right.0));
    let mut hasher = blake3::Hasher::new();
    for (path, content) in files {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        hasher.update(content);
        hasher.update(&[0]);
    }
    for (relative, content) in PHI_HARNESS_SKILL.iter().chain(PLANNING_SKILL) {
        hasher.update(relative.as_bytes());
        hasher.update(&[0]);
        hasher.update(content.as_bytes());
        hasher.update(&[0]);
    }
    hasher.update(&serde_json::to_vec(official)?);
    hasher.update(env!("PHI_BUILD_COMMIT").as_bytes());
    Ok(format!("snapshot-{}", hasher.finalize().to_hex()))
}

fn write_bundled_snapshot(
    target: &Path,
    snapshot_id: &str,
    official: &phi_core::plugin::OfficialPluginCatalog,
) -> Result<()> {
    for plugin in &official.plugins {
        copy_official_plugin(target, &plugin.name, &plugin.path)?;
    }
    let commit = env!("PHI_BUILD_COMMIT");
    if !commit.is_empty() {
        write_bundled(
            &target.join("official-plugins-state.json"),
            &serde_json::to_string_pretty(&phi_core::plugin::OfficialPluginState {
                commit: commit.into(),
            })?,
        )?;
    }
    for (relative, content) in PHI_HARNESS_SKILL {
        write_bundled(&target.join("skills/phi-harness").join(relative), content)?;
    }
    for (relative, content) in PLANNING_SKILL {
        write_bundled(&target.join("skills/planning").join(relative), content)?;
    }
    std::fs::write(target.join(".complete"), snapshot_id)?;
    Ok(())
}

fn bundled_snapshot_matches(
    target: &Path,
    snapshot_id: &str,
    official: &phi_core::plugin::OfficialPluginCatalog,
) -> Result<bool> {
    if !is_real_directory(target)? {
        return Ok(false);
    }
    if !matches!(
        std::fs::read_to_string(target.join(".complete")),
        Ok(content) if content == snapshot_id
    ) {
        return Ok(false);
    }
    for plugin in &official.plugins {
        let relative = plugin.path.strip_prefix("plugins/").unwrap_or(&plugin.path);
        let Some(source) = BUNDLED_PLUGINS.get_dir(relative) else {
            bail!("official plugin package is missing: plugins/{relative}");
        };
        if !bundled_dir_matches(source, &target.join("plugins").join(&plugin.name))? {
            return Ok(false);
        }
        if phi_core::plugin::validate_package(&target.join("plugins").join(&plugin.name)).is_err() {
            return Ok(false);
        }
    }
    for (relative, content) in PHI_HARNESS_SKILL {
        if !matches!(
            std::fs::read_to_string(target.join("skills/phi-harness").join(relative)),
            Ok(actual) if actual == *content
        ) {
            return Ok(false);
        }
    }
    for (relative, content) in PLANNING_SKILL {
        if !matches!(
            std::fs::read_to_string(target.join("skills/planning").join(relative)),
            Ok(actual) if actual == *content
        ) {
            return Ok(false);
        }
    }
    let commit = env!("PHI_BUILD_COMMIT");
    if !commit.is_empty() {
        let state = std::fs::read(target.join("official-plugins-state.json"))
            .ok()
            .and_then(|content| {
                serde_json::from_slice::<phi_core::plugin::OfficialPluginState>(&content).ok()
            });
        if state.is_none_or(|state| state.commit != commit) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn bundled_dir_matches(source: &Dir<'_>, target: &Path) -> Result<bool> {
    fn matches(base: &Path, directory: &Dir<'_>, target: &Path) -> Result<bool> {
        for entry in directory.entries() {
            let relative = entry.path().strip_prefix(base)?;
            let path = target.join(relative);
            match entry {
                DirEntry::Dir(directory) => {
                    if !path.is_dir() || !matches(base, directory, target)? {
                        return Ok(false);
                    }
                }
                DirEntry::File(file) => {
                    if !matches!(std::fs::read(path), Ok(actual) if actual == file.contents()) {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }
    Ok(is_real_directory(target)? && matches(source.path(), source, target)?)
}

fn copy_official_plugin(root: &Path, name: &str, relative: &str) -> Result<()> {
    let relative = relative.strip_prefix("plugins/").unwrap_or(relative);
    let plugin = root.join("plugins").join(name);
    let source = BUNDLED_PLUGINS
        .get_dir(relative)
        .with_context(|| format!("official plugin package is missing: plugins/{relative}"))?;
    extract_bundled_dir(source, &plugin)?;
    phi_core::plugin::validate_package(&plugin)?;
    Ok(())
}

fn extract_bundled_dir(source: &Dir<'_>, target: &Path) -> Result<()> {
    fn extract(base: &Path, directory: &Dir<'_>, target: &Path) -> Result<()> {
        for entry in directory.entries() {
            let relative = entry.path().strip_prefix(base)?;
            let path = target.join(relative);
            match entry {
                DirEntry::Dir(directory) => {
                    std::fs::create_dir_all(&path)?;
                    extract(base, directory, target)?;
                }
                DirEntry::File(file) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(path, file.contents())?;
                }
            }
        }
        Ok(())
    }

    std::fs::create_dir_all(target)?;
    extract(source.path(), source, target)
}

fn write_bundled(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

fn write_if_missing(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(content.as_bytes())?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error.into()),
    }
}

fn initialize_scheme_config(home: &phi_core::home::PhiHome) -> Result<()> {
    write_if_missing(&home.scheme_config(), DEFAULT_SCHEME_CONFIG)
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
    let official = phi_core::plugin::official_catalog()?;
    let mut available = std::collections::BTreeMap::new();
    for plugin in &official.plugins {
        available.insert(
            plugin.name.clone(),
            home.builtins().join("plugins").join(&plugin.name),
        );
    }
    for plugin in &lock.plugins {
        available.insert(
            plugin.name.clone(),
            phi_core::plugin::install_root(home, &plugin.name, &plugin.commit),
        );
    }
    let mut skill_plugins = Vec::new();
    for (name, root) in &available {
        let entrypoint = phi_core::plugin::validate_package(root)
            .with_context(|| format!("validate plugin package: {name}"))?;
        skill_plugins.push(phi_core::session::PluginSource {
            name: name.clone(),
            root: root.clone(),
            entrypoint,
        });
    }
    let mut plugins = Vec::new();
    for name in phi_steel::composition_plugins(&config)? {
        let root = available
            .get(&name)
            .with_context(|| format!("plugin is not installed: {name}"))?
            .clone();
        let entrypoint = phi_core::plugin::validate_package(&root)?;
        plugins.push(phi_core::session::PluginSource {
            name,
            root,
            entrypoint,
        });
    }
    Ok(phi_core::session::ComposedSources {
        config,
        plugins,
        skill_plugins,
    })
}

fn entrypoints(sources: &phi_core::session::ComposedSources) -> Vec<PathBuf> {
    sources
        .plugins
        .iter()
        .map(|plugin| plugin.entrypoint.clone())
        .collect()
}

fn plugin_skill_sources(
    sources: &phi_core::session::ComposedSources,
) -> Vec<phi_core::skill::PluginSkillSource> {
    sources
        .skill_plugins
        .iter()
        .map(|plugin| phi_core::skill::PluginSkillSource {
            plugin: plugin.name.clone(),
            root: plugin.root.clone(),
        })
        .collect()
}

struct ResolvedPolicySources {
    sources: phi_core::session::ComposedSources,
    saved_state: Option<String>,
}

struct SessionBootstrap {
    session: phi_core::session::Session,
    sources: phi_core::session::ComposedSources,
    saved_state: Option<String>,
}

struct PolicyBootstrap {
    policy: phi_steel::Policy,
    capabilities: Arc<phi_core::capability::Registry>,
}

fn resolve_policy_sources(
    home: &phi_core::home::PhiHome,
    workspace: &Path,
    session_id: Option<&str>,
) -> Result<ResolvedPolicySources> {
    match session_id {
        Some(id) => {
            let session = phi_core::session::Session::open(&home.sessions(), id)?;
            let sources = session
                .composed_sources()?
                .context("session has no composition snapshot")?;
            Ok(ResolvedPolicySources {
                sources,
                saved_state: Some(session.load_state()?),
            })
        }
        None => Ok(ResolvedPolicySources {
            sources: resolve_sources(home, workspace)?,
            saved_state: None,
        }),
    }
}

fn resolve_session(
    home: &phi_core::home::PhiHome,
    workspace: &Path,
    session_id: Option<&str>,
) -> Result<SessionBootstrap> {
    let sessions = home.sessions();
    match session_id {
        Some(id) => {
            let session = phi_core::session::Session::open(&sessions, id)?;
            let sources = session
                .composed_sources()?
                .context("session has no composition snapshot")?;
            let saved_state = Some(session.load_state()?);
            Ok(SessionBootstrap {
                session,
                sources,
                saved_state,
            })
        }
        None => {
            let current = resolve_sources(home, workspace)?;
            let session = phi_core::session::Session::create_composed_with_metadata(
                &sessions,
                &current.config,
                &current.plugins,
                &current.skill_plugins,
                &phi_core::session::SessionMetadata {
                    workspace: Some(workspace.to_owned()),
                    ..Default::default()
                },
            )?;
            let sources = session
                .composed_sources()?
                .context("missing composition snapshot")?;
            Ok(SessionBootstrap {
                session,
                sources,
                saved_state: None,
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_policy(
    home: &phi_core::home::PhiHome,
    workspace: &Path,
    sources: &phi_core::session::ComposedSources,
    saved_state: Option<String>,
    full_access: bool,
    workspace_only: bool,
    session_id: &str,
    output_schema: Option<&serde_json::Value>,
) -> Result<PolicyBootstrap> {
    let plugin_skills = plugin_skill_sources(sources);
    let skills = discover_skills_with_plugins(home, workspace, &plugin_skills)?;
    let capabilities = Arc::new(capabilities_for_skills(
        home,
        full_access,
        workspace_only,
        &skills,
        sources.plugins.iter().any(|plugin| plugin.name == "skills"),
        Some(&home.sessions().join(session_id)),
    ));
    let plugin_roots = sources
        .plugins
        .iter()
        .map(|plugin| (plugin.name.clone(), plugin.root.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    let workflow_help = workflow::discovery_help(workspace, &home.root, &plugin_roots);
    let policy = phi_steel::Policy::load_with_state(
        &sources.config,
        &entrypoints(sources),
        &policy_config_with_schema_and_workflows(
            &capabilities,
            session_id,
            &load_user_state(home)?,
            &skills.skills,
            output_schema,
            &workflow_help,
        ),
        saved_state,
    )?;
    Ok(PolicyBootstrap {
        policy,
        capabilities,
    })
}

#[allow(clippy::too_many_arguments)]
fn reload_composition(
    home: &phi_core::home::PhiHome,
    config_path: &Path,
    workspace: &Path,
    session: &phi_core::session::Session,
    state: Option<String>,
    full_access: bool,
    workspace_only: bool,
    output_schema: Option<&serde_json::Value>,
) -> Result<(PolicyBootstrap, CommandCatalog)> {
    let _config = load_config(config_path)?;
    let sources = resolve_sources(home, workspace)?;
    let mut bootstrap = build_policy(
        home,
        workspace,
        &sources,
        state,
        full_access,
        workspace_only,
        session.id(),
        output_schema,
    )?;
    let catalog = catalog(&mut bootstrap.policy)?;
    session.replace_composition(&sources.config, &sources.plugins, &sources.skill_plugins)?;
    let pinned = session
        .composed_sources()?
        .context("session has no composition snapshot after reload")?;
    bootstrap = build_policy(
        home,
        workspace,
        &pinned,
        Some(bootstrap.policy.state().to_owned()),
        full_access,
        workspace_only,
        session.id(),
        output_schema,
    )?;
    Ok((bootstrap, catalog))
}

pub fn check_scheme_config(home: &phi_core::home::PhiHome, workspace: &Path) -> Result<()> {
    let sources = resolve_sources(home, workspace)?;
    phi_steel::check(&sources.config, &entrypoints(&sources))?;
    phi_steel::replay_smoke(&sources.config, &entrypoints(&sources))
}

pub fn create_session_with_id(
    options: &RunOptions,
    id: &str,
    metadata: phi_core::session::SessionMetadata,
) -> Result<()> {
    let workspace = options.workspace.canonicalize()?;
    let _config = load_config(&options.config_path)?;
    let home = home_for_config(&options.config_path)?;
    let current = resolve_sources(&home, &workspace)?;
    let session = phi_core::session::Session::create_composed_with_id(
        &home.sessions(),
        id,
        &current.config,
        &current.plugins,
        &current.skill_plugins,
        &metadata,
    )?;
    let bootstrap = build_policy(
        &home,
        &workspace,
        &current,
        None,
        options.full_access,
        options.workspace_only,
        session.id(),
        options.output_schema.as_ref(),
    )?;
    session.save_state(bootstrap.policy.state())
}

pub fn create_workflow_child_session_with_id(
    options: &RunOptions,
    id: &str,
    parent_session_id: &str,
    metadata: phi_core::session::SessionMetadata,
    model: &str,
    reasoning: &str,
    service_tier: &str,
) -> Result<()> {
    let workspace = options.workspace.canonicalize()?;
    let _config = load_config(&options.config_path)?;
    let home = home_for_config(&options.config_path)?;
    let parent = phi_core::session::Session::open(&home.sessions(), parent_session_id)?;
    let sources = parent
        .composed_sources()?
        .context("workflow parent session has no composition snapshot")?;
    let mut validation = build_policy(
        &home,
        &workspace,
        &sources,
        Some(parent.load_state()?),
        options.full_access,
        options.workspace_only,
        parent.id(),
        None,
    )?;
    let models = validation.policy.models()?;
    let selected = models
        .iter()
        .find(|candidate| candidate.id == model)
        .with_context(|| format!("unknown workflow agent model: {model}"))?;
    if !selected.reasoning.is_empty()
        && !selected
            .reasoning
            .iter()
            .any(|option| option.id() == reasoning)
    {
        bail!("unsupported workflow agent reasoning for {model}: {reasoning}");
    }
    if !selected.service_tiers.is_empty()
        && !selected
            .service_tiers
            .iter()
            .any(|option| option.id() == service_tier)
    {
        bail!("unsupported workflow agent service tier for {model}: {service_tier}");
    }

    let mut bootstrap = build_policy(
        &home,
        &workspace,
        &sources,
        None,
        options.full_access,
        options.workspace_only,
        id,
        options.output_schema.as_ref(),
    )?;
    bootstrap.policy.on_event(&Event::ModelSelected {
        model: model.to_owned(),
        reasoning: reasoning.to_owned(),
        service_tier: service_tier.to_owned(),
    })?;
    let session = phi_core::session::Session::create_composed_with_id(
        &home.sessions(),
        id,
        &sources.config,
        &sources.plugins,
        &sources.skill_plugins,
        &metadata,
    )?;
    session.save_state(bootstrap.policy.state())
}

fn load_user_state(home: &phi_core::home::PhiHome) -> Result<UserState> {
    if !home.state().is_file() {
        return Ok(UserState::default());
    }
    serde_json::from_slice(&std::fs::read(home.state())?).context("read user state")
}

fn save_user_state(home: &phi_core::home::PhiHome, state: &UserState) -> Result<()> {
    phi_core::write_json_atomic(&home.state(), state, phi_core::AtomicWriteMode::Overwrite)
}

pub fn command_catalog(options: &RunOptions) -> Result<CommandCatalog> {
    let workspace = options.workspace.canonicalize()?;
    let _config = load_config(&options.config_path)?;
    let home = home_for_config(&options.config_path)?;
    let resolved = resolve_policy_sources(&home, &workspace, options.session_id.as_deref())?;
    let mut bootstrap = build_policy(
        &home,
        &workspace,
        &resolved.sources,
        resolved.saved_state,
        options.full_access,
        options.workspace_only,
        options.session_id.as_deref().unwrap_or("catalog"),
        None,
    )?;
    catalog(&mut bootstrap.policy)
}

pub fn harness_status(options: &RunOptions) -> Result<HarnessStatus> {
    let workspace = options.workspace.canonicalize()?;
    let _config = load_config(&options.config_path)?;
    let home = home_for_config(&options.config_path)?;
    let resolved = resolve_policy_sources(&home, &workspace, None)?;
    let user_state = load_user_state(&home)?;
    let mut bootstrap = build_policy(
        &home,
        &workspace,
        &resolved.sources,
        resolved.saved_state,
        options.full_access,
        options.workspace_only,
        "status",
        None,
    )?;
    let composition = bootstrap.policy.composition_status()?;
    let lock = phi_core::plugin::read_lock(&home)?;
    let plugins = resolved
        .sources
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
            path: resolved.sources.config.display().to_string(),
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
    if invocation.name == "new" {
        if !invocation.arguments.is_empty() {
            bail!("usage: /new");
        }
        let resolved = resolve_session(&home, &workspace, None)?;
        let mut bootstrap = build_policy(
            &home,
            &workspace,
            &resolved.sources,
            resolved.saved_state,
            options.full_access,
            options.workspace_only,
            resolved.session.id(),
            None,
        )?;
        resolved.session.save_state(bootstrap.policy.state())?;
        return Ok(CommandExecution {
            session_id: resolved.session.id().into(),
            content: "Started a new chat.".into(),
            role: "note".into(),
            catalog: catalog(&mut bootstrap.policy)?,
            action: CommandAction::NewSession,
        });
    }
    let resolved = resolve_session(&home, &workspace, options.session_id.as_deref())?;
    if invocation.name == "reload" {
        if !invocation.arguments.is_empty() {
            bail!("usage: /reload");
        }
        let (mut bootstrap, reloaded_catalog) = reload_composition(
            &home,
            &options.config_path,
            &workspace,
            &resolved.session,
            resolved.saved_state,
            options.full_access,
            options.workspace_only,
            None,
        )?;
        resolved.session.save_state(bootstrap.policy.state())?;
        return Ok(CommandExecution {
            session_id: resolved.session.id().into(),
            content: format!(
                "Reloaded configuration · {} models · {} commands",
                reloaded_catalog.models.len(),
                reloaded_catalog.commands.len()
            ),
            role: "note".into(),
            catalog: catalog(&mut bootstrap.policy)?,
            action: CommandAction::Display,
        });
    }
    if invocation.name == "update-plugins" {
        if !invocation.arguments.is_empty() {
            bail!("usage: /update-plugins");
        }
        let updated = phi_core::plugin::update_all(&home)?;
        check_scheme_config(&home, &workspace)?;
        let mut bootstrap = build_policy(
            &home,
            &workspace,
            &resolved.sources,
            resolved.saved_state,
            options.full_access,
            options.workspace_only,
            resolved.session.id(),
            None,
        )?;
        resolved.session.save_state(bootstrap.policy.state())?;
        return Ok(CommandExecution {
            session_id: resolved.session.id().into(),
            content: format!(
                "Updated {} plugins. Run /reload to use them in this conversation.",
                updated.len()
            ),
            role: "note".into(),
            catalog: catalog(&mut bootstrap.policy)?,
            action: CommandAction::Display,
        });
    }
    let mut bootstrap = build_policy(
        &home,
        &workspace,
        &resolved.sources,
        resolved.saved_state,
        options.full_access,
        options.workspace_only,
        resolved.session.id(),
        None,
    )?;
    let policy = &mut bootstrap.policy;
    let initial_catalog = catalog(policy)?;
    let role = if invocation.name == "ps" {
        "processes"
    } else {
        "note"
    };
    let content = match invocation.name.as_str() {
        "help" => help(&initial_catalog),
        "keys" => {
            if !invocation.arguments.is_empty() {
                bail!("usage: /keys");
            }
            "Run /keys in the interactive TUI to see keybindings and detailed token usage.".into()
        }
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
            &resolved.session,
            policy,
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
    resolved.session.save_state(policy.state())?;
    Ok(CommandExecution {
        session_id: resolved.session.id().into(),
        content,
        role: role.into(),
        catalog: catalog(policy)?,
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
            name: "keys".into(),
            usage: "/keys".into(),
            description: "Show TUI keybindings and detailed token usage.".into(),
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
        CommandSpec {
            name: "update-plugins".into(),
            usage: "/update-plugins".into(),
            description: "Update official and installed plugins.".into(),
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

pub fn plugin_update_notice(options: &RunOptions) -> Option<String> {
    let home = home_for_config(&options.config_path).ok()?;
    match phi_core::plugin::check_updates(&home) {
        Ok(report) if !report.updates.is_empty() => Some(format!(
            "{} plugin update{} available. Run /update-plugins.",
            report.updates.len(),
            if report.updates.len() == 1 {
                " is"
            } else {
                "s are"
            }
        )),
        Ok(report) if !report.warnings.is_empty() => Some(format!(
            "Plugin update check could not reach {} source{}.",
            report.warnings.len(),
            if report.warnings.len() == 1 { "" } else { "s" }
        )),
        _ => None,
    }
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

#[cfg(test)]
fn policy_config(
    capabilities: &phi_core::capability::Registry,
    session_id: &str,
    user_state: &UserState,
    skills: &[phi_core::skill::SkillSpec],
) -> String {
    policy_config_with_schema_and_workflows(capabilities, session_id, user_state, skills, None, "")
}

fn policy_config_with_schema_and_workflows(
    capabilities: &phi_core::capability::Registry,
    session_id: &str,
    user_state: &UserState,
    skills: &[phi_core::skill::SkillSpec],
    output_schema: Option<&serde_json::Value>,
    workflow_help: &str,
) -> String {
    let mut tools = capabilities.specs();
    tools.push(phi_core::capability::exec_command_spec());
    tools.push(phi_core::capability::list_processes_spec());
    tools.push(phi_core::capability::reload_config_spec());
    tools.push(phi_core::capability::terminate_process_spec());
    tools.push(phi_core::capability::write_stdin_spec());
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let mut tool_routes = capabilities
        .specs()
        .into_iter()
        .map(|tool| serde_json::json!({ "name": tool.name, "mode": "capability" }))
        .collect::<Vec<_>>();
    tool_routes.extend([
        serde_json::json!({ "name": "exec_command", "mode": "managed_process", "action": "execute" }),
        serde_json::json!({ "name": "write_stdin", "mode": "managed_process", "action": "write_stdin" }),
        serde_json::json!({ "name": "list_processes", "mode": "managed_process", "action": "list" }),
        serde_json::json!({ "name": "terminate_process", "mode": "managed_process", "action": "terminate" }),
        serde_json::json!({ "name": "reload_config", "mode": "reload_config" }),
    ]);
    tool_routes.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    let mut value = serde_json::json!({
        "session_id": session_id,
        "skills": skills,
        "tools": tools,
        "tool_routes": tool_routes,
        "workflow_help": workflow_help,
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

#[cfg(test)]
fn capabilities(
    home: &phi_core::home::PhiHome,
    full_access: bool,
) -> phi_core::capability::Registry {
    let skills =
        phi_core::skill::discover(&home.builtin_skills(), &home.skills(), &home.root, &[]).unwrap();
    capabilities_for_skills(home, full_access, false, &skills, true, None)
}

fn capabilities_for_skills(
    home: &phi_core::home::PhiHome,
    full_access: bool,
    workspace_only: bool,
    skills: &phi_core::skill::SkillCatalog,
    expose_skills: bool,
    session_dir: Option<&Path>,
) -> phi_core::capability::Registry {
    let mut capabilities = phi_core::capability::Registry::default();
    capabilities.register(phi_core::capability::ReadFile {
        full_access,
        additional_root: (!workspace_only).then(|| home.root.clone()),
        resource_roots: skills.resource_roots(),
        resource_help: expose_skills
            .then(|| skill_resource_help(&skills.skills))
            .flatten(),
    });
    if let Some(session_dir) = session_dir {
        capabilities.register(phi_core::capability::CreatePlan {
            session_dir: session_dir.to_owned(),
        });
    }
    capabilities
}

fn skill_resource_help(skills: &[phi_core::skill::SkillSpec]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let entries = skills
        .iter()
        .map(|skill| {
            format!(
                "- {}: {} Read `{}`.",
                skill.name, skill.description, skill.path
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Load a relevant skill before acting by reading its listed SKILL.md resource. If the user writes $skill-name, read that skill before responding. Resolve referenced paths beneath the same skill://NAME/ prefix; reads remain contained within that skill. Available skills:\n{entries}"
    ))
}

#[cfg(test)]
fn discover_skills(
    home: &phi_core::home::PhiHome,
    workspace: &Path,
) -> Result<phi_core::skill::SkillCatalog> {
    discover_skills_with_plugins(home, workspace, &[])
}

fn discover_skills_with_plugins(
    home: &phi_core::home::PhiHome,
    workspace: &Path,
    plugin_sources: &[phi_core::skill::PluginSkillSource],
) -> Result<phi_core::skill::SkillCatalog> {
    phi_core::skill::discover(
        &home.builtin_skills(),
        &home.skills(),
        workspace,
        plugin_sources,
    )
}

pub fn start(options: RunOptions, prompt: String) -> Handle {
    start_events(options, vec![Event::UserMessage { content: prompt }])
}

pub fn start_messages(options: RunOptions, messages: Vec<String>) -> Handle {
    start_events(
        options,
        messages
            .into_iter()
            .map(|content| Event::UserMessage { content })
            .collect(),
    )
}

pub fn compact(options: RunOptions) -> Handle {
    start_events(options, vec![Event::CompactRequested])
}

fn start_events(options: RunOptions, initial_events: Vec<Event>) -> Handle {
    assert!(
        !initial_events.is_empty(),
        "a run requires an initial event"
    );
    let (event_tx, mut internal_event_rx) = mpsc::unbounded_channel();
    let (frontend_event_tx, event_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (steering_tx, steering_rx) = mpsc::unbounded_channel();
    let cancellation = CancellationToken::new();
    let run_cancellation = cancellation.clone();
    let event_observer = options.observability.clone();
    let run_observer = options.observability.clone();
    runtime_worker()
        .tasks
        .send(Box::new(move |local| {
            local.spawn_local(async move {
                if let Err(error) = run(
                    options,
                    initial_events,
                    &event_tx,
                    command_rx,
                    steering_rx,
                    &run_cancellation,
                )
                .await
                {
                    if let Some(observer) = &run_observer {
                        observer.record(
                            "runtime.failed",
                            "error",
                            serde_json::json!({ "error": runtime_error(&error) }),
                        );
                    }
                    let _ = event_tx.send(RuntimeEvent::Error {
                        message: runtime_error(&error),
                    });
                }
            });
            local.spawn_local(async move {
                while let Some(event) = internal_event_rx.recv().await {
                    if let Some(observer) = &event_observer {
                        observer.runtime_event(&event);
                    }
                    if frontend_event_tx.send(event).is_err() {
                        break;
                    }
                }
            });
        }))
        .expect("runtime worker stopped");
    Handle {
        events: event_rx,
        commands: command_tx,
        steering: steering_tx,
        cancellation,
    }
}

type RuntimeTask = Box<dyn FnOnce(&tokio::task::LocalSet) + Send>;

struct RuntimeWorker {
    tasks: mpsc::UnboundedSender<RuntimeTask>,
}

fn runtime_worker() -> &'static RuntimeWorker {
    static WORKER: std::sync::OnceLock<RuntimeWorker> = std::sync::OnceLock::new();
    WORKER.get_or_init(|| {
        let (tasks, mut pending) = mpsc::unbounded_channel::<RuntimeTask>();
        spawn_worker("phi-runtime", move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build runtime worker");
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, async {
                while let Some(task) = pending.recv().await {
                    task(&local);
                }
            });
        })
        .expect("failed to spawn runtime worker");
        RuntimeWorker { tasks }
    })
}

fn spawn_worker(
    name: &str,
    task: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(name.into()).spawn(task)
}

#[allow(clippy::while_let_loop)] // The final non-queue effect must remain available for dispatch.
async fn run(
    options: RunOptions,
    initial_events: Vec<Event>,
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    mut commands: mpsc::UnboundedReceiver<RuntimeCommand>,
    mut steering: mpsc::UnboundedReceiver<Vec<String>>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let workspace = options.workspace.canonicalize()?;
    let mut config = load_config(&options.config_path)?;
    let home = home_for_config(&options.config_path)?;
    let resolved = resolve_session(&home, &workspace, options.session_id.as_deref())?;
    let session = resolved.session;
    if let Some(observer) = &options.observability {
        observer.bind_session(session.id(), session.dir());
        observer.record("runtime.started", "info", serde_json::json!({}));
    }
    let mut plugin_roots = resolved
        .sources
        .plugins
        .iter()
        .map(|plugin| (plugin.name.clone(), plugin.root.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    send(
        events,
        RuntimeEvent::Session {
            id: session.id().into(),
        },
    )?;
    let bootstrap = build_policy(
        &home,
        &workspace,
        &resolved.sources,
        resolved.saved_state,
        options.full_access,
        options.workspace_only,
        session.id(),
        options.output_schema.as_ref(),
    )?;
    let mut capabilities = bootstrap.capabilities;
    let mut policy = bootstrap.policy;
    if options.session_id.is_some() {
        let state: serde_json::Value = serde_json::from_str(policy.state())?;
        send(
            events,
            RuntimeEvent::History {
                messages: state["messages"].as_array().cloned().unwrap_or_default(),
            },
        )?;
    }
    let mut context_statuses = context_job_statuses(policy.state())?;
    let mut legacy_file_editor_tool = policy.file_editor_tool_name()?;
    let selected_model = state_string(policy.state(), "model")?;
    let tool_routes = policy.resolved_tool_routes(&selected_model)?;
    for route in tool_routes {
        observe!(
            options.observability.as_ref(),
            "tool.route_selected",
            "info",
            serde_json::json!({
                "capability": route.capability,
                "implementation": route.implementation,
            }),
        );
        send(
            events,
            RuntimeEvent::ToolRouteSelected {
                capability: route.capability,
                implementation: route.implementation,
            },
        )?;
    }
    let mut pending_events = VecDeque::from(initial_events);
    let abandoned_jobs = pending_context_job_ids(policy.state())?;
    if !abandoned_jobs.is_empty() {
        let cancelled = Event::ContextCompactionsCancelled {
            job_ids: abandoned_jobs,
            reason: "session resumed without its background workers".into(),
        };
        if matches!(pending_events.front(), Some(Event::ToolsCompleted { .. })) {
            pending_events.insert(1, cancelled);
        } else {
            pending_events.push_front(cancelled);
        }
    }
    let mut event = pending_events
        .pop_front()
        .expect("initial events were checked above");
    let mut activity = "ready".to_owned();
    let permissions = phi_core::permissions::Permissions {
        allow_shell: options.allow_shell,
        allow_write: options.allow_write,
    };
    let shell_sessions = Arc::clone(&options.processes);
    let (context_tx, mut context_rx) = mpsc::unbounded_channel::<Event>();
    let context_cancellation = cancellation.child_token();

    macro_rules! cancellable_or_cancel_jobs {
        ($future:expr) => {
            match cancellable(cancellation, $future).await {
                Ok(value) => value,
                Err(error) => {
                    cancel_pending_context_compactions(
                        &mut policy,
                        &session,
                        events,
                        &mut context_statuses,
                        "agent turn cancelled",
                    )?;
                    return Err(error);
                }
            }
        };
    }

    loop {
        let settle_cancelled_tools =
            cancellation.is_cancelled() && matches!(&event, Event::ToolsCompleted { .. });
        if cancellation.is_cancelled() && !settle_cancelled_tools {
            observe!(
                options.observability.as_ref(),
                "runtime.cancelled",
                "warn",
                serde_json::json!({ "reason": "agent turn cancelled" }),
            );
            cancel_pending_context_compactions(
                &mut policy,
                &session,
                events,
                &mut context_statuses,
                "agent turn cancelled",
            )?;
            bail!("cancelled");
        }
        if let Event::UserMessage { content } = &event {
            send(
                events,
                RuntimeEvent::UserMessage {
                    content: content.clone(),
                },
            )?;
        }
        append_session(&session, &event, options.observability.as_ref(), "event")?;
        let previous_policy_state = policy.state().to_owned();
        observe!(
            options.observability.as_ref(),
            "policy.evaluation_started",
            "info",
            serde_json::json!({ "input": protocol_event_name(&event) }),
        );
        let mut output = match policy.on_event(&event) {
            Ok(output) => {
                observe!(
                    options.observability.as_ref(),
                    "policy.evaluation_completed",
                    "info",
                    serde_json::json!({ "effect_count": output.effects.len() }),
                );
                output
            }
            Err(error) => {
                observe!(
                    options.observability.as_ref(),
                    "policy.evaluation_failed",
                    "error",
                    serde_json::json!({ "error": runtime_error(&error) }),
                );
                return Err(error);
            }
        };
        emit_context_tool_events(events, &previous_policy_state, policy.state())?;
        append_session(
            &session,
            &output,
            options.observability.as_ref(),
            "policy_output",
        )?;
        session.save_state(policy.state())?;
        send_context(events, policy.state())?;
        report_context_job_statuses(events, policy.state(), &mut context_statuses)?;
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
        if cancellation.is_cancelled() {
            observe!(
                options.observability.as_ref(),
                "runtime.cancelled",
                "warn",
                serde_json::json!({ "reason": "agent turn cancelled" }),
            );
            cancel_pending_context_compactions(
                &mut policy,
                &session,
                events,
                &mut context_statuses,
                "agent turn cancelled",
            )?;
            bail!("cancelled");
        }
        if matches!(&event, Event::ToolsCompleted { .. }) {
            let mut queued = Vec::new();
            while let Ok(messages) = steering.try_recv() {
                queued.extend(messages);
            }
            if !queued.is_empty() {
                send(
                    events,
                    RuntimeEvent::QueuedMessagesInjected {
                        contents: queued.clone(),
                    },
                )?;
                pending_events.extend(
                    queued
                        .into_iter()
                        .map(|content| Event::UserMessage { content }),
                );
            }
        }
        if let Some(next_event) = pending_events.pop_front() {
            event = next_event;
            continue;
        }
        let stream_output = activity == "working";
        let mut effect = output
            .effects
            .into_iter()
            .next()
            .context("policy emitted no effect")?;
        loop {
            let Effect::QueueContextCompaction {
                job_id,
                url,
                secret,
                headers,
                body,
                timeout_ms,
                stream,
                next,
            } = effect
            else {
                break;
            };
            let started = Event::ContextCompactionStarted {
                job_id: job_id.clone(),
            };
            session.append(&started)?;
            let started_output = policy.on_event(&started)?;
            session.append(&started_output)?;
            session.save_state(policy.state())?;
            send_context(events, policy.state())?;
            report_context_job_statuses(events, policy.state(), &mut context_statuses)?;
            spawn_context_compaction(
                job_id,
                url,
                secret,
                headers,
                body,
                timeout_ms,
                stream,
                config.clone(),
                context_tx.clone(),
                context_cancellation.clone(),
            );
            effect = *next;
        }
        while let Ok(completed) = context_rx.try_recv() {
            session.append(&completed)?;
            output = policy.on_event(&completed)?;
            session.append(&output)?;
            session.save_state(policy.state())?;
            send_context(events, policy.state())?;
            report_context_job_statuses(events, policy.state(), &mut context_statuses)?;
            let mut completed_effect = output
                .effects
                .into_iter()
                .next()
                .context("policy emitted no effect")?;
            loop {
                let Effect::QueueContextCompaction {
                    job_id,
                    url,
                    secret,
                    headers,
                    body,
                    timeout_ms,
                    stream,
                    next,
                } = completed_effect
                else {
                    break;
                };
                spawn_context_compaction(
                    job_id,
                    url,
                    secret,
                    headers,
                    body,
                    timeout_ms,
                    stream,
                    config.clone(),
                    context_tx.clone(),
                    context_cancellation.clone(),
                );
                completed_effect = *next;
            }
            effect = merge_context_completion_effect(effect, completed_effect);
        }
        match effect {
            Effect::Process {
                program,
                args,
                stdin,
                timeout_ms,
            } => {
                observe!(
                    options.observability.as_ref(),
                    "process.started",
                    "info",
                    serde_json::json!({ "program": program, "argument_count": args.len() }),
                );
                let result = cancellable_or_cancel_jobs!(phi_core::process::run(
                    &workspace,
                    &config.allowed_programs,
                    &program,
                    &args,
                    &stdin,
                    timeout_ms,
                ));
                match result {
                    Ok(completed) => {
                        let (success, exit_code) = match &completed {
                            Event::ProcessCompleted {
                                success, exit_code, ..
                            } => (*success, *exit_code),
                            _ => (false, None),
                        };
                        observe!(
                            options.observability.as_ref(),
                            "process.completed",
                            if success { "info" } else { "error" },
                            serde_json::json!({ "program": program, "success": success, "exit_code": exit_code }),
                        );
                        event = completed;
                    }
                    Err(error) => {
                        observe!(
                            options.observability.as_ref(),
                            "process.failed",
                            "error",
                            serde_json::json!({ "program": program, "error": runtime_error(&error) }),
                        );
                        return Err(error);
                    }
                }
            }
            Effect::RunTools { calls } => {
                observe!(
                    options.observability.as_ref(),
                    "tools.dispatch_started",
                    "info",
                    serde_json::json!({ "tool_count": calls.len() }),
                );
                let executor = ToolBatchExecutor {
                    workspace: &workspace,
                    home: &home,
                    config_path: &options.config_path,
                    session: &session,
                    capabilities: Arc::clone(&capabilities),
                    config: Arc::new(config.clone()),
                    legacy_file_editor_tool: &legacy_file_editor_tool,
                    events,
                    cancellation,
                    allow_shell: options.allow_shell,
                    allow_write: options.allow_write,
                    interactive_approvals: options.interactive_approvals,
                    full_access: options.full_access,
                    workspace_only: options.workspace_only,
                    output_schema: options.output_schema.as_ref(),
                    workflows: Arc::clone(&options.workflows),
                    plugin_roots: &plugin_roots,
                    observability: options.observability.as_ref(),
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
                    let bootstrap = build_policy(
                        &home,
                        &workspace,
                        &sources,
                        Some(state),
                        options.full_access,
                        options.workspace_only,
                        session.id(),
                        options.output_schema.as_ref(),
                    )?;
                    capabilities = bootstrap.capabilities;
                    policy = bootstrap.policy;
                    legacy_file_editor_tool = policy.file_editor_tool_name()?;
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
                let mut output_phases = BTreeMap::new();
                let request = phi_core::http::post_sse_observed(
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
                            emit_stream_events(events, provider_event, &stream, &mut output_phases)
                        } else {
                            false
                        }
                    },
                    |observation| observe_http(options.observability.as_ref(), observation, None),
                );
                event = cancellable_or_cancel_jobs!(request)?;
            }
            Effect::Finish { content } => {
                context_cancellation.cancel();
                cancel_pending_context_compactions(
                    &mut policy,
                    &session,
                    events,
                    &mut context_statuses,
                    "agent turn finished",
                )?;
                send(events, RuntimeEvent::Finished { content })?;
                observe!(
                    options.observability.as_ref(),
                    "runtime.finished",
                    "info",
                    serde_json::json!({}),
                );
                return Ok(());
            }
            Effect::WaitForContextCompactions { call_id, job_ids } => {
                while job_ids.iter().any(|id| {
                    context_job_status(policy.state(), id)
                        .ok()
                        .flatten()
                        .is_some_and(|status| !is_terminal_context_status(&status))
                }) {
                    let completed = cancellable_or_cancel_jobs!(context_rx.recv())
                        .context("context compaction worker stopped")?;
                    session.append(&completed)?;
                    let mut completion_output = policy.on_event(&completed)?;
                    session.append(&completion_output)?;
                    session.save_state(policy.state())?;
                    send_context(events, policy.state())?;
                    report_context_job_statuses(events, policy.state(), &mut context_statuses)?;
                    if let Some(Effect::QueueContextCompaction {
                        job_id,
                        url,
                        secret,
                        headers,
                        body,
                        timeout_ms,
                        stream,
                        ..
                    }) = completion_output.effects.pop()
                    {
                        spawn_context_compaction(
                            job_id,
                            url,
                            secret,
                            headers,
                            body,
                            timeout_ms,
                            stream,
                            config.clone(),
                            context_tx.clone(),
                            context_cancellation.clone(),
                        );
                    }
                }
                event = Event::ContextWaitCompleted { call_id, job_ids };
            }
            Effect::Continue => {
                if pending_context_job_ids(policy.state())?.is_empty() {
                    bail!("policy requested continuation without pending work");
                }
                event = cancellable_or_cancel_jobs!(context_rx.recv())
                    .context("context compaction worker stopped")?;
            }
            Effect::QueueContextCompaction { .. } => {
                unreachable!("queued compactions are unwrapped before dispatch")
            }
        }
    }
}

fn emit_context_tool_events(
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    previous_state: &str,
    next_state: &str,
) -> Result<()> {
    let previous: serde_json::Value = serde_json::from_str(previous_state)?;
    let next: serde_json::Value = serde_json::from_str(next_state)?;
    let previous_messages = previous["messages"]
        .as_array()
        .context("missing messages")?;
    let next_messages = next["messages"].as_array().context("missing messages")?;
    let previous_context_events = previous_messages
        .iter()
        .filter_map(|message| context_tool_message_key(message, previous_messages))
        .collect::<HashSet<_>>();
    for message in next_messages {
        let Some(key) = context_tool_message_key(message, next_messages) else {
            continue;
        };
        if previous_context_events.contains(&key) {
            continue;
        }
        let call_id = message["call_id"].as_str().unwrap_or_default().to_owned();
        match message["kind"].as_str() {
            Some("tool_call") => {
                let name = message["name"].as_str().unwrap_or_default().to_owned();
                let arguments = message["arguments"]
                    .as_str()
                    .and_then(|arguments| serde_json::from_str(arguments).ok())
                    .unwrap_or_else(|| message["arguments"].clone());
                send(
                    events,
                    RuntimeEvent::ToolStarted {
                        call_id,
                        name,
                        arguments,
                    },
                )?;
            }
            Some("tool_result") => {
                let name = context_tool_name_for_call(next_messages, &call_id)
                    .unwrap_or_default()
                    .to_owned();
                let result = message["content"]
                    .as_str()
                    .and_then(|content| serde_json::from_str(content).ok())
                    .unwrap_or_else(|| message["content"].clone());
                send(
                    events,
                    RuntimeEvent::ToolCompleted {
                        call_id,
                        name,
                        result,
                    },
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn context_tool_message_key(
    message: &serde_json::Value,
    messages: &[serde_json::Value],
) -> Option<(String, String)> {
    let kind = message["kind"].as_str()?;
    if !matches!(kind, "tool_call" | "tool_result") {
        return None;
    }
    let call_id = message["call_id"].as_str()?;
    let name = if kind == "tool_call" {
        message["name"].as_str()
    } else {
        context_tool_name_for_call(messages, call_id)
    };
    if !matches!(
        name,
        Some("context_mark" | "context_inspect" | "context_compact" | "context_wait")
    ) {
        return None;
    }
    Some((kind.to_owned(), call_id.to_owned()))
}

fn context_tool_name_for_call<'a>(
    messages: &'a [serde_json::Value],
    call_id: &str,
) -> Option<&'a str> {
    messages
        .iter()
        .find(|candidate| {
            candidate["kind"] == "tool_call" && candidate["call_id"].as_str() == Some(call_id)
        })
        .and_then(|call| call["name"].as_str())
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
    reloaded: bool,
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

enum ToolAuthorization {
    Allow,
    Ask(String),
    Deny(String),
}

struct ToolBatchExecutor<'a> {
    workspace: &'a Path,
    home: &'a phi_core::home::PhiHome,
    config_path: &'a Path,
    session: &'a phi_core::session::Session,
    capabilities: Arc<phi_core::capability::Registry>,
    config: Arc<Config>,
    legacy_file_editor_tool: &'a str,
    events: &'a mpsc::UnboundedSender<RuntimeEvent>,
    cancellation: &'a CancellationToken,
    allow_shell: bool,
    allow_write: bool,
    interactive_approvals: bool,
    full_access: bool,
    workspace_only: bool,
    output_schema: Option<&'a serde_json::Value>,
    workflows: Arc<WorkflowTasks>,
    plugin_roots: &'a std::collections::HashMap<String, PathBuf>,
    observability: Option<&'a Observability>,
}

fn workflow_agent_context(
    policy: &mut phi_steel::Policy,
    executor: &ToolBatchExecutor<'_>,
) -> Result<workflow::WorkflowAgentContext> {
    let state: serde_json::Value = serde_json::from_str(policy.state())?;
    Ok(workflow::WorkflowAgentContext {
        models: policy.models()?,
        model: state["model"]
            .as_str()
            .filter(|value| !value.is_empty())
            .context("workflow parent has no selected model")?
            .to_owned(),
        reasoning: state["reasoning"].as_str().unwrap_or("").to_owned(),
        service_tier: state["service_tier"].as_str().unwrap_or("").to_owned(),
        allow_shell: executor.allow_shell,
        allow_write: executor.allow_write,
        full_access: executor.full_access,
        interactive_approvals: executor.interactive_approvals,
        workspace_only: executor.workspace_only,
    })
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
    for (index, mut call) in calls.into_iter().enumerate() {
        migrate_legacy_tool_execution(&mut call, executor.legacy_file_editor_tool);
        let parallel_safe = tool_call_parallel_safe(&call, &executor.capabilities);
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
        let authorization = authorize_tool_call(
            permissions,
            policy,
            &call.name,
            &call.arguments,
            executor.full_access,
        )?;
        let decision = match &authorization {
            ToolAuthorization::Allow => "allow",
            ToolAuthorization::Ask(_) => "ask",
            ToolAuthorization::Deny(_) => "deny",
        };
        observe!(
            executor.observability,
            "policy.tool_authorization",
            "info",
            serde_json::json!({
                "call_id": call.call_id,
                "tool": call.name,
                "decision": decision,
            }),
        );
        let (approved, denial) = match authorization {
            ToolAuthorization::Allow => (true, None),
            ToolAuthorization::Ask(detail) if interactive_approvals => {
                send(
                    executor.events,
                    RuntimeEvent::ApprovalRequested {
                        name: call.name.clone(),
                        detail,
                    },
                )?;
                let approved = matches!(
                    cancellable(executor.cancellation, commands.recv()).await?,
                    Some(RuntimeCommand::ApproveOnce)
                );
                (approved, (!approved).then(|| "approval denied".to_owned()))
            }
            ToolAuthorization::Ask(detail) => (
                false,
                Some(format!(
                    "approval required but no approval channel is available: {detail}"
                )),
            ),
            ToolAuthorization::Deny(detail) => (
                false,
                Some(format!("denied by tool approval policy: {detail}")),
            ),
        };
        if approved {
            observe!(
                executor.observability,
                "tool.execution_started",
                "info",
                serde_json::json!({ "call_id": call.call_id, "tool": call.name }),
            );
            let pending = PendingToolCall { index, call };
            if parallel_safe {
                parallel.push(pending);
            } else {
                let raw = execute_serial_call(executor, pending, policy, shell_sessions).await;
                reloaded |= finish_tool_call(
                    raw,
                    policy,
                    executor.events,
                    executor.observability,
                    &mut completed,
                )?;
            }
        } else {
            let result = serde_json::json!({
                "error": format!(
                    "{} {}",
                    call.name,
                    denial.as_deref().unwrap_or("approval denied")
                )
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

fn authorize_tool_call(
    permissions: &phi_core::permissions::Permissions,
    policy: &mut phi_steel::Policy,
    name: &str,
    arguments: &serde_json::Value,
    full_access: bool,
) -> Result<ToolAuthorization> {
    if full_access {
        return Ok(ToolAuthorization::Allow);
    }
    let fallback = if permissions.authorize_tool(name).is_ok()
        && !bundled_git_requires_approval(name, arguments)
    {
        phi_steel::ApprovalDecision::Allow
    } else {
        phi_steel::ApprovalDecision::Ask
    };
    let policy = policy.tool_approval(name, arguments)?;
    let detail = policy
        .as_ref()
        .map(|approval| approval.detail.trim())
        .filter(|detail| !detail.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| fallback_approval_detail(name, arguments));
    let decision = policy
        .map(|approval| stricter_approval(fallback, approval.decision))
        .unwrap_or(fallback);
    Ok(match decision {
        phi_steel::ApprovalDecision::Allow => ToolAuthorization::Allow,
        phi_steel::ApprovalDecision::Ask => ToolAuthorization::Ask(detail),
        phi_steel::ApprovalDecision::Deny => ToolAuthorization::Deny(detail),
    })
}

fn bundled_git_requires_approval(name: &str, arguments: &serde_json::Value) -> bool {
    if name != "exec_command" {
        return false;
    }
    let Some(command) = arguments.get("cmd").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let words = command.split_whitespace().collect::<Vec<_>>();
    if words.first() != Some(&"git") {
        return false;
    }
    let safe_operation = words.get(1).is_some_and(|operation| {
        matches!(
            *operation,
            "status"
                | "diff"
                | "log"
                | "show"
                | "rev-parse"
                | "ls-files"
                | "grep"
                | "blame"
                | "cat-file"
        )
    });
    let shell_metacharacter = command.chars().any(|character| {
        matches!(
            character,
            ';' | '|' | '&' | '>' | '<' | '\n' | '\r' | '`' | '$' | '\\' | '\'' | '"'
        )
    });
    !safe_operation || shell_metacharacter
}

fn stricter_approval(
    fallback: phi_steel::ApprovalDecision,
    policy: phi_steel::ApprovalDecision,
) -> phi_steel::ApprovalDecision {
    use phi_steel::ApprovalDecision::{Allow, Ask, Deny};
    match (fallback, policy) {
        (Deny, _) | (_, Deny) => Deny,
        (Ask, _) | (_, Ask) => Ask,
        (Allow, Allow) => Allow,
    }
}

fn fallback_approval_detail(name: &str, arguments: &serde_json::Value) -> String {
    let encoded = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into());
    let detail = format!("{name}: {encoded}");
    let mut characters = detail.chars();
    let truncated = characters.by_ref().take(159).collect::<String>();
    if characters.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn migrate_legacy_tool_execution(call: &mut ToolCall, file_editor_tool: &str) {
    if !matches!(call.execution, ToolExecution::LegacyDirect) {
        return;
    }
    call.execution = match call.name.as_str() {
        "exec_command" => ToolExecution::ManagedProcess {
            action: ManagedProcessAction::Execute,
        },
        "write_stdin" => ToolExecution::ManagedProcess {
            action: ManagedProcessAction::WriteStdin,
        },
        "list_processes" => ToolExecution::ManagedProcess {
            action: ManagedProcessAction::List,
        },
        "terminate_process" => ToolExecution::ManagedProcess {
            action: ManagedProcessAction::Terminate,
        },
        "Workflow" => ToolExecution::Workflow {
            action: WorkflowAction::Launch,
        },
        "TaskOutput" => ToolExecution::Workflow {
            action: WorkflowAction::Output,
        },
        "TaskStop" => ToolExecution::Workflow {
            action: WorkflowAction::Stop,
        },
        "reload_config" => ToolExecution::ReloadConfig,
        name if name == file_editor_tool => ToolExecution::FileEdit,
        _ => ToolExecution::Capability,
    };
}

fn tool_call_parallel_safe(call: &ToolCall, capabilities: &phi_core::capability::Registry) -> bool {
    match &call.execution {
        ToolExecution::Http { parallel, .. } => *parallel,
        ToolExecution::Capability => capabilities.parallel_safe(&call.name),
        ToolExecution::ManagedProcess {
            action: ManagedProcessAction::Execute,
        } => true,
        ToolExecution::ManagedProcess { .. }
        | ToolExecution::FileEdit
        | ToolExecution::Workflow { .. }
        | ToolExecution::ReloadConfig
        | ToolExecution::LegacyDirect => false,
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
        let _ = finish_tool_call(
            raw,
            policy,
            executor.events,
            executor.observability,
            completed,
        )?;
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
    let result = match execution {
        ToolExecution::Capability => {
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
        ToolExecution::ManagedProcess {
            action: ManagedProcessAction::Execute,
        } => {
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
        ToolExecution::ManagedProcess { .. }
        | ToolExecution::FileEdit
        | ToolExecution::Workflow { .. }
        | ToolExecution::ReloadConfig
        | ToolExecution::LegacyDirect => RawToolOutput::Value {
            result: serde_json::json!({ "error": "tool effect is not parallel-safe" }),
            display: None,
        },
    };
    RawToolResult {
        index,
        call_id,
        name,
        result,
        reloaded: false,
    }
}

async fn execute_serial_call(
    executor: &ToolBatchExecutor<'_>,
    pending: PendingToolCall,
    policy: &mut phi_steel::Policy,
    shell_sessions: &Arc<phi_core::process::ShellSessions>,
) -> RawToolResult {
    let PendingToolCall { index, call } = pending;
    let ToolCall {
        call_id,
        name,
        arguments,
        execution,
    } = call;
    let value = |result| RawToolOutput::Value {
        result,
        display: None,
    };
    let (result, reloaded) = match execution {
        ToolExecution::Workflow { action } => {
            observe!(
                executor.observability,
                "workflow.lifecycle",
                "info",
                serde_json::json!({
                    "call_id": call_id,
                    "task_id": arguments.get("task_id").and_then(serde_json::Value::as_str),
                    "action": format!("{action:?}").to_ascii_lowercase(),
                    "phase": "started",
                }),
            );
            let result = cancellable(executor.cancellation, async {
                match action {
                    WorkflowAction::Launch => match workflow_agent_context(policy, executor) {
                        Ok(agent_context) => {
                            executor
                                .workflows
                                .launch(
                                    executor.workspace,
                                    &executor.home.root,
                                    executor.session.id(),
                                    executor.session.dir(),
                                    executor.plugin_roots,
                                    agent_context,
                                    &arguments,
                                )
                                .await
                        }
                        Err(error) => Err(error),
                    },
                    WorkflowAction::Output => {
                        executor
                            .workflows
                            .output(executor.session.dir(), &arguments)
                            .await
                    }
                    WorkflowAction::Stop => {
                        executor
                            .workflows
                            .stop(executor.session.dir(), &arguments)
                            .await
                    }
                }
            })
            .await
            .and_then(|result| result)
            .unwrap_or_else(|error| serde_json::json!({ "error": runtime_error(&error) }));
            observe!(
                executor.observability,
                "workflow.lifecycle",
                if result.get("error").is_some() {
                    "error"
                } else {
                    "info"
                },
                serde_json::json!({
                    "call_id": call_id,
                    "task_id": result.get("task_id").and_then(serde_json::Value::as_str)
                        .or_else(|| arguments.get("task_id").and_then(serde_json::Value::as_str)),
                    "phase": "completed",
                    "status": result.get("status").and_then(serde_json::Value::as_str),
                    "error": result.get("error").and_then(serde_json::Value::as_str),
                }),
            );
            (value(result), false)
        }
        ToolExecution::ManagedProcess { action } => {
            let event_name = name.clone();
            let event_call_id = call_id.clone();
            let events = executor.events.clone();
            let result = cancellable(executor.cancellation, async {
                let emit = move |content: &str| {
                    let _ = events.send(RuntimeEvent::ToolOutput {
                        call_id: event_call_id.clone(),
                        name: event_name.clone(),
                        content: content.to_owned(),
                    });
                };
                match action {
                    ManagedProcessAction::Execute => {
                        shell_sessions
                            .exec_with_access(
                                executor.workspace,
                                &arguments,
                                executor.full_access,
                                emit,
                            )
                            .await
                    }
                    ManagedProcessAction::WriteStdin => {
                        shell_sessions.write_stdin(&arguments, emit).await
                    }
                    ManagedProcessAction::Terminate => shell_sessions.terminate(&arguments).await,
                    ManagedProcessAction::List => shell_sessions
                        .list()
                        .map(|processes| serde_json::json!({ "processes": processes })),
                }
            })
            .await
            .and_then(|result| result)
            .unwrap_or_else(|error| serde_json::json!({ "error": runtime_error(&error) }));
            (value(result), false)
        }
        ToolExecution::FileEdit => {
            let (result, display) = execute_file_edit(
                executor.workspace,
                &executor.home.root,
                policy,
                &name,
                &arguments,
                executor.full_access,
                executor.workspace_only,
            )
            .map(|(result, display)| (result, Some(display)))
            .unwrap_or_else(|error| (serde_json::json!({ "error": runtime_error(&error) }), None));
            (RawToolOutput::Value { result, display }, false)
        }
        ToolExecution::ReloadConfig => {
            let result = reload_composition(
                executor.home,
                executor.config_path,
                executor.workspace,
                executor.session,
                Some(policy.state().to_owned()),
                executor.full_access,
                executor.workspace_only,
                executor.output_schema,
            )
            .map(|(_, catalog)| {
                let _ = executor.events.send(RuntimeEvent::CatalogUpdated {
                    catalog: catalog.clone(),
                });
                serde_json::json!({
                    "reloaded": true,
                    "models": catalog.models.len(),
                    "commands": catalog.commands.len()
                })
            })
            .map(|result| (result, true))
            .unwrap_or_else(|error| (serde_json::json!({ "error": runtime_error(&error) }), false));
            (value(result.0), result.1)
        }
        execution @ (ToolExecution::Capability | ToolExecution::Http { .. }) => {
            return execute_parallel_call(
                PendingToolCall {
                    index,
                    call: ToolCall {
                        call_id,
                        name,
                        arguments,
                        execution,
                    },
                },
                executor.workspace.to_owned(),
                Arc::clone(&executor.capabilities),
                Arc::clone(&executor.config),
                Arc::clone(shell_sessions),
                executor.events.clone(),
                executor.full_access,
            )
            .await;
        }
        ToolExecution::LegacyDirect => {
            return RawToolResult {
                index,
                call_id,
                name,
                result: value(serde_json::json!({
                    "error": "legacy direct tool route was not migrated"
                })),
                reloaded: false,
            };
        }
    };
    RawToolResult {
        index,
        call_id,
        name,
        result,
        reloaded,
    }
}

fn finish_tool_call(
    raw: RawToolResult,
    policy: &mut phi_steel::Policy,
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    observability: Option<&Observability>,
    completed: &mut Vec<(usize, ToolResult)>,
) -> Result<bool> {
    let reloaded = raw.reloaded;
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
    observe!(
        observability,
        "tool.execution_completed",
        if result.get("error").is_some() {
            "error"
        } else {
            "info"
        },
        serde_json::json!({
            "call_id": raw.call_id,
            "tool": raw.name,
            "success": result.get("error").is_none(),
        }),
    );
    completed.push((
        raw.index,
        ToolResult {
            call_id: raw.call_id,
            name: raw.name,
            result,
        },
    ));
    Ok(reloaded)
}

fn emit_stream_events(
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    provider_event: &serde_json::Value,
    rules: &[StreamRule],
    phases: &mut BTreeMap<String, String>,
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
        let key = provider_event
            .pointer(&rule.key)
            .map(serde_json::Value::to_string)
            .unwrap_or_default();
        if rule.emit == "output_phase" {
            if let Some(phase) = value.as_str() {
                phases.insert(key, phase.to_owned());
                if phase == "commentary" {
                    let _ = events.send(RuntimeEvent::CommentaryStarted);
                }
            }
            continue;
        }
        let event = match rule.emit.as_str() {
            "output_delta" => value.as_str().map(|content| {
                if phases.get(&key).is_some_and(|phase| phase == "commentary") {
                    RuntimeEvent::CommentaryDelta {
                        content: content.into(),
                    }
                } else {
                    RuntimeEvent::ModelDelta {
                        content: content.into(),
                    }
                }
            }),
            "model_delta" => value.as_str().map(|content| RuntimeEvent::ModelDelta {
                content: content.into(),
            }),
            "reasoning_summary_delta" => {
                value
                    .as_str()
                    .map(|content| RuntimeEvent::ReasoningSummaryDelta {
                        content: content.into(),
                    })
            }
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

#[allow(clippy::too_many_arguments)]
fn spawn_context_compaction(
    job_id: String,
    url: String,
    secret: String,
    headers: BTreeMap<String, String>,
    body: serde_json::Value,
    timeout_ms: u64,
    _stream: Vec<StreamRule>,
    config: Config,
    completed: mpsc::UnboundedSender<Event>,
    cancellation: CancellationToken,
) {
    tokio::task::spawn_local(async move {
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
            |_| false,
        );
        let event = match cancellable(&cancellation, request).await {
            Ok(Ok(Event::HttpCompleted {
                success,
                status,
                events,
                error,
            })) => Event::ContextCompactionCompleted {
                job_id,
                success,
                status,
                events,
                error,
            },
            Ok(Ok(_)) => unreachable!("HTTP client returned a non-HTTP event"),
            Ok(Err(error)) | Err(error) => Event::ContextCompactionCompleted {
                job_id,
                success: false,
                status: 0,
                events: Vec::new(),
                error: runtime_error(&error),
            },
        };
        let _ = completed.send(event);
    });
}

fn pending_context_job_ids(state: &str) -> Result<Vec<String>> {
    let state: serde_json::Value = serde_json::from_str(state)?;
    Ok(state["context_jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|job| {
            job["status"]
                .as_str()
                .is_some_and(|status| !is_terminal_context_status(status))
        })
        .filter_map(|job| job["id"].as_str().map(str::to_owned))
        .collect())
}

fn context_job_status(state: &str, job_id: &str) -> Result<Option<String>> {
    let state: serde_json::Value = serde_json::from_str(state)?;
    Ok(state["context_jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|job| job["id"] == job_id)
        .and_then(|job| job["status"].as_str().map(str::to_owned)))
}

fn context_job_statuses(state: &str) -> Result<BTreeMap<String, String>> {
    let state: serde_json::Value = serde_json::from_str(state)?;
    Ok(state["context_jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|job| {
            Some((
                job["id"].as_str()?.to_owned(),
                job["status"].as_str()?.to_owned(),
            ))
        })
        .collect())
}

fn report_context_job_statuses(
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    state: &str,
    reported: &mut BTreeMap<String, String>,
) -> Result<()> {
    let current = context_job_statuses(state)?;
    for (job_id, status) in &current {
        if reported.get(job_id) != Some(status) {
            send(
                events,
                RuntimeEvent::ContextCompactionStatus {
                    job_id: job_id.clone(),
                    status: status.clone(),
                },
            )?;
        }
    }
    *reported = current;
    Ok(())
}

fn cancel_pending_context_compactions(
    policy: &mut phi_steel::Policy,
    session: &phi_core::session::Session,
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    reported: &mut BTreeMap<String, String>,
    reason: &str,
) -> Result<()> {
    let job_ids = pending_context_job_ids(policy.state())?;
    if job_ids.is_empty() {
        return Ok(());
    }
    let cancelled = Event::ContextCompactionsCancelled {
        job_ids,
        reason: reason.into(),
    };
    session.append(&cancelled)?;
    let output = policy.on_event(&cancelled)?;
    session.append(&output)?;
    session.save_state(policy.state())?;
    send_context(events, policy.state())?;
    report_context_job_statuses(events, policy.state(), reported)
}

fn is_terminal_context_status(status: &str) -> bool {
    matches!(status, "applied" | "failed" | "cancelled" | "stale")
}

fn merge_context_completion_effect(foreground: Effect, completed: Effect) -> Effect {
    if matches!(foreground, Effect::HttpRequest { .. } | Effect::Continue)
        && matches!(completed, Effect::HttpRequest { .. })
    {
        completed
    } else {
        foreground
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

fn runtime_error(error: &anyhow::Error) -> String {
    phi_steel::user_error_message(error).unwrap_or_else(|| format!("{error:#}"))
}

fn observe_http(
    observer: Option<&Observability>,
    observation: phi_core::http::HttpObservation,
    call_id: Option<&str>,
) {
    let Some(observer) = observer else { return };
    let (event, level, fields) = match observation {
        phi_core::http::HttpObservation::Attempt { attempt } => (
            "provider.http_attempt",
            "info",
            serde_json::json!({ "attempt": attempt, "call_id": call_id }),
        ),
        phi_core::http::HttpObservation::Status { attempt, status } => (
            "provider.http_status",
            if status < 400 { "info" } else { "error" },
            serde_json::json!({ "attempt": attempt, "status": status, "call_id": call_id }),
        ),
        phi_core::http::HttpObservation::Retry { attempt, status } => (
            "provider.http_retry",
            "warn",
            serde_json::json!({ "attempt": attempt, "status": status, "call_id": call_id }),
        ),
        phi_core::http::HttpObservation::Failure { attempt } => (
            "provider.http_failure",
            "error",
            serde_json::json!({ "attempt": attempt, "call_id": call_id }),
        ),
    };
    observer.record(event, level, fields);
}

fn append_session(
    session: &phi_core::session::Session,
    value: &impl Serialize,
    observer: Option<&Observability>,
    kind: &str,
) -> Result<()> {
    match session.append(value) {
        Ok(()) => {
            observe!(
                observer,
                "session.write_completed",
                "info",
                serde_json::json!({ "kind": kind }),
            );
            Ok(())
        }
        Err(error) => {
            observe!(
                observer,
                "session.write_failed",
                "error",
                serde_json::json!({ "kind": kind, "error": runtime_error(&error) }),
            );
            Err(error)
        }
    }
}

fn protocol_event_name(event: &Event) -> &'static str {
    match event {
        Event::UserMessage { .. } => "user_message",
        Event::CompactRequested => "compact_requested",
        Event::ModelSelected { .. } => "model_selected",
        Event::ProcessCompleted { .. } => "process_completed",
        Event::ToolsCompleted { .. } => "tools_completed",
        Event::HttpCompleted { .. } => "http_completed",
        Event::ContextCompactionStarted { .. } => "context_compaction_started",
        Event::ContextCompactionCompleted { .. } => "context_compaction_completed",
        Event::ContextWaitCompleted { .. } => "context_wait_completed",
        Event::ContextCompactionsCancelled { .. } => "context_compactions_cancelled",
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
    config_root: &Path,
    policy: &mut phi_steel::Policy,
    name: &str,
    arguments: &serde_json::Value,
    full_access: bool,
    workspace_only: bool,
) -> Result<(serde_json::Value, serde_json::Value)> {
    let preparation: phi_core::file_edit::EditPreparation =
        serde_json::from_value(policy.prepare_file_edit(name, arguments)?)?;
    let snapshots = phi_core::file_edit::snapshots(
        workspace,
        &preparation.targets,
        full_access,
        (!workspace_only).then_some(config_root),
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
        (!workspace_only).then_some(config_root),
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
        sync::{Condvar, Mutex, mpsc as std_mpsc},
        time::{Duration, Instant},
    };

    #[test]
    fn runtime_worker_does_not_block_the_caller_during_synchronous_setup() {
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let worker = spawn_worker("phi-runtime-test", move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })
        .unwrap();

        started_rx.recv().unwrap();
        assert!(!worker.is_finished());
        release_tx.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn emits_independent_display_events_for_context_tool_messages() {
        let previous = serde_json::json!({ "messages": [] }).to_string();
        let next = serde_json::json!({
            "messages": [
                {
                    "kind": "tool_call",
                    "call_id": "inspect-a",
                    "name": "context_inspect",
                    "arguments": "{}"
                },
                {
                    "kind": "tool_result",
                    "call_id": "inspect-a",
                    "content": "{\"items\":[]}"
                },
                {
                    "kind": "tool_call",
                    "call_id": "mark-b",
                    "name": "context_mark",
                    "arguments": "{\"label\":\"next\"}"
                },
                {
                    "kind": "tool_result",
                    "call_id": "mark-b",
                    "content": "{\"opened\":\"S2\"}"
                },
                {
                    "kind": "tool_call",
                    "call_id": "wait-c",
                    "name": "context_wait",
                    "arguments": "{\"job_ids\":[\"J1\"]}"
                },
                {
                    "kind": "tool_result",
                    "call_id": "wait-c",
                    "content": "{\"jobs\":[]}"
                },
                {
                    "kind": "tool_call",
                    "call_id": "ordinary",
                    "name": "read_file",
                    "arguments": "{\"path\":\"README.md\"}"
                },
                {
                    "kind": "tool_result",
                    "call_id": "ordinary",
                    "content": "{\"content\":\"ignored\"}"
                }
            ]
        })
        .to_string();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        emit_context_tool_events(&event_tx, &previous, &next).unwrap();

        let events = std::iter::from_fn(|| event_rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(events.len(), 6);
        assert!(
            matches!(&events[0], RuntimeEvent::ToolStarted { call_id, name, .. }
            if call_id == "inspect-a" && name == "context_inspect")
        );
        assert!(
            matches!(&events[1], RuntimeEvent::ToolCompleted { call_id, name, .. }
            if call_id == "inspect-a" && name == "context_inspect")
        );
        assert!(
            matches!(&events[2], RuntimeEvent::ToolStarted { call_id, name, .. }
            if call_id == "mark-b" && name == "context_mark")
        );
        assert!(
            matches!(&events[3], RuntimeEvent::ToolCompleted { call_id, name, .. }
            if call_id == "mark-b" && name == "context_mark")
        );
        assert!(
            matches!(&events[4], RuntimeEvent::ToolStarted { call_id, name, .. }
            if call_id == "wait-c" && name == "context_wait")
        );
        assert!(
            matches!(&events[5], RuntimeEvent::ToolCompleted { call_id, name, .. }
            if call_id == "wait-c" && name == "context_wait")
        );
    }

    fn test_http_effect(marker: &str) -> Effect {
        Effect::HttpRequest {
            url: format!("https://example.com/{marker}"),
            secret: String::new(),
            headers: BTreeMap::new(),
            body: serde_json::json!({ "marker": marker }),
            timeout_ms: 1,
            stream: Vec::new(),
        }
    }

    #[test]
    fn context_completion_refreshes_only_safe_foreground_requests() {
        let refreshed =
            merge_context_completion_effect(test_http_effect("before"), test_http_effect("after"));
        assert!(matches!(
            refreshed,
            Effect::HttpRequest { body, .. } if body["marker"] == "after"
        ));

        let tools = Effect::RunTools { calls: Vec::new() };
        assert_eq!(
            merge_context_completion_effect(tools.clone(), test_http_effect("after")),
            tools
        );
        let finished = Effect::Finish {
            content: "done".into(),
        };
        assert_eq!(
            merge_context_completion_effect(finished.clone(), test_http_effect("after")),
            finished
        );
    }

    #[test]
    fn persisted_context_job_statuses_distinguish_pending_and_terminal_jobs() {
        let state = serde_json::json!({
            "context_jobs": [
                { "id": "J1", "status": "queued" },
                { "id": "J2", "status": "running" },
                { "id": "J3", "status": "applied" },
                { "id": "J4", "status": "failed" },
                { "id": "J5", "status": "cancelled" },
                { "id": "J6", "status": "stale" }
            ]
        })
        .to_string();
        assert_eq!(pending_context_job_ids(&state).unwrap(), vec!["J1", "J2"]);
        assert_eq!(
            context_job_status(&state, "J3").unwrap().as_deref(),
            Some("applied")
        );
        assert_eq!(context_job_status(&state, "unknown").unwrap(), None);
    }

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
            workspace_only: false,
            processes: Arc::new(phi_core::process::ShellSessions::default()),
            workflows: Arc::new(WorkflowTasks::default()),
            output_schema: None,
            observability: None,
        };
        (workspace, options)
    }

    #[test]
    fn workflow_child_session_persists_validated_effective_selection() {
        let (workspace, options) = options();
        let home = home_for_config(&options.config_path).unwrap();
        let sources = resolve_sources(&home, workspace.path()).unwrap();
        let parent = phi_core::session::Session::create_composed(
            &home.sessions(),
            &sources.config,
            &sources.plugins,
            &sources.skill_plugins,
        )
        .unwrap();
        let bootstrap = build_policy(
            &home,
            workspace.path(),
            &sources,
            None,
            false,
            false,
            parent.id(),
            None,
        )
        .unwrap();
        parent.save_state(bootstrap.policy.state()).unwrap();

        let child_id = "11111111-1111-4111-8111-111111111111";
        create_workflow_child_session_with_id(
            &options,
            child_id,
            parent.id(),
            phi_core::session::SessionMetadata {
                model: Some("openai/gpt-5.6-sol".into()),
                reasoning: Some("high".into()),
                service_tier: Some("default".into()),
                timeout_ms: Some(30_000),
                capability_profile: Some("read-only".into()),
                ..Default::default()
            },
            "openai/gpt-5.6-sol",
            "high",
            "default",
        )
        .unwrap();
        let child = phi_core::session::Session::open(&home.sessions(), child_id).unwrap();
        let state: serde_json::Value = serde_json::from_str(&child.load_state().unwrap()).unwrap();
        assert_eq!(state["model"], "openai/gpt-5.6-sol");
        assert_eq!(state["reasoning"], "high");
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(child.dir().join("meta.json")).unwrap()).unwrap();
        assert_eq!(metadata["timeout_ms"], 30_000);
        assert_eq!(metadata["capability_profile"], "read-only");

        let invalid_id = "22222222-2222-4222-8222-222222222222";
        let error = create_workflow_child_session_with_id(
            &options,
            invalid_id,
            parent.id(),
            Default::default(),
            "openai/gpt-5.6-sol",
            "invalid",
            "default",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported workflow agent reasoning")
        );
        assert!(!home.sessions().join(invalid_id).exists());
    }

    #[test]
    fn rust_combines_cli_permissions_with_argument_aware_policy() {
        let (workspace, options) = options();
        let home = home_for_config(&options.config_path).unwrap();
        let sources = resolve_sources(&home, workspace.path()).unwrap();
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
        let shell_allowed = phi_core::permissions::Permissions {
            allow_shell: true,
            allow_write: false,
        };
        assert!(matches!(
            authorize_tool_call(
                &shell_allowed,
                &mut policy,
                "exec_command",
                &serde_json::json!({ "cmd": "git status --short" }),
                false,
            )
            .unwrap(),
            ToolAuthorization::Allow
        ));
        assert!(matches!(
            authorize_tool_call(
                &shell_allowed,
                &mut policy,
                "exec_command",
                &serde_json::json!({ "cmd": "git push --force origin main" }),
                false,
            )
            .unwrap(),
            ToolAuthorization::Ask(detail) if detail.contains("git push --force")
        ));
        assert!(matches!(
            authorize_tool_call(
                &shell_allowed,
                &mut policy,
                "patch",
                &serde_json::json!({ "patch": "change" }),
                false,
            )
            .unwrap(),
            ToolAuthorization::Ask(detail) if detail == "patch: 6 characters"
        ));
        assert!(matches!(
            authorize_tool_call(
                &shell_allowed,
                &mut policy,
                "exec_command",
                &serde_json::json!({ "cmd": "git clean -fdx" }),
                true,
            )
            .unwrap(),
            ToolAuthorization::Allow
        ));
    }

    #[test]
    fn approval_precedence_is_fail_closed() {
        use phi_steel::ApprovalDecision::{Allow, Ask, Deny};
        assert_eq!(stricter_approval(Allow, Allow), Allow);
        assert_eq!(stricter_approval(Allow, Ask), Ask);
        assert_eq!(stricter_approval(Ask, Allow), Ask);
        assert_eq!(stricter_approval(Allow, Deny), Deny);
        assert_eq!(stricter_approval(Ask, Deny), Deny);
        assert!(!bundled_git_requires_approval(
            "exec_command",
            &serde_json::json!({ "cmd": "git status --short" })
        ));
        assert!(bundled_git_requires_approval(
            "exec_command",
            &serde_json::json!({ "cmd": "git clean -fdx" })
        ));
        assert!(bundled_git_requires_approval(
            "exec_command",
            &serde_json::json!({ "cmd": "git status; git clean -fdx" })
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_interactive_argument_aware_ask_fails_closed_without_execution() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (workspace, options) = options();
                let home = home_for_config(&options.config_path).unwrap();
                let sources = resolve_sources(&home, workspace.path()).unwrap();
                let session = phi_core::session::Session::create_composed(
                    &home.sessions(),
                    &sources.config,
                    &sources.plugins,
                    &sources.skill_plugins,
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
                    legacy_file_editor_tool: "patch",
                    events: &event_tx,
                    cancellation: &cancellation,
                    allow_shell: false,
                    allow_write: false,
                    interactive_approvals: true,
                    full_access: false,
                    workspace_only: false,
                    output_schema: None,
                    workflows: Arc::new(WorkflowTasks::default()),
                    plugin_roots: &plugin_roots,
                    observability: None,
                };
                let (results, _) = execute_tool_calls(
                    vec![ToolCall {
                        call_id: "dangerous".into(),
                        name: "exec_command".into(),
                        arguments: serde_json::json!({
                            "cmd": "git status; touch approval-bypass-marker",
                            "workdir": null,
                            "shell": null,
                            "login": null,
                            "tty": null,
                            "yield-time_ms": null,
                            "max_output_tokens": null
                        }),
                        execution: ToolExecution::LegacyDirect,
                    }],
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

                assert!(
                    results[0].result["error"]
                        .as_str()
                        .unwrap()
                        .contains("no approval channel is available")
                );
                assert!(!workspace.path().join("approval-bypass-marker").exists());
                assert!(
                    !std::iter::from_fn(|| event_rx.try_recv().ok())
                        .any(|event| matches!(event, RuntimeEvent::ApprovalRequested { .. }))
                );
            })
            .await;
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

    fn assert_same_catalog(left: &CommandCatalog, right: &CommandCatalog) {
        assert_eq!(
            serde_json::to_value(left).unwrap(),
            serde_json::to_value(right).unwrap()
        );
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
        assert!(
            catalog
                .commands
                .iter()
                .any(|command| command.name == "keys")
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
    fn fresh_and_resumed_catalog_match_command_initialization() {
        let (_workspace, mut options) = options();

        let fresh_catalog = command_catalog(&options).unwrap();
        let fresh_command = execute_command(
            &options,
            &CommandInvocation {
                name: "help".into(),
                arguments: String::new(),
            },
        )
        .unwrap();
        assert_same_catalog(&fresh_catalog, &fresh_command.catalog);

        options.session_id = Some(fresh_command.session_id);
        let resumed_catalog = command_catalog(&options).unwrap();
        let resumed_command = execute_command(
            &options,
            &CommandInvocation {
                name: "help".into(),
                arguments: String::new(),
            },
        )
        .unwrap();
        assert_same_catalog(&resumed_catalog, &resumed_command.catalog);
    }

    #[test]
    fn fresh_and_resumed_command_and_run_policy_initialization_match() {
        let (workspace, options) = options();
        let workspace = workspace.path().canonicalize().unwrap();
        let home = home_for_config(&options.config_path).unwrap();
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } }
        });

        let command_session = resolve_session(&home, &workspace, None).unwrap();
        let run_session = resolve_session(&home, &workspace, None).unwrap();
        let mut command = build_policy(
            &home,
            &workspace,
            &command_session.sources,
            command_session.saved_state,
            options.full_access,
            options.workspace_only,
            command_session.session.id(),
            None,
        )
        .unwrap();
        let mut run = build_policy(
            &home,
            &workspace,
            &run_session.sources,
            run_session.saved_state,
            options.full_access,
            options.workspace_only,
            run_session.session.id(),
            Some(&schema),
        )
        .unwrap();
        assert_same_catalog(
            &catalog(&mut command.policy).unwrap(),
            &catalog(&mut run.policy).unwrap(),
        );

        command_session
            .session
            .save_state(command.policy.state())
            .unwrap();
        let resumed_command =
            resolve_session(&home, &workspace, Some(command_session.session.id())).unwrap();
        let resumed_run =
            resolve_session(&home, &workspace, Some(command_session.session.id())).unwrap();
        let mut command = build_policy(
            &home,
            &workspace,
            &resumed_command.sources,
            resumed_command.saved_state,
            options.full_access,
            options.workspace_only,
            resumed_command.session.id(),
            None,
        )
        .unwrap();
        let mut run = build_policy(
            &home,
            &workspace,
            &resumed_run.sources,
            resumed_run.saved_state,
            options.full_access,
            options.workspace_only,
            resumed_run.session.id(),
            Some(&schema),
        )
        .unwrap();
        assert_same_catalog(
            &catalog(&mut command.policy).unwrap(),
            &catalog(&mut run.policy).unwrap(),
        );
    }

    #[test]
    fn new_command_creates_a_fresh_session_and_preserves_the_old_one() {
        let (_workspace, mut options) = options();
        let home = home_for_config(&options.config_path).unwrap();
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
        phi_core::session::Session::open(&home.sessions(), &created.session_id).unwrap();
        let fresh_session =
            phi_core::session::Session::open(&home.sessions(), &fresh.session_id).unwrap();
        fresh_session.load_state().unwrap();
    }

    #[test]
    fn resumes_a_home_session_from_a_different_workspace() {
        let (_workspace, options) = options();
        let home = home_for_config(&options.config_path).unwrap();
        let original_workspace = options.workspace.canonicalize().unwrap();
        let created = resolve_session(&home, &original_workspace, None).unwrap();
        created.session.save_state("{\"messages\":[]}").unwrap();
        let session_id = created.session.id().to_owned();

        let other_workspace = tempfile::tempdir().unwrap();
        let resumed = resolve_session(&home, other_workspace.path(), Some(&session_id)).unwrap();
        assert_eq!(resumed.session.dir(), home.sessions().join(&session_id));
        assert_eq!(resumed.saved_state.as_deref(), Some("{\"messages\":[]}"));
        assert!(!other_workspace.path().join(".phi/sessions").exists());
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
        assert!(execution.content.contains("- planning:"));
        assert!(execution.content.contains("- dynamic-workflows:"));
    }

    #[test]
    fn unloaded_plugin_skills_are_session_pinned_across_update_and_remove() {
        let (workspace, options) = options();
        let home = home_for_config(&options.config_path).unwrap();
        let write_plugin = |commit: &str, instructions: &str| {
            let root = phi_core::plugin::install_root(&home, "unloaded", commit);
            std::fs::create_dir_all(root.join("skills/example")).unwrap();
            std::fs::write(root.join("plugin.scm"), "(define unloaded #t)").unwrap();
            std::fs::write(
                root.join("skills/example/SKILL.md"),
                format!(
                    "---\nname: example\ndescription: Example plugin skill.\n---\n\n{instructions}\n"
                ),
            )
            .unwrap();
        };
        let write_lock = |commit: &str| {
            std::fs::write(
                home.plugin_lock(),
                serde_json::to_vec_pretty(&phi_core::plugin::PluginLock {
                    plugins: vec![phi_core::plugin::LockedPlugin {
                        name: "unloaded".into(),
                        url: "https://example.invalid/plugins.git".into(),
                        requested_rev: "main".into(),
                        commit: commit.into(),
                        path: "plugins/unloaded".into(),
                    }],
                })
                .unwrap(),
            )
            .unwrap();
        };
        let instructions = |sources: &phi_core::session::ComposedSources| {
            let catalog = discover_skills_with_plugins(
                &home,
                workspace.path(),
                &plugin_skill_sources(sources),
            )
            .unwrap();
            std::fs::read_to_string(catalog.resource_roots()["skill://example/"].join("SKILL.md"))
                .unwrap()
        };

        write_plugin("commit-a", "Pinned version A.");
        write_lock("commit-a");
        let current = resolve_sources(&home, workspace.path()).unwrap();
        assert!(
            !current
                .plugins
                .iter()
                .any(|plugin| plugin.name == "unloaded")
        );
        assert!(
            current
                .skill_plugins
                .iter()
                .any(|plugin| plugin.name == "unloaded")
        );
        let session = phi_core::session::Session::create_composed(
            &home.sessions(),
            &current.config,
            &current.plugins,
            &current.skill_plugins,
        )
        .unwrap();
        let pinned = session.composed_sources().unwrap().unwrap();
        assert!(instructions(&pinned).contains("Pinned version A."));

        write_plugin("commit-b", "Updated version B.");
        write_lock("commit-b");
        let updated = resolve_sources(&home, workspace.path()).unwrap();
        assert!(instructions(&updated).contains("Updated version B."));
        assert!(instructions(&pinned).contains("Pinned version A."));

        phi_core::plugin::remove(&home, "unloaded").unwrap();
        let removed = resolve_sources(&home, workspace.path()).unwrap();
        assert!(
            !removed
                .skill_plugins
                .iter()
                .any(|plugin| plugin.name == "unloaded")
        );
        assert!(instructions(&pinned).contains("Pinned version A."));
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
    fn provider_stream_rules_classify_output_by_phase_and_emit_tool_events() {
        let rules: Vec<StreamRule> = serde_json::from_value(serde_json::json!([
            {
                "match": { "/type": "response.output_item.added", "/item/type": "message" },
                "emit": "output_phase",
                "key": "/output_index",
                "value": "/item/phase"
            },
            {
                "match": { "/type": "response.output_text.delta" },
                "emit": "output_delta",
                "key": "/output_index",
                "value": "/delta"
            },
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
        let mut phases = BTreeMap::new();

        emit_stream_events(
            &events,
            &serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": { "type": "message", "phase": "commentary" }
            }),
            &rules,
            &mut phases,
        );
        emit_stream_events(
            &events,
            &serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "Checked the request."
            }),
            &rules,
            &mut phases,
        );
        emit_stream_events(
            &events,
            &serde_json::json!({
                "type": "response.output_item.added",
                "item": { "type": "web_search_call" }
            }),
            &rules,
            &mut phases,
        );
        emit_stream_events(
            &events,
            &serde_json::json!({
                "type": "response.output_item.done",
                "item": { "type": "web_search_call", "action": { "sources": [] } }
            }),
            &rules,
            &mut phases,
        );

        assert!(matches!(
            received.try_recv().unwrap(),
            RuntimeEvent::CommentaryStarted
        ));
        assert!(matches!(
            received.try_recv().unwrap(),
            RuntimeEvent::CommentaryDelta { content }
                if content == "Checked the request."
        ));
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
        let home = home_for_config(&options.config_path).unwrap();
        let execution = execute_command(
            &options,
            &CommandInvocation {
                name: "model".into(),
                arguments: "openai/gpt-5.6-terra high fast".into(),
            },
        )
        .unwrap();
        options.session_id = Some(execution.session_id.clone());
        let state = phi_core::session::Session::open(&home.sessions(), &execution.session_id)
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

        let session =
            phi_core::session::Session::open(&home.sessions(), &execution.session_id).unwrap();
        let sources = session.composed_sources().unwrap().unwrap();
        let capabilities = capabilities(&home, false);
        let skills = discover_skills(&home, &options.workspace).unwrap();
        let plugins = entrypoints(&sources);
        let mut policy = phi_steel::Policy::load_with_state(
            &sources.config,
            &plugins,
            &policy_config(
                &capabilities,
                session.id(),
                &UserState::default(),
                &skills.skills,
            ),
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
                    && body["reasoning"]["summary"] == "none"
                    && body["service_tier"] == "priority"
                    && body["parallel_tool_calls"] == true
                    && body["prompt_cache_key"] == execution.session_id
                    && headers["session_id"] == execution.session_id
                    && body["tools"].as_array().unwrap().len() == 15
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "read_file"
                            && tool["description"].as_str().unwrap()
                                .contains("phi-harness: Inspect, explain")
                            && tool["description"].as_str().unwrap()
                                .contains("skill://phi-harness/SKILL.md"))
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
                        .any(|tool| tool["name"] == "context_mark")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "context_inspect")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "context_compact")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "context_wait"
                            && tool["strict"] == true
                            && tool["parameters"]["properties"]["job_ids"]["type"]
                                == serde_json::json!(["array", "null"])
                            && tool["parameters"]["required"]
                                == serde_json::json!(["job_ids"]))
                    && !body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "load_skill")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "patch")
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["name"] == "Workflow"
                            && tool["strict"] == false
                            && tool["parameters"]["properties"]["path"]["type"] == "string"
                            && tool["parameters"]["properties"]["source"]["type"] == "string"
                            && tool["parameters"]["properties"]["args"].get("type").is_none()
                            && tool["parameters"]["properties"]["args"]["description"]
                                .as_str().unwrap().contains("Declared input schemas")
                            && tool["parameters"]["required"]
                                == serde_json::json!(["args"])
                            && tool["parameters"]["oneOf"].as_array().unwrap().len() == 2)
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
            true,
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
    fn editor_applies_repeated_updates_to_one_file_atomically() {
        let (workspace, options) = options();
        std::fs::write(
            workspace.path().join("notes.txt"),
            "first old\nmiddle\nlast old\n",
        )
        .unwrap();
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

        let (result, _) = execute_file_edit(
            workspace.path(),
            &home.root,
            &mut policy,
            "patch",
            &serde_json::json!({
                "patch": concat!(
                    "*** Begin Patch\n",
                    "*** Update File: notes.txt\n",
                    "@@\n",
                    "-last old\n",
                    "+last new\n",
                    "*** Update File: notes.txt\n",
                    "@@\n",
                    "-first old\n",
                    "+first new\n",
                    "*** End Patch\n"
                )
            }),
            false,
            false,
        )
        .unwrap();

        assert_eq!(result["changes"].as_array().unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("notes.txt")).unwrap(),
            "first new\nmiddle\nlast new\n"
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
            false,
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
            false,
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "(configured #t)\n");
    }

    #[test]
    fn workspace_only_editor_cannot_reconfigure_phi_home() {
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
        let path = home.root.join("forbidden.scm");

        execute_file_edit(
            workspace.path(),
            &home.root,
            &mut policy,
            "patch",
            &serde_json::json!({
                "patch": format!(
                    "*** Begin Patch\n*** Add File: {}\n+(forbidden #t)\n*** End Patch\n",
                    path.display()
                )
            }),
            false,
            true,
        )
        .unwrap_err();

        assert!(!path.exists());
    }

    #[test]
    fn workspace_only_reader_cannot_read_phi_home() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = phi_core::home::PhiHome {
            root: root.path().join("home"),
        };
        std::fs::create_dir(&workspace).unwrap();
        initialize_at(&home).unwrap();
        std::fs::write(workspace.join("allowed.txt"), "allowed\n").unwrap();
        let forbidden = home.root.join("forbidden.txt");
        std::fs::write(&forbidden, "forbidden\n").unwrap();
        let skills = discover_skills(&home, &workspace).unwrap();
        let capabilities = capabilities_for_skills(&home, false, true, &skills, true, None);

        let allowed = capabilities
            .execute(
                &workspace,
                "read_file",
                serde_json::json!({ "path": "allowed.txt" }),
            )
            .unwrap();
        assert_eq!(allowed["content"], "allowed\n");
        let error = capabilities
            .execute(
                &workspace,
                "read_file",
                serde_json::json!({ "path": forbidden }),
            )
            .unwrap_err();
        assert!(error.to_string().contains("outside allowed roots"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executes_parallel_safe_calls_concurrently() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (workspace, options) = options();
                let home = home_for_config(&options.config_path).unwrap();
                let sources = resolve_sources(&home, workspace.path()).unwrap();
                let session = phi_core::session::Session::create_composed(
                    &home.sessions(),
                    &sources.config,
                    &sources.plugins,
                    &sources.skill_plugins,
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
                        execution: ToolExecution::Capability,
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
                    legacy_file_editor_tool: "patch",
                    events: &event_tx,
                    cancellation: &cancellation,
                    allow_shell: false,
                    allow_write: false,
                    interactive_approvals: true,
                    full_access: false,
                    workspace_only: false,
                    output_schema: None,
                    workflows: Arc::new(WorkflowTasks::default()),
                    plugin_roots: &plugin_roots,
                    observability: None,
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

    #[test]
    fn typed_effect_parallel_safety_does_not_depend_on_tool_names() {
        let capabilities = phi_core::capability::Registry::default();
        let process = ToolCall {
            call_id: "process".into(),
            name: "arbitrary_process_alias".into(),
            arguments: serde_json::json!({}),
            execution: ToolExecution::ManagedProcess {
                action: ManagedProcessAction::Execute,
            },
        };
        let workflow = ToolCall {
            call_id: "workflow".into(),
            name: "arbitrary_workflow_alias".into(),
            arguments: serde_json::json!({}),
            execution: ToolExecution::Workflow {
                action: WorkflowAction::Launch,
            },
        };

        assert!(tool_call_parallel_safe(&process, &capabilities));
        assert!(!tool_call_parallel_safe(&workflow, &capabilities));
    }

    #[test]
    fn persisted_direct_routes_are_migrated_before_dispatch() {
        let cases = [
            (
                "exec_command",
                ToolExecution::ManagedProcess {
                    action: ManagedProcessAction::Execute,
                },
            ),
            (
                "write_stdin",
                ToolExecution::ManagedProcess {
                    action: ManagedProcessAction::WriteStdin,
                },
            ),
            (
                "list_processes",
                ToolExecution::ManagedProcess {
                    action: ManagedProcessAction::List,
                },
            ),
            (
                "terminate_process",
                ToolExecution::ManagedProcess {
                    action: ManagedProcessAction::Terminate,
                },
            ),
            (
                "Workflow",
                ToolExecution::Workflow {
                    action: WorkflowAction::Launch,
                },
            ),
            (
                "TaskOutput",
                ToolExecution::Workflow {
                    action: WorkflowAction::Output,
                },
            ),
            (
                "TaskStop",
                ToolExecution::Workflow {
                    action: WorkflowAction::Stop,
                },
            ),
            ("reload_config", ToolExecution::ReloadConfig),
            ("custom_editor", ToolExecution::FileEdit),
            ("custom_capability", ToolExecution::Capability),
        ];

        for (name, expected) in cases {
            let mut call = ToolCall {
                call_id: "legacy".into(),
                name: name.into(),
                arguments: serde_json::json!({}),
                execution: ToolExecution::LegacyDirect,
            };
            migrate_legacy_tool_execution(&mut call, "custom_editor");
            assert_eq!(call.execution, expected, "legacy route for {name}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_settles_completed_tool_calls_before_saving_state() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (workspace, mut options) = options();
                let home = home_for_config(&options.config_path).unwrap();
                let sources = resolve_sources(&home, workspace.path()).unwrap();
                let session = phi_core::session::Session::create_composed(
                    &home.sessions(),
                    &sources.config,
                    &sources.plugins,
                    &sources.skill_plugins,
                )
                .unwrap();
                let capabilities = capabilities(&home, false);
                let mut policy = phi_steel::Policy::load_with_state(
                    &sources.config,
                    &entrypoints(&sources),
                    &policy_config(
                        &capabilities,
                        session.id(),
                        &load_user_state(&home).unwrap(),
                        &[],
                    ),
                    None,
                )
                .unwrap();
                policy
                    .on_event(&Event::UserMessage {
                        content: "wait".into(),
                    })
                    .unwrap();
                let output = policy
                    .on_event(&Event::HttpCompleted {
                        success: true,
                        status: 200,
                        events: vec![serde_json::json!({
                            "type": "response.output_item.done",
                            "item": {
                                "type": "function_call",
                                "call_id": "waiting-call",
                                "name": "TaskOutput",
                                "arguments": "{\"task_id\":\"task\",\"wait_ms\":300000}"
                            }
                        })],
                        error: String::new(),
                    })
                    .unwrap();
                assert!(matches!(&output.effects[0], Effect::RunTools { .. }));
                let mut saved_state: serde_json::Value =
                    serde_json::from_str(policy.state()).unwrap();
                saved_state["context_jobs"] = serde_json::json!([{
                    "id": "J1",
                    "items": [],
                    "label": "pending",
                    "status": "running",
                    "error": "",
                    "attempt": 0,
                    "snapshot": []
                }]);
                session.save_state(&saved_state.to_string()).unwrap();
                options.session_id = Some(session.id().into());

                let (event_tx, _event_rx) = mpsc::unbounded_channel();
                let (_command_tx, command_rx) = mpsc::unbounded_channel();
                let (_steering_tx, steering_rx) = mpsc::unbounded_channel();
                let cancellation = CancellationToken::new();
                cancellation.cancel();
                let error = run(
                    options,
                    vec![Event::ToolsCompleted {
                        results: vec![ToolResult {
                            call_id: "waiting-call".into(),
                            name: "TaskOutput".into(),
                            result: serde_json::json!({ "error": "cancelled" }),
                        }],
                    }],
                    &event_tx,
                    command_rx,
                    steering_rx,
                    &cancellation,
                )
                .await
                .unwrap_err();
                assert_eq!(runtime_error(&error), "cancelled");

                let state: serde_json::Value =
                    serde_json::from_str(&session.load_state().unwrap()).unwrap();
                let messages = state["messages"].as_array().unwrap();
                assert!(messages.iter().any(|message| {
                    message["kind"] == "tool_call" && message["call_id"] == "waiting-call"
                }));
                assert!(messages.iter().any(|message| {
                    message["kind"] == "tool_result" && message["call_id"] == "waiting-call"
                }));
                assert_eq!(state["context_jobs"][0]["status"], "cancelled");
                assert_eq!(state["context_jobs"][0]["error"], "agent turn cancelled");
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
                    &home.sessions(),
                    &sources.config,
                    &sources.plugins,
                    &sources.skill_plugins,
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
                    legacy_file_editor_tool: "patch",
                    events: &event_tx,
                    cancellation: &cancellation,
                    allow_shell: false,
                    allow_write: false,
                    interactive_approvals: true,
                    full_access: false,
                    workspace_only: false,
                    output_schema: None,
                    workflows: Arc::new(WorkflowTasks::default()),
                    plugin_roots: &plugin_roots,
                    observability: None,
                };
                let (results, reloaded) = execute_tool_calls(
                    vec![ToolCall {
                        call_id: "reload".into(),
                        name: "reload_config".into(),
                        arguments: serde_json::json!({}),
                        execution: ToolExecution::ReloadConfig,
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
    fn refreshes_versioned_builtins_without_overwriting_user_config() {
        let root = tempfile::tempdir().unwrap();
        let home = phi_core::home::PhiHome {
            root: root.path().join("home"),
        };
        initialize_at(&home).unwrap();
        std::fs::write(
            home.builtins().join("plugins/responses/plugin.scm"),
            "stale",
        )
        .unwrap();
        std::fs::write(home.scheme_config(), "user composition").unwrap();
        initialize_at(&home).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.builtins().join("plugins/responses/plugin.scm")).unwrap(),
            BUNDLED_PLUGINS
                .get_file("responses/plugin.scm")
                .unwrap()
                .contents_utf8()
                .unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(
                home.builtins()
                    .join("plugins/compaction-structured/plugin.scm")
            )
            .unwrap(),
            BUNDLED_PLUGINS
                .get_file("compaction-structured/plugin.scm")
                .unwrap()
                .contents_utf8()
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
        let planning =
            std::fs::read_to_string(home.builtin_skills().join("planning/SKILL.md")).unwrap();
        assert_eq!(planning, PLANNING_SKILL[0].1);
        assert!(planning.contains("**Stage:** writing"));
        assert!(planning.contains("# Acceptance Criteria"));
        assert!(planning.contains("[>]"));
        assert!(!planning.contains("**Current:**"));
        assert!(planning.contains("explicitly approves"));
        assert!(planning.contains("call `create_plan`"));
        assert!(planning.contains("Multiple plans may coexist"));
        assert!(planning.contains("never delete, archive, or move them automatically"));
        assert!(
            phi_core::plugin::read_lock(&home)
                .unwrap()
                .plugins
                .is_empty()
        );
        assert_eq!(
            phi_core::plugin::official_catalog().unwrap().plugins.len(),
            12
        );
    }

    #[test]
    fn concurrent_initialization_processes_publish_one_complete_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let sync = root.path().join("sync");
        std::fs::create_dir(&sync).unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for id in 0..6 {
            children.push(
                std::process::Command::new(&executable)
                    .args([
                        "--exact",
                        "tests::concurrent_initialization_subprocess",
                        "--ignored",
                    ])
                    .env("PHI_INITIALIZATION_TEST_HOME", &home)
                    .env("PHI_INITIALIZATION_TEST_SYNC", &sync)
                    .env("PHI_INITIALIZATION_TEST_ID", id.to_string())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read_dir(&sync).unwrap().count() < children.len() {
            assert!(
                Instant::now() < deadline,
                "subprocesses did not become ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(sync.join("go"), "").unwrap();

        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "subprocess failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let home = phi_core::home::PhiHome { root: home };
        let official = phi_core::plugin::official_catalog().unwrap();
        let snapshot_id = bundled_snapshot_id(&official).unwrap();
        assert!(
            bundled_snapshot_matches(&home.builtins(), &snapshot_id, &official).unwrap(),
            "published snapshot must be complete"
        );
        assert_eq!(
            std::fs::read_dir(home.builtin_version_root())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("snapshot-"))
                .count(),
            1
        );
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn concurrent_initialization_subprocess() {
        let Some(root) = std::env::var_os("PHI_INITIALIZATION_TEST_HOME") else {
            return;
        };
        let sync = PathBuf::from(std::env::var_os("PHI_INITIALIZATION_TEST_SYNC").unwrap());
        let id = std::env::var("PHI_INITIALIZATION_TEST_ID").unwrap();
        std::fs::write(sync.join(format!("ready-{id}")), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sync.join("go").is_file() {
            assert!(
                Instant::now() < deadline,
                "parent did not release subprocess"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let home = phi_core::home::PhiHome {
            root: PathBuf::from(root),
        };
        initialize_at(&home).unwrap();
        resolve_sources(&home, Path::new(".")).unwrap();
    }

    #[test]
    fn preserves_previous_snapshots_and_user_data_while_cleaning_staging() {
        let root = tempfile::tempdir().unwrap();
        let home = phi_core::home::PhiHome {
            root: root.path().join("home"),
        };
        initialize_at(&home).unwrap();
        let version_root = home.builtin_version_root();
        let previous = version_root.join("snapshot-previous");
        std::fs::rename(home.builtins(), &previous).unwrap();
        std::fs::write(version_root.join("current"), "snapshot-previous").unwrap();
        let stale = version_root.join(".staging-stale");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("partial"), "partial").unwrap();
        #[cfg(unix)]
        let outside = {
            let outside = root.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("sentinel"), "outside").unwrap();
            std::os::unix::fs::symlink(&outside, version_root.join(".staging-linked")).unwrap();
            outside
        };

        std::fs::create_dir_all(home.plugins().join("custom/commit")).unwrap();
        std::fs::write(
            home.plugins().join("custom/commit/user-data"),
            "user plugin",
        )
        .unwrap();
        std::fs::write(home.config(), "user json config").unwrap();
        std::fs::write(home.scheme_config(), "user scheme config").unwrap();
        std::fs::write(home.state(), "user state").unwrap();
        std::fs::write(home.plugin_lock(), "{\"plugins\":[]}").unwrap();

        initialize_at(&home).unwrap();

        assert!(previous.join(".complete").is_file());
        assert!(previous.join("plugins/responses/plugin.scm").is_file());
        assert!(!stale.exists());
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_to_string(outside.join("sentinel")).unwrap(),
            "outside"
        );
        assert_eq!(
            std::fs::read_to_string(home.plugins().join("custom/commit/user-data")).unwrap(),
            "user plugin"
        );
        assert_eq!(
            std::fs::read_to_string(home.config()).unwrap(),
            "user json config"
        );
        assert_eq!(
            std::fs::read_to_string(home.scheme_config()).unwrap(),
            "user scheme config"
        );
        assert_eq!(std::fs::read_to_string(home.state()).unwrap(), "user state");
        assert_ne!(home.builtins(), previous);
        assert!(home.builtins().join(".complete").is_file());
    }
}
