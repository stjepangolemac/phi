use std::{
    collections::HashSet,
    env,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    policy: PathBuf,
    provider: PathBuf,
    compaction: PathBuf,
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
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
    CheckPolicy,
    PolicyCandidate {
        path: PathBuf,
    },
    PolicyActivate {
        id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tokio::task::LocalSet::new().run_until(run()).await
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let workspace = cli
        .workspace
        .canonicalize()
        .context("workspace does not exist")?;
    match cli.command {
        Command::Run { prompt } => {
            run_frontend(
                &workspace,
                prompt,
                cli.json,
                None,
                cli.allow_shell,
                cli.allow_write,
            )
            .await
        }
        Command::Resume { session, prompt } => {
            run_frontend(
                &workspace,
                prompt,
                cli.json,
                Some(session),
                cli.allow_shell,
                cli.allow_write,
            )
            .await
        }
        Command::Read { path } => {
            let mut registry = phi_core::capability::Registry::default();
            registry.register(phi_core::capability::ReadFile);
            print_json(&registry.execute(
                &workspace,
                "read_file",
                serde_json::json!({ "path": path }),
            )?)
        }
        Command::Shell {
            program,
            args,
            stdin,
            timeout_ms,
        } => {
            let allowed = config()?.allowed_programs;
            print_json(
                &phi_core::process::run(&workspace, &allowed, &program, &args, &stdin, timeout_ms)
                    .await?,
            )
        }
        Command::CheckPolicy => {
            let config = config()?;
            phi_steel::check(&config.policy, &config.provider, &config.compaction)?;
            println!("policy ok");
            Ok(())
        }
        Command::PolicyCandidate { path } => {
            let provider = config()?.provider;
            let compaction = config()?.compaction;
            phi_steel::check(&path, &provider, &compaction)?;
            phi_steel::replay_smoke(&path, &provider, &compaction)?;
            let id = phi_core::policy_store::submit(&workspace.join(".phi/policies"), &path)?;
            println!("{id}");
            Ok(())
        }
        Command::PolicyActivate { id } => {
            phi_core::policy_store::activate(&workspace.join(".phi/policies"), &id)?;
            println!("activated {id}");
            Ok(())
        }
    }
}

async fn run_frontend(
    workspace: &Path,
    prompt: String,
    json_output: bool,
    session_id: Option<String>,
    allow_shell: bool,
    allow_write: bool,
) -> Result<()> {
    let mut handle = phi_runtime::start(
        phi_runtime::RunOptions {
            workspace: workspace.into(),
            config_path: repo_file("phi.json")?,
            session_id,
            allow_shell,
            allow_write,
            interactive_approvals: false,
        },
        prompt,
    );
    let mut streamed = false;
    while let Some(event) = handle.events.recv().await {
        if json_output {
            emit_json(&event)?;
            match event {
                phi_runtime::RuntimeEvent::Finished { .. } => return Ok(()),
                phi_runtime::RuntimeEvent::Error { message } => bail!(message),
                _ => {}
            }
            continue;
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
                    println!();
                } else {
                    println!("{content}");
                }
                return Ok(());
            }
            phi_runtime::RuntimeEvent::Error { message } => bail!(message),
            phi_runtime::RuntimeEvent::UserMessage { .. }
            | phi_runtime::RuntimeEvent::ContextUpdated { .. }
            | phi_runtime::RuntimeEvent::ApprovalRequested { .. } => {}
        }
    }
    bail!("runtime stopped without finishing")
}

fn repo_file(relative: &str) -> Result<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join(relative);
    if !path.is_file() {
        bail!("missing {}", path.display());
    }
    Ok(path)
}

fn config() -> Result<Config> {
    let path = repo_file("phi.json")?;
    let root = path.parent().context("config has no parent")?;
    let mut config: Config = serde_json::from_slice(&std::fs::read(&path)?)?;
    config.policy = root.join(config.policy);
    config.provider = root.join(config.provider);
    config.compaction = root.join(config.compaction);
    Ok(config)
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
