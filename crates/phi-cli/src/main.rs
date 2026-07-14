use std::{collections::HashSet, io::Write, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    allowed_programs: HashSet<String>,
}

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    allow_shell: bool,
    #[arg(long)]
    allow_write: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Init,
    Doctor,
    Run {
        prompt: String,
    },
    Resume {
        session: String,
        prompt: String,
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
    CheckPolicy,
    PolicyCandidate {
        path: PathBuf,
    },
    PolicyActivate {
        id: String,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    Install {
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
    let options = || phi_runtime::RunOptions {
        workspace: workspace.clone(),
        config_path: home.config(),
        session_id: None,
        allow_shell: cli.allow_shell,
        allow_write: cli.allow_write,
        interactive_approvals: true,
    };
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
        Some(Command::Run { prompt }) => run_frontend(options(), prompt, cli.json).await,
        Some(Command::Resume { session, prompt }) => {
            let mut options = options();
            options.session_id = Some(session);
            run_frontend(options, prompt, cli.json).await
        }
        Some(Command::Read { path }) => {
            let mut registry = phi_core::capability::Registry::default();
            registry.register(phi_core::capability::ReadFile);
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
        Some(Command::CheckPolicy) => {
            phi_runtime::check_policy(&home, &workspace)?;
            println!("policy ok");
            Ok(())
        }
        Some(Command::PolicyCandidate { path }) => {
            phi_runtime::check_policy_candidate(&home, &workspace, &path)?;
            let id = phi_core::policy_store::submit(&workspace.join(".phi/policies"), &path)?;
            println!("{id}");
            Ok(())
        }
        Some(Command::PolicyActivate { id }) => {
            phi_core::policy_store::activate(&workspace.join(".phi/policies"), &id)?;
            println!("activated {id}");
            Ok(())
        }
    }
}

fn plugin(home: &phi_core::home::PhiHome, command: PluginCommand) -> Result<()> {
    match command {
        PluginCommand::Install { url, rev, path } => {
            let locked = phi_core::plugin::install(home, &url, &rev, &path)?;
            let installed = phi_core::plugin::installed(home, &locked.name)?;
            phi_steel::check_plugin(&installed.root.join(installed.manifest.entrypoint))?;
            println!("installed {} {}", locked.name, locked.commit);
        }
        PluginCommand::Update { name, rev } => {
            let current = phi_core::plugin::installed(home, &name)?.locked;
            let locked = phi_core::plugin::install(home, &current.url, &rev, &current.path)?;
            println!("updated {} {}", locked.name, locked.commit);
        }
        PluginCommand::Remove { name } => {
            phi_core::plugin::remove(home, &name)?;
            println!("removed {name}");
        }
        PluginCommand::List => {
            for plugin in phi_core::plugin::read_lock(home)?.plugins {
                println!("{} {} {}", plugin.name, plugin.commit, plugin.url);
            }
        }
        PluginCommand::Check { name } => {
            let plugin = phi_core::plugin::installed(home, &name)?;
            phi_steel::check_plugin(&plugin.root.join(plugin.manifest.entrypoint))?;
            println!("{name} ok");
        }
        PluginCommand::Sync => {
            let plugins = phi_core::plugin::read_lock(home)?.plugins;
            for plugin in plugins {
                if !phi_core::plugin::install_root(home, &plugin.name, &plugin.commit).is_dir() {
                    phi_core::plugin::install(home, &plugin.url, &plugin.commit, &plugin.path)?;
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

fn emit_json(value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer(std::io::stdout().lock(), value)?;
    println!();
    std::io::stdout().flush()?;
    Ok(())
}
