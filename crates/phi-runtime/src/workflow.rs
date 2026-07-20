use std::{
    collections::HashMap,
    fs,
    path::{Component, Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::{process::Command, time::Instant};
use uuid::Uuid;

const MAX_CONCURRENCY: u64 = 8;
const MAX_AGENTS: u64 = 32;
const MAX_PROGRESS_BYTES: usize = 32 * 1024;

struct TaskEntry {
    child: tokio::sync::Mutex<tokio::process::Child>,
    dir: PathBuf,
}

#[derive(Default)]
pub struct WorkflowTasks {
    tasks: Mutex<HashMap<String, Arc<TaskEntry>>>,
}

impl WorkflowTasks {
    pub async fn launch(
        &self,
        workspace: &Path,
        home: &Path,
        parent_session_id: &str,
        session_dir: &Path,
        plugin_roots: &HashMap<String, PathBuf>,
        arguments: &Value,
    ) -> Result<Value> {
        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .context("Workflow requires name")?;
        validate_name(name)?;
        let requested_path = arguments
            .get("path")
            .map(|path| path.as_str().context("Workflow path must be a string"))
            .transpose()?;
        let workflow_path = resolve_workflow(workspace, home, plugin_roots, name, requested_path)?;
        let args = arguments.get("args").cloned().unwrap_or(Value::Null);
        let plugin = plugin_roots
            .get("dynamic-workflows")
            .context("dynamic-workflows plugin is not loaded")?;
        let metadata = inspect_workflow(plugin, &workflow_path, name, Some(&args))?;
        let runner = plugin.join("runner/workflow-runner.mjs");
        if !runner.is_file() {
            bail!("dynamic workflow runner is missing");
        }

        let task_id = Uuid::new_v4().to_string();
        let dir = session_dir.join("workflows").join(&task_id);
        fs::create_dir_all(&dir)?;
        let started_at = now_ms()?;
        write_json(
            &dir.join("state.json"),
            &json!({
                "taskId": task_id,
                "workflow": name,
                "status": "pending",
                "startedAt": started_at,
            }),
        )?;
        let phi = std::env::current_exe().context("resolve Phi executable")?;
        let git = git_context(workspace);
        let worktree_root = git.as_ref().map(|context| {
            std::env::temp_dir()
                .join("phi-worktrees")
                .join(format!("{}-{}", context.repo_name, context.repo_hash))
                .join(&task_id)
        });
        let request = json!({
            "taskId": task_id,
            "parentSessionId": parent_session_id,
            "name": name,
            "workflowPath": workflow_path,
            "args": args,
            "workspace": workspace,
            "home": home,
            "taskDir": dir,
            "phi": phi,
            "git": git,
            "worktreeRoot": worktree_root,
            "startedAt": started_at,
            "limits": {
                "maxConcurrency": MAX_CONCURRENCY,
                "maxAgents": MAX_AGENTS,
            }
        });
        let request_path = dir.join("request.json");
        write_json(&request_path, &request)?;
        let stdout = fs::File::create(dir.join("stdout.log"))?;
        let stderr = fs::File::create(dir.join("stderr.log"))?;
        let mut command = Command::new("node");
        command
            .arg(&runner)
            .arg(&request_path)
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().context("start Node workflow runner")?;
        self.tasks
            .lock()
            .expect("workflow task registry poisoned")
            .insert(
                task_id.clone(),
                Arc::new(TaskEntry {
                    child: tokio::sync::Mutex::new(child),
                    dir: dir.clone(),
                }),
            );
        Ok(json!({
            "status": "async_launched",
            "task_id": task_id,
            "workflow": name,
            "description": metadata["description"],
            "input_schema": metadata.get("inputSchema").cloned().unwrap_or(Value::Null),
            "task_dir": dir,
        }))
    }

    pub async fn output(&self, session_dir: &Path, arguments: &Value) -> Result<Value> {
        let task_id = task_id(arguments)?;
        let wait_ms = arguments
            .get("wait_ms")
            .filter(|value| !value.is_null())
            .map(|value| value.as_u64().context("wait_ms must be an integer"))
            .transpose()?
            .unwrap_or(15_000)
            .min(300_000);
        let dir = self
            .tasks
            .lock()
            .expect("workflow task registry poisoned")
            .get(task_id)
            .map(|entry| entry.dir.clone())
            .unwrap_or_else(|| session_dir.join("workflows").join(task_id));
        if !dir.join("state.json").is_file() {
            bail!("workflow task not found: {task_id}");
        }
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            self.reconcile(task_id, &dir).await?;
            let state = read_json(&dir.join("state.json"))?;
            let status = state["status"].as_str().unwrap_or("unknown");
            if terminal(status) || Instant::now() >= deadline {
                let result = read_optional_json(&dir.join("result.json"))?;
                let request = read_optional_json(&dir.join("request.json"))?;
                let summary = read_optional_json(&dir.join("summary.json"))?;
                let progress = read_tail(&dir.join("progress.jsonl"), MAX_PROGRESS_BYTES)?;
                return Ok(json!({
                    "task_id": task_id,
                    "workflow": state["workflow"].as_str()
                        .or_else(|| request["name"].as_str()),
                    "status": status,
                    "state": state,
                    "summary": summary,
                    "result": result,
                    "progress": progress,
                }));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn stop(&self, session_dir: &Path, arguments: &Value) -> Result<Value> {
        let task_id = task_id(arguments)?;
        let entry = self
            .tasks
            .lock()
            .expect("workflow task registry poisoned")
            .get(task_id)
            .cloned();
        let dir = entry
            .as_ref()
            .map(|entry| entry.dir.clone())
            .unwrap_or_else(|| session_dir.join("workflows").join(task_id));
        let request = read_optional_json(&dir.join("request.json"))?;
        let state = read_optional_json(&dir.join("state.json"))?;
        let workflow = request["name"]
            .as_str()
            .or_else(|| state["workflow"].as_str())
            .map(str::to_owned);
        let started_at = state["startedAt"].as_u64();
        let Some(entry) = entry else {
            if dir.join("state.json").is_file() {
                cleanup_owned_worktrees(&dir).await?;
                return Ok(json!({
                    "task_id": task_id,
                    "workflow": workflow.as_deref(),
                    "status": "not_running"
                }));
            }
            bail!("workflow task not found: {task_id}");
        };
        terminate(&entry).await;
        cleanup_owned_worktrees(&dir).await?;
        write_json(
            &dir.join("state.json"),
            &json!({
                "taskId": task_id,
                "workflow": workflow.as_deref(),
                "status": "cancelled",
                "startedAt": started_at,
                "completedAt": now_ms()?,
            }),
        )?;
        self.tasks
            .lock()
            .expect("workflow task registry poisoned")
            .remove(task_id);
        Ok(json!({
            "task_id": task_id,
            "workflow": workflow.as_deref(),
            "status": "cancelled"
        }))
    }

    pub async fn shutdown(&self) {
        let entries = self
            .tasks
            .lock()
            .expect("workflow task registry poisoned")
            .drain()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        for entry in entries {
            terminate(&entry).await;
            let _ = cleanup_owned_worktrees(&entry.dir).await;
        }
    }

    async fn reconcile(&self, task_id: &str, dir: &Path) -> Result<()> {
        let entry = self
            .tasks
            .lock()
            .expect("workflow task registry poisoned")
            .get(task_id)
            .cloned();
        let Some(entry) = entry else {
            let state = read_json(&dir.join("state.json"))?;
            if !terminal(state["status"].as_str().unwrap_or("")) {
                let cleanup = cleanup_owned_worktrees(dir).await;
                write_json(
                    &dir.join("state.json"),
                    &json!({
                        "taskId": task_id,
                        "workflow": state["workflow"].as_str(),
                        "status": "failed",
                        "error": match cleanup {
                            Ok(()) => "workflow task was stale and had no managed runner".to_owned(),
                            Err(error) => format!("workflow task was stale; cleanup failed: {error:#}"),
                        },
                        "startedAt": state["startedAt"].as_u64(),
                        "completedAt": now_ms()?,
                    }),
                )?;
            } else {
                cleanup_owned_worktrees(dir).await?;
            }
            return Ok(());
        };
        let mut child = entry.child.lock().await;
        if let Some(status) = child.try_wait()? {
            let state = read_json(&entry.dir.join("state.json"))?;
            drop(child);
            terminate(&entry).await;
            let cleanup = cleanup_owned_worktrees(&entry.dir).await;
            if !terminal(state["status"].as_str().unwrap_or("")) || cleanup.is_err() {
                let error = match cleanup {
                    Ok(()) => format!("workflow runner exited with {status}"),
                    Err(error) => format!(
                        "workflow runner exited with {status}; managed worktree cleanup failed: {error:#}"
                    ),
                };
                write_json(
                    &entry.dir.join("state.json"),
                    &json!({
                        "taskId": task_id,
                        "workflow": state["workflow"].as_str(),
                        "status": "failed",
                        "error": error,
                        "startedAt": state["startedAt"].as_u64(),
                        "completedAt": now_ms()?,
                    }),
                )?;
            }
            self.tasks
                .lock()
                .expect("workflow task registry poisoned")
                .remove(task_id);
        }
        Ok(())
    }
}

async fn cleanup_owned_worktrees(dir: &Path) -> Result<()> {
    let manifest_path = dir.join("worktrees.json");
    if !manifest_path.is_file() {
        return Ok(());
    }
    let request = read_json(&dir.join("request.json"))?;
    let mut manifest = read_json(&manifest_path)?;
    if manifest["version"].as_u64() != Some(1) {
        bail!("unsupported managed worktree manifest version");
    }
    let task_id = request["taskId"]
        .as_str()
        .context("workflow request has no taskId")?;
    if manifest["taskId"].as_str() != Some(task_id) {
        bail!("managed worktree manifest task does not match request");
    }
    let repo_root = PathBuf::from(
        request["git"]["repoRoot"]
            .as_str()
            .context("workflow request has no Git repository root")?,
    );
    if manifest["repoRoot"].as_str() != repo_root.to_str() {
        bail!("managed worktree manifest repository does not match request");
    }
    let expected_common_dir = PathBuf::from(
        request["git"]["gitCommonDir"]
            .as_str()
            .context("workflow request has no Git common directory")?,
    );
    let common_dir = PathBuf::from(git_output(&repo_root, &["rev-parse", "--git-common-dir"])?);
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        repo_root.join(common_dir)
    }
    .canonicalize()
    .context("canonicalize current Git common directory")?;
    if common_dir != expected_common_dir {
        bail!("Git repository identity changed during managed worktree cleanup");
    }
    let worktree_root = PathBuf::from(
        request["worktreeRoot"]
            .as_str()
            .context("workflow request has no managed worktree root")?,
    );
    if !worktree_root.is_absolute() {
        bail!("managed worktree root is not absolute");
    }
    if manifest["worktreeRoot"].as_str() != worktree_root.to_str() {
        bail!("managed worktree manifest root does not match request");
    }
    let task_short = task_id
        .get(..8)
        .context("workflow taskId is too short for managed branch namespace")?;
    let branch_prefix = format!("phi/{task_short}/");
    let root_owned = manifest["rootOwned"].as_bool() == Some(true);
    let entries = manifest["entries"]
        .as_array_mut()
        .context("managed worktree manifest entries must be an array")?;
    let mut errors = Vec::new();
    for entry in entries.iter_mut().rev() {
        if entry["state"].as_str() == Some("cleaned") {
            continue;
        }
        let branch = entry["branch"]
            .as_str()
            .context("managed worktree entry has no branch")?
            .to_owned();
        if !branch.starts_with(&branch_prefix) {
            bail!("managed branch is outside task namespace: {branch}");
        }
        let path = PathBuf::from(
            entry["path"]
                .as_str()
                .context("managed worktree entry has no path")?,
        );
        if !path.is_absolute() || path.parent() != Some(worktree_root.as_path()) {
            bail!(
                "managed worktree path is outside task root: {}",
                path.display()
            );
        }
        let state = entry["state"].as_str().unwrap_or("");
        let mut owns_branch = entry["branchCreated"].as_bool() == Some(true) || state == "active";
        if !owns_branch && path.exists() {
            let symbolic = Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(["symbolic-ref", "-q", "HEAD"])
                .output()
                .await
                .context("inspect managed Git worktree branch")?;
            owns_branch = symbolic.status.success()
                && String::from_utf8_lossy(&symbolic.stdout).trim()
                    == format!("refs/heads/{branch}");
        }
        let removal = Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(&path)
            .output()
            .await
            .context("remove managed Git worktree")?;
        if root_owned && path.exists() {
            fs::remove_dir_all(&path).with_context(|| {
                format!("force-remove managed worktree path {}", path.display())
            })?;
        }
        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .args(["worktree", "prune"])
            .status()
            .await;
        let deletion = if owns_branch {
            Some(
                Command::new("git")
                    .arg("-C")
                    .arg(&repo_root)
                    .args(["branch", "-D", &branch])
                    .output()
                    .await
                    .context("delete managed Git branch")?,
            )
        } else {
            None
        };
        let branch_exists = Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .args(["show-ref", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}"))
            .status()
            .await
            .context("verify managed Git branch cleanup")?
            .success();
        if path.exists() {
            errors.push(format!(
                "remove {}: {}",
                path.display(),
                String::from_utf8_lossy(&removal.stderr).trim()
            ));
        }
        if owns_branch && branch_exists {
            errors.push(format!(
                "delete {branch}: {}",
                String::from_utf8_lossy(&deletion.as_ref().unwrap().stderr).trim()
            ));
        }
        if !path.exists() && (!owns_branch || !branch_exists) {
            entry["state"] = Value::String("cleaned".to_owned());
        }
    }
    let _ = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .args(["worktree", "prune"])
        .status()
        .await;
    if root_owned {
        match fs::remove_dir(&worktree_root) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => errors.push(format!(
                "remove managed worktree root {}: {error}",
                worktree_root.display()
            )),
        }
    }
    write_json(&manifest_path, &manifest)?;
    if !errors.is_empty() {
        bail!(errors.join("; "));
    }
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GitContext {
    repo_root: PathBuf,
    git_common_dir: PathBuf,
    starting_commit: String,
    workspace_relative: PathBuf,
    repo_name: String,
    repo_hash: String,
}

fn git_context(workspace: &Path) -> Option<GitContext> {
    let root = PathBuf::from(git_output(workspace, &["rev-parse", "--show-toplevel"]).ok()?);
    let repo_root = root.canonicalize().ok()?;
    let workspace = workspace.canonicalize().ok()?;
    let workspace_relative = workspace.strip_prefix(&repo_root).ok()?;
    let common = PathBuf::from(git_output(&repo_root, &["rev-parse", "--git-common-dir"]).ok()?);
    let common = if common.is_absolute() {
        common
    } else {
        repo_root.join(common)
    };
    let git_common_dir = common.canonicalize().ok()?;
    let starting_commit =
        git_output(&repo_root, &["rev-parse", "--verify", "HEAD^{commit}"]).ok()?;
    let repo_name = repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("repository")
        .to_owned();
    let repo_hash =
        blake3::hash(git_common_dir.to_string_lossy().as_bytes()).to_hex()[..12].to_owned();
    Some(GitContext {
        repo_root,
        git_common_dir,
        starting_commit,
        workspace_relative: workspace_relative.to_owned(),
        repo_name,
        repo_hash,
    })
}

fn git_output(directory: &Path, arguments: &[&str]) -> Result<String> {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .context("run Git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)
        .context("Git output is not UTF-8")?
        .trim()
        .to_owned())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '-'
                || character == '_'
                || character == '.'
        })
    {
        bail!("invalid workflow name: {name}");
    }
    Ok(())
}

fn resolve_workflow(
    workspace: &Path,
    home: &Path,
    plugin_roots: &HashMap<String, PathBuf>,
    name: &str,
    requested_path: Option<&str>,
) -> Result<PathBuf> {
    if let Some(requested_path) = requested_path {
        return resolve_exact_workflow(workspace, home, plugin_roots, requested_path);
    }

    let filename = format!("{name}.js");
    let mut candidates = vec![
        home.join("workflows").join(&filename),
        workspace.join(".phi/workflows").join(&filename),
    ];
    let mut roots = plugin_roots.values().collect::<Vec<_>>();
    roots.sort();
    candidates.extend(
        roots
            .into_iter()
            .map(|root| root.join("workflows").join(&filename)),
    );
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.canonicalize())
        .transpose()?
        .with_context(|| format!("workflow not found: {name}"))
}

fn inspect_workflow(
    plugin: &Path,
    workflow_path: &Path,
    name: &str,
    args: Option<&Value>,
) -> Result<Value> {
    let inspector = plugin.join("runner/workflow-inspect.mjs");
    if !inspector.is_file() {
        bail!("dynamic workflow inspector is missing");
    }
    let mut child = StdCommand::new("node")
        .arg(&inspector)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start workflow metadata inspector")?;
    let request = json!({
        "workflowPath": workflow_path,
        "name": name,
        "args": args.cloned().unwrap_or(Value::Null),
        "validateArgs": args.is_some(),
    });
    serde_json::to_writer(
        child
            .stdin
            .as_mut()
            .context("open workflow inspector stdin")?,
        &request,
    )?;
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .context("wait for workflow metadata inspector")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        let message = error.lines().next().unwrap_or("workflow inspection failed");
        bail!("inspect workflow {}: {message}", workflow_path.display());
    }
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("decode workflow metadata for {}", workflow_path.display()))
}

pub(crate) fn discovery_help(
    workspace: &Path,
    home: &Path,
    plugin_roots: &HashMap<String, PathBuf>,
) -> String {
    let Some(plugin) = plugin_roots.get("dynamic-workflows") else {
        return String::new();
    };
    let mut selected = HashMap::<String, PathBuf>::new();
    for (_, root) in workflow_roots(workspace, home, plugin_roots) {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        let mut paths = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.is_file() && path.extension().and_then(|value| value.to_str()) == Some("js")
            })
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let Some(name) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if validate_name(name).is_ok() {
                selected.entry(name.to_owned()).or_insert(path);
            }
        }
    }
    let mut selected = selected.into_iter().collect::<Vec<_>>();
    selected.sort_by(|left, right| left.0.cmp(&right.0));
    if selected.is_empty() {
        return String::new();
    }
    let lines = selected
        .into_iter()
        .map(
            |(name, path)| match inspect_workflow(plugin, &path, &name, None) {
                Ok(metadata) => format!(
                    "- {name}: {} inputSchema={}",
                    metadata["description"].as_str().unwrap_or(""),
                    metadata
                        .get("inputSchema")
                        .map(Value::to_string)
                        .unwrap_or_else(|| "<none; arbitrary JSON args>".into())
                ),
                Err(error) => format!("- {name}: unavailable ({error:#})"),
            },
        )
        .collect::<Vec<_>>()
        .join("\n");
    format!("\n\nDiscovered name-only workflows and their declared input schemas:\n{lines}")
}

fn resolve_exact_workflow(
    workspace: &Path,
    home: &Path,
    plugin_roots: &HashMap<String, PathBuf>,
    requested_path: &str,
) -> Result<PathBuf> {
    if requested_path.is_empty() {
        bail!("workflow path must not be empty");
    }
    let requested_path = Path::new(requested_path);
    if requested_path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        bail!("workflow path traversal is not allowed");
    }
    if requested_path
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("js")
    {
        bail!("workflow path must have a .js extension");
    }
    let candidate = if requested_path.is_absolute() {
        requested_path.to_owned()
    } else {
        workspace.join(requested_path)
    };
    let candidate = candidate
        .canonicalize()
        .with_context(|| format!("workflow path does not exist: {}", candidate.display()))?;
    if !candidate.is_file() {
        bail!(
            "workflow path is not a regular file: {}",
            candidate.display()
        );
    }

    let allowed = workflow_roots(workspace, home, plugin_roots)
        .into_iter()
        .filter_map(|(scope, root)| canonical_workflow_root(&scope, &root))
        .any(|root| candidate.starts_with(root));
    if !allowed {
        bail!(
            "workflow path is outside global, workspace, and loaded plugin workflow roots: {}",
            candidate.display()
        );
    }
    Ok(candidate)
}

fn workflow_roots(
    workspace: &Path,
    home: &Path,
    plugin_roots: &HashMap<String, PathBuf>,
) -> Vec<(PathBuf, PathBuf)> {
    let mut roots = vec![
        (home.to_owned(), home.join("workflows")),
        (workspace.to_owned(), workspace.join(".phi/workflows")),
    ];
    let mut plugins = plugin_roots.values().collect::<Vec<_>>();
    plugins.sort();
    roots.extend(
        plugins
            .into_iter()
            .map(|plugin| (plugin.clone(), plugin.join("workflows"))),
    );
    roots
}

fn canonical_workflow_root(scope: &Path, root: &Path) -> Option<PathBuf> {
    let relative = root.strip_prefix(scope).ok()?;
    let mut current = scope.to_owned();
    for component in relative.components() {
        current.push(component);
        if fs::symlink_metadata(&current)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
    }
    let scope = scope.canonicalize().ok()?;
    let root = root.canonicalize().ok()?;
    root.starts_with(scope).then_some(root)
}

fn task_id(arguments: &Value) -> Result<&str> {
    let id = arguments
        .get("task_id")
        .and_then(Value::as_str)
        .context("task_id is required")?;
    Uuid::parse_str(id).context("invalid task_id")?;
    Ok(id)
}

fn terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

fn now_ms() -> Result<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    phi_core::write_json_atomic(path, value, phi_core::AtomicWriteMode::Overwrite)
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path)?).with_context(|| format!("read {}", path.display()))
}

fn read_optional_json(path: &Path) -> Result<Value> {
    if path.is_file() {
        read_json(path)
    } else {
        Ok(Value::Null)
    }
}

fn read_tail(path: &Path, max_bytes: usize) -> Result<String> {
    if !path.is_file() {
        return Ok(String::new());
    }
    let bytes = fs::read(path)?;
    let start = bytes.len().saturating_sub(max_bytes);
    Ok(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

async fn terminate(entry: &TaskEntry) {
    let mut child = entry.child.lock().await;
    #[cfg(unix)]
    if let Some(id) = child.id() {
        use nix::{
            sys::signal::{Signal, killpg},
            unistd::Pid,
        };
        let group = Pid::from_raw(id as i32);
        let _ = killpg(group, Signal::SIGTERM);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = killpg(group, Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialize_repository(root: &Path) -> String {
        git_output(root, &["init", "-q"]).unwrap();
        git_output(root, &["config", "user.name", "Phi Test"]).unwrap();
        git_output(root, &["config", "user.email", "phi@example.invalid"]).unwrap();
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git_output(root, &["add", "."]).unwrap();
        git_output(root, &["commit", "-qm", "base"]).unwrap();
        git_output(root, &["rev-parse", "HEAD"]).unwrap()
    }

    #[test]
    fn workflow_names_are_single_safe_components() {
        for name in ["review-changes", ".hidden", "with.dot", "with_underscore"] {
            assert!(validate_name(name).is_ok(), "expected valid name: {name:?}");
        }
        for name in ["", ".", "..", "../escape", "nested/name", "nested\\name"] {
            assert!(
                validate_name(name).is_err(),
                "expected invalid name: {name:?}"
            );
        }
    }

    #[test]
    fn resolves_workflows_in_discovery_order() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let plugins_root = tempfile::tempdir().unwrap();
        let first_plugin = plugins_root.path().join("a-plugin");
        let second_plugin = plugins_root.path().join("z-plugin");
        let workspace_workflow = workspace.path().join(".phi/workflows/.hidden.js");
        let home_workflow = home.path().join("workflows/.hidden.js");
        let first_plugin_workflow = first_plugin.join("workflows/.hidden.js");
        let second_plugin_workflow = second_plugin.join("workflows/.hidden.js");
        for path in [
            &workspace_workflow,
            &home_workflow,
            &first_plugin_workflow,
            &second_plugin_workflow,
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "export default async () => null").unwrap();
        }
        let plugins = HashMap::from([
            ("z-plugin".into(), second_plugin),
            ("a-plugin".into(), first_plugin),
        ]);

        assert_eq!(
            resolve_workflow(workspace.path(), home.path(), &plugins, ".hidden", None).unwrap(),
            home_workflow.canonicalize().unwrap()
        );
        fs::remove_file(&home_workflow).unwrap();
        assert_eq!(
            resolve_workflow(workspace.path(), home.path(), &plugins, ".hidden", None).unwrap(),
            workspace_workflow.canonicalize().unwrap()
        );
        fs::remove_file(&workspace_workflow).unwrap();
        assert_eq!(
            resolve_workflow(workspace.path(), home.path(), &plugins, ".hidden", None).unwrap(),
            first_plugin_workflow.canonicalize().unwrap()
        );
    }

    #[test]
    fn exact_workflow_paths_are_resolved_and_contained() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let plugins_root = tempfile::tempdir().unwrap();
        let loaded_plugin = plugins_root.path().join("loaded");
        let unloaded_plugin = plugins_root.path().join("unloaded");
        let outside = tempfile::tempdir().unwrap();
        let workspace_workflow = workspace.path().join(".phi/workflows/review.js");
        let home_workflow = home.path().join("workflows/review.js");
        let loaded_workflow = loaded_plugin.join("workflows/review.js");
        let unloaded_workflow = unloaded_plugin.join("workflows/review.js");
        let outside_workflow = outside.path().join("review.js");
        for path in [
            &workspace_workflow,
            &home_workflow,
            &loaded_workflow,
            &unloaded_workflow,
            &outside_workflow,
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "export default async () => null").unwrap();
        }
        let plugins = HashMap::from([("loaded".into(), loaded_plugin)]);

        assert_eq!(
            resolve_workflow(
                workspace.path(),
                home.path(),
                &plugins,
                "review",
                Some(".phi/workflows/review.js"),
            )
            .unwrap(),
            workspace_workflow.canonicalize().unwrap()
        );
        for path in [&home_workflow, &loaded_workflow] {
            assert_eq!(
                resolve_workflow(
                    workspace.path(),
                    home.path(),
                    &plugins,
                    "review",
                    Some(path.to_str().unwrap()),
                )
                .unwrap(),
                path.canonicalize().unwrap()
            );
        }

        let directory = workspace.path().join(".phi/workflows/directory.js");
        fs::create_dir_all(&directory).unwrap();
        let wrong_extension = workspace.path().join(".phi/workflows/review.mjs");
        fs::write(&wrong_extension, "").unwrap();
        for path in [
            "".to_owned(),
            "missing.js".to_owned(),
            ".phi/workflows/directory.js".to_owned(),
            ".phi/workflows/review.mjs".to_owned(),
            ".phi/workflows/../workflows/review.js".to_owned(),
            outside_workflow.to_string_lossy().into_owned(),
            unloaded_workflow.to_string_lossy().into_owned(),
        ] {
            assert!(
                resolve_workflow(
                    workspace.path(),
                    home.path(),
                    &plugins,
                    "review",
                    Some(&path),
                )
                .is_err(),
                "expected exact path to be rejected: {path}"
            );
        }

        #[cfg(unix)]
        {
            let symlink = workspace.path().join(".phi/workflows/escape.js");
            std::os::unix::fs::symlink(&outside_workflow, &symlink).unwrap();
            assert!(
                resolve_workflow(
                    workspace.path(),
                    home.path(),
                    &plugins,
                    "review",
                    Some(".phi/workflows/escape.js"),
                )
                .is_err()
            );

            let symlinked_root_workspace = tempfile::tempdir().unwrap();
            fs::create_dir(symlinked_root_workspace.path().join(".phi")).unwrap();
            let escaped = symlinked_root_workspace.path().join("root-escape.js");
            fs::write(&escaped, "export default async () => null").unwrap();
            std::os::unix::fs::symlink(
                symlinked_root_workspace.path(),
                symlinked_root_workspace.path().join(".phi/workflows"),
            )
            .unwrap();
            assert!(
                resolve_workflow(
                    symlinked_root_workspace.path(),
                    home.path(),
                    &plugins,
                    "review",
                    Some(".phi/workflows/root-escape.js"),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn dynamic_workflow_skill_documents_global_first_lookup_and_promotion() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let skill = fs::read_to_string(
            root.join("plugins/dynamic-workflows/skills/dynamic-workflows/SKILL.md"),
        )
        .unwrap();
        assert!(skill.contains("inspect global workflows first"));
        assert!(skill.contains("global → workspace → plugin precedence"));
        assert!(skill.contains("user explicitly asks to make one global"));
        assert!(skill.contains("$PHI_HOME/sessions/<parent-session-id>/workflows/<task-id>/"));
        assert!(skill.contains("child sessions and run records remain durable"));
        assert!(skill.contains("meta.inputSchema"));
        assert!(skill.contains("before task, runner, or child creation"));
        assert!(skill.contains("Unsupported keywords"));
    }

    #[test]
    fn captures_immutable_git_context_from_repository_subdirectory() {
        let repository = tempfile::tempdir().unwrap();
        let starting_commit = initialize_repository(repository.path());
        let workspace = repository.path().join("nested/workspace");
        fs::create_dir_all(&workspace).unwrap();

        let context = git_context(&workspace).unwrap();
        assert_eq!(context.repo_root, repository.path().canonicalize().unwrap());
        assert_eq!(context.starting_commit, starting_commit);
        assert_eq!(context.workspace_relative, Path::new("nested/workspace"));
        assert_eq!(context.repo_hash.len(), 12);

        fs::write(repository.path().join("tracked.txt"), "later\n").unwrap();
        git_output(repository.path(), &["commit", "-qam", "later"]).unwrap();
        assert_ne!(
            context.starting_commit,
            git_output(repository.path(), &["rev-parse", "HEAD"]).unwrap()
        );
    }

    #[test]
    fn non_git_workspace_has_no_git_context() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(git_context(workspace.path()).is_none());
    }

    #[test]
    fn unborn_git_repository_does_not_block_unbranched_workflows() {
        let workspace = tempfile::tempdir().unwrap();
        git_output(workspace.path(), &["init", "-q"]).unwrap();
        assert!(git_context(workspace.path()).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fallback_cleanup_removes_only_manifest_owned_worktrees_and_refs() {
        let repository = tempfile::tempdir().unwrap();
        let starting_commit = initialize_repository(repository.path());
        let task_root = tempfile::tempdir().unwrap();
        let task_id = "55555555-5555-4555-8555-555555555555";
        let task_dir = task_root.path().join(task_id);
        let worktree_root = task_root.path().join("owned").join(task_id);
        let worktree = worktree_root.join("feature-deadbeef");
        let branch = "phi/55555555/feature-deadbeef";
        fs::create_dir_all(&task_dir).unwrap();
        git_output(
            repository.path(),
            &[
                "worktree",
                "add",
                "-b",
                branch,
                worktree.to_str().unwrap(),
                &starting_commit,
            ],
        )
        .unwrap();
        let unrelated = "keep-this-branch";
        git_output(repository.path(), &["branch", unrelated]).unwrap();
        write_json(
            &task_dir.join("request.json"),
            &json!({
                "taskId": task_id,
                "git": {
                    "repoRoot": repository.path().canonicalize().unwrap(),
                    "gitCommonDir": repository.path().join(".git").canonicalize().unwrap()
                },
                "worktreeRoot": worktree_root,
            }),
        )
        .unwrap();
        write_json(
            &task_dir.join("worktrees.json"),
            &json!({
                "version": 1,
                "taskId": task_id,
                "repoRoot": repository.path().canonicalize().unwrap(),
                "worktreeRoot": worktree_root,
                "rootOwned": true,
                "entries": [{
                    "logicalBranch": "feature",
                    "branch": branch,
                    "path": worktree,
                    "state": "active"
                }]
            }),
        )
        .unwrap();

        cleanup_owned_worktrees(&task_dir).await.unwrap();
        assert!(!worktree.exists());
        assert!(!worktree_root.exists());
        assert!(
            git_output(
                repository.path(),
                &["show-ref", "--verify", &format!("refs/heads/{branch}")]
            )
            .is_err()
        );
        assert!(
            git_output(
                repository.path(),
                &["show-ref", "--verify", &format!("refs/heads/{unrelated}")]
            )
            .is_ok()
        );
        let manifest = read_json(&task_dir.join("worktrees.json")).unwrap();
        assert_eq!(manifest["entries"][0]["state"], "cleaned");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_task_reconciliation_marks_task_failed() {
        let session = tempfile::tempdir().unwrap();
        let task_id = "88888888-8888-4888-8888-888888888888";
        let task_dir = session.path().join("workflows").join(task_id);
        fs::create_dir_all(&task_dir).unwrap();
        write_json(
            &task_dir.join("request.json"),
            &json!({ "taskId": task_id, "name": "stale-test" }),
        )
        .unwrap();
        write_json(
            &task_dir.join("state.json"),
            &json!({
                "taskId": task_id,
                "workflow": "stale-test",
                "status": "running",
                "startedAt": 1,
            }),
        )
        .unwrap();

        let output = WorkflowTasks::default()
            .output(session.path(), &json!({ "task_id": task_id, "wait_ms": 0 }))
            .await
            .unwrap();
        assert_eq!(output["status"], "failed");
        assert!(output["state"]["error"].as_str().unwrap().contains("stale"));
    }

    #[test]
    fn node_managed_worktree_tests_pass() {
        if StdCommand::new("node").arg("--version").output().is_err() {
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let status = StdCommand::new("node")
            .args([
                "--test",
                "plugins/dynamic-workflows/runner/worktrees.test.mjs",
                "plugins/dynamic-workflows/runner/workflow-module.test.mjs",
                "plugins/dynamic-workflows/runner/workflow-runner.test.mjs",
            ])
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn launches_and_inspects_a_leading_dot_workflow() {
        if Command::new("node")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session = home.path().join("sessions/test");
        fs::create_dir_all(&session).unwrap();
        let workflow = workspace.path().join(".phi/workflows/.hidden.js");
        fs::create_dir_all(workflow.parent().unwrap()).unwrap();
        fs::write(
            workflow,
            r#"
                export const meta = { name: ".hidden", description: "test workflow" }
                export default async function ({ args }) { return args }
            "#,
        )
        .unwrap();
        let mut plugins = HashMap::new();
        plugins.insert(
            "dynamic-workflows".into(),
            root.join("plugins/dynamic-workflows"),
        );
        let tasks = WorkflowTasks::default();
        let launched = tasks
            .launch(
                workspace.path(),
                home.path(),
                "11111111-1111-4111-8111-111111111111",
                &session,
                &plugins,
                &json!({ "name": ".hidden", "args": ["arbitrary", 7] }),
            )
            .await
            .unwrap();
        let task_id = launched["task_id"].as_str().unwrap();
        let request: Value = serde_json::from_slice(
            &fs::read(session.join("workflows").join(task_id).join("request.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            request["limits"],
            json!({
                "maxConcurrency": MAX_CONCURRENCY,
                "maxAgents": MAX_AGENTS,
            })
        );
        let output = tasks
            .output(&session, &json!({ "task_id": task_id }))
            .await
            .unwrap();
        assert_eq!(output["status"], "completed");
        assert_eq!(output["workflow"], ".hidden");
        assert_eq!(output["result"]["value"], json!(["arbitrary", 7]));
        assert!(
            output["progress"]
                .as_str()
                .unwrap()
                .contains("workflow_started")
        );
        tasks.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn declared_input_schemas_validate_before_launch_and_are_discoverable() {
        if Command::new("node")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session = home.path().join("sessions/test");
        fs::create_dir_all(&session).unwrap();
        let workflow = workspace.path().join(".phi/workflows/review.js");
        fs::create_dir_all(workflow.parent().unwrap()).unwrap();
        fs::write(
            &workflow,
            r#"
                export const meta = {
                  name: "review",
                  description: "review selected files",
                  inputSchema: {
                    type: "object",
                    properties: {
                      request: {
                        type: "object",
                        properties: {
                          files: { type: "array", items: { type: "string", minLength: 2 } }
                        },
                        required: ["files"],
                        additionalProperties: false
                      }
                    },
                    required: ["request"],
                    additionalProperties: false
                  }
                }
                export default async function ({ args }) { return args }
            "#,
        )
        .unwrap();
        let plugins = HashMap::from([(
            "dynamic-workflows".into(),
            root.join("plugins/dynamic-workflows"),
        )]);
        let tasks = WorkflowTasks::default();

        let help = discovery_help(workspace.path(), home.path(), &plugins);
        assert!(help.contains("review selected files"));
        assert!(help.contains("\"minLength\":2"));

        let error = tasks
            .launch(
                workspace.path(),
                home.path(),
                "11111111-1111-4111-8111-111111111111",
                &session,
                &plugins,
                &json!({ "name": "review", "args": { "request": { "files": [""] } } }),
            )
            .await
            .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("args at /request/files/0"), "{error}");
        assert!(
            error.contains("input schema at /properties/request/properties/files/items/minLength"),
            "{error}"
        );
        assert!(!session.join("workflows").exists());

        let args = json!({ "request": { "files": ["ok"] } });
        let by_name = tasks
            .launch(
                workspace.path(),
                home.path(),
                "11111111-1111-4111-8111-111111111111",
                &session,
                &plugins,
                &json!({ "name": "review", "args": args }),
            )
            .await
            .unwrap();
        let by_path = tasks
            .launch(
                workspace.path(),
                home.path(),
                "11111111-1111-4111-8111-111111111111",
                &session,
                &plugins,
                &json!({
                    "name": "review",
                    "path": ".phi/workflows/review.js",
                    "args": args
                }),
            )
            .await
            .unwrap();
        assert_eq!(by_name["description"], "review selected files");
        assert_eq!(by_name["input_schema"], meta_schema(&workflow));
        assert_eq!(by_path["input_schema"], by_name["input_schema"]);
        for launch in [&by_name, &by_path] {
            let output = tasks
                .output(
                    &session,
                    &json!({ "task_id": launch["task_id"], "wait_ms": null }),
                )
                .await
                .unwrap();
            assert_eq!(output["status"], "completed");
            assert_eq!(output["result"]["value"], args);
        }
        tasks.shutdown().await;
    }

    fn meta_schema(workflow: &Path) -> Value {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let plugin = root.join("plugins/dynamic-workflows");
        inspect_workflow(&plugin, workflow, "review", None).unwrap()["inputSchema"].clone()
    }

    #[test]
    fn invalid_and_unsupported_declared_schemas_fail_with_paths() {
        if StdCommand::new("node").arg("--version").output().is_err() {
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let plugin = root.join("plugins/dynamic-workflows");
        let directory = tempfile::tempdir().unwrap();
        let workflow = directory.path().join("invalid.js");
        for (schema, expected) in [
            ("{ type: 'array', minItems: -1 }", "/minItems"),
            (
                "{ type: 'object', properties: { item: { $ref: '#' } } }",
                "/properties/item/$ref",
            ),
        ] {
            fs::write(
                &workflow,
                format!(
                    "export const meta = {{ name: 'invalid', description: 'invalid', inputSchema: {schema} }}; export default async () => null"
                ),
            )
            .unwrap();
            let error = inspect_workflow(&plugin, &workflow, "invalid", None).unwrap_err();
            let error = format!("{error:#}");
            assert!(error.contains("invalid workflow input schema"), "{error}");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exact_paths_launch_same_named_workflows_independently() {
        if Command::new("node")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let plugins_root = tempfile::tempdir().unwrap();
        let session = home.path().join("sessions/test");
        fs::create_dir_all(&session).unwrap();
        let global = home.path().join("workflows/same.js");
        let workspace_definition = workspace.path().join(".phi/workflows/same.js");
        let first_plugin = plugins_root.path().join("first");
        let second_plugin = plugins_root.path().join("second");
        let first_plugin_definition = first_plugin.join("workflows/same.js");
        let second_plugin_definition = second_plugin.join("workflows/same.js");
        for (path, value) in [
            (&global, "global"),
            (&workspace_definition, "workspace"),
            (&first_plugin_definition, "first-plugin"),
            (&second_plugin_definition, "second-plugin"),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!(
                    r#"
                        export const meta = {{ name: "same", description: "test workflow" }}
                        export default async function () {{ return "{value}" }}
                    "#
                ),
            )
            .unwrap();
        }
        let plugins = HashMap::from([
            (
                "dynamic-workflows".into(),
                root.join("plugins/dynamic-workflows"),
            ),
            ("first".into(), first_plugin),
            ("second".into(), second_plugin),
        ]);
        let tasks = WorkflowTasks::default();
        let requests = [
            (global.to_string_lossy().into_owned(), "global", &global),
            (
                ".phi/workflows/same.js".to_owned(),
                "workspace",
                &workspace_definition,
            ),
            (
                first_plugin_definition.to_string_lossy().into_owned(),
                "first-plugin",
                &first_plugin_definition,
            ),
            (
                second_plugin_definition.to_string_lossy().into_owned(),
                "second-plugin",
                &second_plugin_definition,
            ),
        ];
        let mut task_ids = std::collections::HashSet::new();
        for (path, expected, source) in requests {
            let launched = tasks
                .launch(
                    workspace.path(),
                    home.path(),
                    "11111111-1111-4111-8111-111111111111",
                    &session,
                    &plugins,
                    &json!({ "name": "same", "path": path, "args": {} }),
                )
                .await
                .unwrap();
            let task_id = launched["task_id"].as_str().unwrap();
            assert!(task_ids.insert(task_id.to_owned()));
            let task_dir = session.join("workflows").join(task_id);
            let request = read_json(&task_dir.join("request.json")).unwrap();
            assert_eq!(
                PathBuf::from(request["workflowPath"].as_str().unwrap()),
                source.canonicalize().unwrap()
            );
            let output = tasks
                .output(&session, &json!({ "task_id": task_id }))
                .await
                .unwrap();
            assert_eq!(output["status"], "completed");
            assert_eq!(output["result"]["value"], expected);
            assert!(task_dir.is_dir());
        }
        assert_eq!(task_ids.len(), 4);
        tasks.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn workflow_scheduling_limits_parallelism_and_preserves_batch_barriers() {
        if Command::new("node")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session = home.path().join("sessions/test");
        fs::create_dir_all(&session).unwrap();
        let mut plugins = HashMap::new();
        plugins.insert(
            "dynamic-workflows".into(),
            root.join("plugins/dynamic-workflows"),
        );
        let tasks = WorkflowTasks::default();
        let launched = tasks
            .launch(
                workspace.path(),
                home.path(),
                "11111111-1111-4111-8111-111111111111",
                &session,
                &plugins,
                &json!({ "name": "scheduling-example" }),
            )
            .await
            .unwrap();
        let task_id = launched["task_id"].as_str().unwrap();
        let output = tasks
            .output(&session, &json!({ "task_id": task_id, "wait_ms": 10_000 }))
            .await
            .unwrap();
        assert_eq!(output["status"], "completed");
        assert_eq!(output["workflow"], "scheduling-example");
        assert_eq!(output["summary"]["phase"], "Fixed-size batches");
        assert_eq!(output["summary"]["latestLog"], "Batch tasks completed");

        let result = &output["result"]["value"];
        assert_eq!(result["parallel"]["maximum"], 2);
        assert_eq!(
            result["parallel"]["results"],
            json!(["p1", "p2", "p3", "p4"])
        );
        assert_eq!(result["batch"]["maximum"], 2);
        assert_eq!(result["batch"]["results"], json!(["b1", "b2", "b3", "b4"]));

        let batch_events = result["batch"]["events"].as_array().unwrap();
        let first_batch_end = batch_events
            .iter()
            .position(|event| event == "end:b1")
            .unwrap()
            .max(
                batch_events
                    .iter()
                    .position(|event| event == "end:b2")
                    .unwrap(),
            );
        let second_batch_start = batch_events
            .iter()
            .position(|event| event == "start:b3")
            .unwrap();
        assert!(second_batch_start > first_batch_end);
        tasks.shutdown().await;
    }
}
