use std::{collections::HashSet, io::Write, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    allowed_programs: HashSet<String>,
}

#[derive(Parser)]
#[command(name = "phi", version)]
struct Cli {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    allow_shell: bool,
    #[arg(long)]
    allow_write: bool,
    /// Run all tool calls without approval.
    #[arg(long)]
    yolo: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Init,
    Doctor,
    Status,
    Run {
        prompt: String,
    },
    Resume {
        session: String,
        prompt: String,
    },
    /// Serve one-shot agent requests over line-framed JSON-RPC on stdin/stdout.
    Rpc {
        #[arg(long)]
        session: Option<String>,
    },
    #[command(hide = true)]
    InternalCreateSession {
        id: String,
        #[arg(long)]
        parent_session: String,
        #[arg(long)]
        workflow_task: String,
        #[arg(long)]
        agent_label: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        worktree_path: Option<PathBuf>,
    },
    Read {
        path: PathBuf,
    },
    Shell {
        program: String,
        args: Vec<String>,
        #[arg(long, default_value = "")]
        stdin: String,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
    },
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Update official and installed plugins from their configured sources.
    UpdatePlugins,
    CheckConfig,
}

#[derive(Subcommand)]
enum PluginCommand {
    Install {
        name: String,
        url: String,
        #[arg(long)]
        rev: String,
        #[arg(long, default_value = ".")]
        path: String,
    },
    Update {
        name: String,
        #[arg(long)]
        rev: String,
    },
    Remove {
        name: String,
    },
    List,
    Check {
        name: String,
    },
    Sync,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tokio::task::LocalSet::new().run_until(run()).await
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let workspace = cli
        .workspace
        .canonicalize()
        .context("workspace does not exist")?;
    let home = phi_runtime::initialize_home()?;
    let (allow_shell, allow_write, interactive_approvals) = approval_settings(&cli);
    let processes = std::sync::Arc::new(phi_core::process::ShellSessions::default());
    let workflows = std::sync::Arc::new(phi_runtime::WorkflowTasks::default());
    let options = || phi_runtime::RunOptions {
        workspace: workspace.clone(),
        config_path: home.config(),
        session_id: None,
        allow_shell,
        allow_write,
        interactive_approvals,
        full_access: cli.yolo,
        processes: std::sync::Arc::clone(&processes),
        workflows: std::sync::Arc::clone(&workflows),
        output_schema: None,
    };
    let result = async {
        match cli.command {
            None => phi_tui::launch(options(), None).await,
            Some(Command::Init) => {
                println!("initialized {}", home.root.display());
                Ok(())
            }
            Some(Command::Doctor) => {
                let catalog = phi_runtime::command_catalog(&options())?;
                println!("home: {}", home.root.display());
                println!("models: {}", catalog.models.len());
                println!(
                    "selected: {}",
                    catalog.selected_model.as_deref().unwrap_or("none")
                );
                println!("ok");
                Ok(())
            }
            Some(Command::Status) => {
                let status = phi_runtime::harness_status(&options())?;
                if cli.json {
                    print_json(&status)
                } else {
                    print_status(&status);
                    Ok(())
                }
            }
            Some(Command::Run { prompt }) => run_frontend(options(), prompt, cli.json).await,
            Some(Command::Resume { session, prompt }) => {
                let mut options = options();
                options.session_id = Some(session);
                run_frontend(options, prompt, cli.json).await
            }
            Some(Command::Rpc { session }) => {
                let mut options = options();
                options.session_id = session;
                run_rpc(options).await
            }
            Some(Command::InternalCreateSession {
                id,
                parent_session,
                workflow_task,
                agent_label,
                branch,
                worktree_path,
            }) => {
                phi_runtime::create_session_with_id(
                    &options(),
                    &id,
                    phi_core::session::SessionMetadata {
                        workspace: Some(workspace.clone()),
                        parent_session_id: Some(parent_session),
                        workflow_task_id: Some(workflow_task),
                        agent_label: Some(agent_label),
                        branch,
                        worktree_path,
                    },
                )?;
                println!("{id}");
                Ok(())
            }
            Some(Command::Read { path }) => {
                let mut registry = phi_core::capability::Registry::default();
                registry.register(phi_core::capability::ReadFile {
                    full_access: cli.yolo,
                    additional_root: Some(home.root.clone()),
                    resource_roots: Default::default(),
                    resource_help: None,
                });
                print_json(&registry.execute(
                    &workspace,
                    "read_file",
                    serde_json::json!({ "path": path }),
                )?)
            }
            Some(Command::Shell {
                program,
                args,
                stdin,
                timeout_ms,
            }) => {
                let config: Config = serde_json::from_slice(&std::fs::read(home.config())?)?;
                print_json(
                    &phi_core::process::run(
                        &workspace,
                        &config.allowed_programs,
                        &program,
                        &args,
                        &stdin,
                        timeout_ms,
                    )
                    .await?,
                )
            }
            Some(Command::Plugin { command }) => plugin(&home, command),
            Some(Command::UpdatePlugins) => update_plugins(&home, &workspace),
            Some(Command::CheckConfig) => {
                phi_runtime::check_scheme_config(&home, &workspace)?;
                println!("config ok");
                Ok(())
            }
        }
    }
    .await;
    workflows.shutdown().await;
    processes.shutdown().await;
    result
}

fn update_plugins(home: &phi_core::home::PhiHome, workspace: &std::path::Path) -> Result<()> {
    let updated = phi_core::plugin::update_all(home)?;
    phi_runtime::check_scheme_config(home, workspace)?;
    println!("updated {} plugins", updated.len());
    Ok(())
}

async fn run_rpc(mut options: phi_runtime::RunOptions) -> Result<()> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let request: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(error) => {
            return rpc_error(
                serde_json::Value::Null,
                -32700,
                &format!("parse error: {error}"),
            );
        }
    };
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if request.get("jsonrpc").and_then(|value| value.as_str()) != Some("2.0") {
        return rpc_error(id, -32600, "invalid JSON-RPC request");
    }
    if request.get("method").and_then(|value| value.as_str()) != Some("agent.run") {
        return rpc_error(id, -32601, "method not found");
    }
    let Some(params) = request.get("params").and_then(|value| value.as_object()) else {
        return rpc_error(id, -32602, "agent.run requires object params");
    };
    let Some(prompt) = params.get("prompt").and_then(|value| value.as_str()) else {
        return rpc_error(id, -32602, "agent.run requires a string prompt");
    };
    let prompt = prompt.to_owned();
    options.output_schema = params
        .get("schema")
        .filter(|value| !value.is_null())
        .cloned();
    let structured = options.output_schema.is_some();
    let mut handle = phi_runtime::start(options, prompt);
    let mut session_id = None;
    while let Some(event) = handle.events.recv().await {
        match &event {
            phi_runtime::RuntimeEvent::Session { id } => session_id = Some(id.clone()),
            phi_runtime::RuntimeEvent::Finished { content } => {
                let value = if structured {
                    match serde_json::from_str(content) {
                        Ok(value) => value,
                        Err(error) => {
                            return rpc_error(
                                id,
                                -32001,
                                &format!("agent returned invalid structured output: {error}"),
                            );
                        }
                    }
                } else {
                    serde_json::Value::String(content.clone())
                };
                emit_json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "value": value, "sessionId": session_id }
                }))?;
                return Ok(());
            }
            phi_runtime::RuntimeEvent::Error { message } => {
                return rpc_error(id, -32000, message);
            }
            _ => {
                emit_json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "agent.event",
                    "params": event
                }))?;
            }
        }
    }
    rpc_error(id, -32000, "agent stopped without a result")
}

fn rpc_error(id: serde_json::Value, code: i64, message: &str) -> Result<()> {
    emit_json(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    }))
}

fn approval_settings(cli: &Cli) -> (bool, bool, bool) {
    (
        cli.allow_shell || cli.yolo,
        cli.allow_write || cli.yolo,
        !cli.yolo,
    )
}

fn plugin(home: &phi_core::home::PhiHome, command: PluginCommand) -> Result<()> {
    match command {
        PluginCommand::Install {
            name,
            url,
            rev,
            path,
        } => {
            let locked = phi_core::plugin::install(home, &name, &url, &rev, &path)?;
            let installed = phi_core::plugin::installed(home, &locked.name)?;
            phi_steel::check_plugin(&installed.entrypoint)?;
            println!("installed {} {}", locked.name, locked.commit);
        }
        PluginCommand::Update { name, rev } => {
            let current = phi_core::plugin::installed(home, &name)?.locked;
            let locked =
                phi_core::plugin::install(home, &current.name, &current.url, &rev, &current.path)?;
            println!("updated {} {}", locked.name, locked.commit);
        }
        PluginCommand::Remove { name } => {
            phi_core::plugin::remove(home, &name)?;
            println!("removed {name}");
        }
        PluginCommand::List => {
            let lock = phi_core::plugin::read_lock(home)?;
            for plugin in phi_core::plugin::official_catalog()?.plugins {
                if let Some(locked) = lock.plugins.iter().find(|item| item.name == plugin.name) {
                    println!("{} installed {}", plugin.name, locked.commit);
                } else {
                    println!("{} bundled", plugin.name);
                }
            }
            let official = phi_core::plugin::official_catalog()?;
            for plugin in lock
                .plugins
                .iter()
                .filter(|plugin| !official.plugins.iter().any(|item| item.name == plugin.name))
            {
                println!("{} {} installed {}", plugin.name, plugin.commit, plugin.url);
            }
        }
        PluginCommand::Check { name } => {
            let plugin = phi_core::plugin::installed(home, &name)?;
            phi_steel::check_plugin(&plugin.entrypoint)?;
            println!("{name} ok");
        }
        PluginCommand::Sync => {
            let plugins = phi_core::plugin::read_lock(home)?.plugins;
            for plugin in plugins {
                if !phi_core::plugin::install_root(home, &plugin.name, &plugin.commit).is_dir() {
                    phi_core::plugin::install(
                        home,
                        &plugin.name,
                        &plugin.url,
                        &plugin.commit,
                        &plugin.path,
                    )?;
                }
            }
            println!("plugins synced");
        }
    }
    Ok(())
}

async fn run_frontend(
    options: phi_runtime::RunOptions,
    prompt: String,
    json_output: bool,
) -> Result<()> {
    let mut handle = phi_runtime::start(options, prompt);
    let mut streamed = false;
    while let Some(event) = handle.events.recv().await {
        if json_output {
            emit_json(&event)?;
            match event {
                phi_runtime::RuntimeEvent::Finished { .. } => return Ok(()),
                phi_runtime::RuntimeEvent::Error { message } => bail!(message),
                _ => continue,
            }
        }
        match event {
            phi_runtime::RuntimeEvent::Session { id } => eprintln!("session: {id}"),
            phi_runtime::RuntimeEvent::ModelDelta { content } => {
                streamed = true;
                print!("{content}");
                std::io::stdout().flush()?;
            }
            phi_runtime::RuntimeEvent::ToolOutput { content, .. } => {
                print!("{content}");
                std::io::stdout().flush()?;
            }
            phi_runtime::RuntimeEvent::ToolStarted { name, .. } => {
                eprintln!("running tool: {name}")
            }
            phi_runtime::RuntimeEvent::ToolCompleted { name, .. } => {
                eprintln!("tool complete: {name}")
            }
            phi_runtime::RuntimeEvent::Finished { content } => {
                if streamed {
                    println!()
                } else {
                    println!("{content}")
                }
                return Ok(());
            }
            phi_runtime::RuntimeEvent::Error { message } => bail!(message),
            _ => {}
        }
    }
    bail!("runtime stopped without finishing")
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_status(status: &phi_runtime::HarnessStatus) {
    println!("version: {}", status.version);
    println!("workspace: {}", status.workspace);
    println!("home: {}", status.home);
    println!(
        "model: {} · {} · {}",
        status.model.as_deref().unwrap_or("none"),
        status.reasoning.as_deref().unwrap_or("none"),
        status.service_tier.as_deref().unwrap_or("none")
    );
    println!("config: {}", status.config.path);
    println!("prompt builder: {}", status.composition.prompt_builder);
    println!("file editor: {}", status.composition.file_editor);
    println!("compactor: {}", status.composition.compactor);
    println!("plugins:");
    for plugin in &status.plugins {
        println!(
            "  {} · {} · {}",
            plugin.name, plugin.source, plugin.revision
        );
    }
}

#[cfg(test)]
mod plugin_cli_tests {
    use super::*;

    #[test]
    fn plugin_install_requires_an_explicit_name() {
        assert!(
            Cli::try_parse_from([
                "phi",
                "plugin",
                "install",
                "https://example.invalid/plugin.git",
                "--rev",
                "main",
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from([
            "phi",
            "plugin",
            "install",
            "example",
            "https://example.invalid/plugin.git",
            "--rev",
            "main",
            "--path",
            "plugins/example",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plugin {
                command: PluginCommand::Install { name, path, .. }
            }) if name == "example" && path == "plugins/example"
        ));
    }
}

fn emit_json(value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer(std::io::stdout().lock(), value)?;
    println!();
    std::io::stdout().flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn yolo_preapproves_shell_and_writes() {
        let cli = Cli::try_parse_from(["phi", "--yolo"]).unwrap();
        assert_eq!(approval_settings(&cli), (true, true, false));
    }

    #[test]
    fn version_flags_report_the_package_version() {
        for flag in ["--version", "-V"] {
            let error = Cli::try_parse_from(["phi", flag]).err().unwrap();
            assert_eq!(error.kind(), ErrorKind::DisplayVersion);
            assert_eq!(
                error.to_string(),
                format!("phi {}\n", env!("CARGO_PKG_VERSION"))
            );
        }
    }

    #[test]
    fn standard_metadata_preserves_help_and_status_parsing() {
        let help = Cli::try_parse_from(["phi", "--help"]).err().unwrap();
        assert_eq!(help.kind(), ErrorKind::DisplayHelp);

        let cli = Cli::try_parse_from(["phi", "--json", "status"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Some(Command::Status)));
    }
}
