use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use phi_protocol::{Effect, Event};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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

impl Handle {
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
}

#[derive(Deserialize)]
struct Config {
    policy: PathBuf,
    provider: PathBuf,
    compaction: PathBuf,
    allowed_programs: HashSet<String>,
    allowed_http_origins: HashSet<String>,
    secrets: BTreeMap<String, phi_core::http::SecretConfig>,
    context_char_budget: usize,
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
            let sources = session.sources()?;
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
                &config.compaction,
            )?;
            let sources = session.sources()?;
            (session, sources, None)
        }
    };
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
        &sources.compaction,
        &serde_json::json!({ "context_char_budget": config.context_char_budget }).to_string(),
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
                    let mut registry = phi_core::capability::Registry::default();
                    registry.register(phi_core::capability::ReadFile);
                    registry.register(phi_core::capability::ReplaceFile);
                    registry.register(phi_eval::SubmitPolicyCandidate {
                        active_policy: sources.policy.clone(),
                        provider: sources.provider.clone(),
                        compaction: sources.compaction.clone(),
                    });
                    registry
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
    send(
        events,
        RuntimeEvent::ContextUpdated {
            estimated_tokens: state["estimated_tokens"].as_f64().unwrap_or_default() as u64,
            token_budget: (context_char_budget / 4) as u64,
            compactions: state["compactions"].as_f64().unwrap_or_default() as u64,
        },
    )
}

fn load_config(path: &Path) -> Result<Config> {
    let root = path.parent().context("config has no parent")?;
    let mut config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    config.policy = root.join(config.policy);
    config.provider = root.join(config.provider);
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
