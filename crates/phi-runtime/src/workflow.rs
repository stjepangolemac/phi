use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::{process::Command, time::Instant};
use uuid::Uuid;

const MAX_CONCURRENCY: u64 = 8;
const MAX_AGENTS: u64 = 32;
const WORKFLOW_TIMEOUT_MS: u64 = 60 * 60 * 1_000;
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
        session_dir: &Path,
        plugin_roots: &HashMap<String, PathBuf>,
        arguments: &Value,
    ) -> Result<Value> {
        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .context("Workflow requires name")?;
        validate_name(name)?;
        let args = arguments.get("args").cloned().unwrap_or(Value::Null);
        let plugin = plugin_roots
            .get("dynamic-workflows")
            .context("dynamic-workflows plugin is not loaded")?;
        let runner = plugin.join("runner/workflow-runner.mjs");
        if !runner.is_file() {
            bail!("dynamic workflow runner is missing");
        }

        let task_id = Uuid::new_v4().to_string();
        let dir = session_dir.join("workflows/tasks").join(&task_id);
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
        let mut roots = plugin_roots.values().cloned().collect::<Vec<_>>();
        roots.sort();
        let request = json!({
            "taskId": task_id,
            "name": name,
            "args": args,
            "workspace": workspace,
            "home": home,
            "taskDir": dir,
            "pluginDirs": roots,
            "phi": phi,
            "startedAt": started_at,
            "limits": {
                "maxConcurrency": MAX_CONCURRENCY,
                "maxAgents": MAX_AGENTS,
                "timeoutMs": WORKFLOW_TIMEOUT_MS,
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
            .unwrap_or(0)
            .min(300_000);
        let dir = self
            .tasks
            .lock()
            .expect("workflow task registry poisoned")
            .get(task_id)
            .map(|entry| entry.dir.clone())
            .unwrap_or_else(|| session_dir.join("workflows/tasks").join(task_id));
        if !dir.join("state.json").is_file() {
            bail!("workflow task not found: {task_id}");
        }
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            self.reconcile(task_id).await?;
            let state = read_json(&dir.join("state.json"))?;
            let status = state["status"].as_str().unwrap_or("unknown");
            if terminal(status) || Instant::now() >= deadline {
                let result = read_optional_json(&dir.join("result.json"))?;
                let progress = read_tail(&dir.join("progress.jsonl"), MAX_PROGRESS_BYTES)?;
                return Ok(json!({
                    "task_id": task_id,
                    "status": status,
                    "state": state,
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
            .unwrap_or_else(|| session_dir.join("workflows/tasks").join(task_id));
        let Some(entry) = entry else {
            if dir.join("state.json").is_file() {
                return Ok(json!({ "task_id": task_id, "status": "not_running" }));
            }
            bail!("workflow task not found: {task_id}");
        };
        terminate(&entry).await;
        write_json(
            &dir.join("state.json"),
            &json!({
                "taskId": task_id,
                "status": "cancelled",
                "completedAt": now_ms()?,
            }),
        )?;
        self.tasks
            .lock()
            .expect("workflow task registry poisoned")
            .remove(task_id);
        Ok(json!({ "task_id": task_id, "status": "cancelled" }))
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
        }
    }

    async fn reconcile(&self, task_id: &str) -> Result<()> {
        let entry = self
            .tasks
            .lock()
            .expect("workflow task registry poisoned")
            .get(task_id)
            .cloned();
        let Some(entry) = entry else {
            return Ok(());
        };
        let mut child = entry.child.lock().await;
        if let Some(status) = child.try_wait()? {
            let state = read_json(&entry.dir.join("state.json"))?;
            if !terminal(state["status"].as_str().unwrap_or("")) {
                write_json(
                    &entry.dir.join("state.json"),
                    &json!({
                        "taskId": task_id,
                        "status": "failed",
                        "error": format!("workflow runner exited with {status}"),
                        "completedAt": now_ms()?,
                    }),
                )?;
            }
            drop(child);
            self.tasks
                .lock()
                .expect("workflow task registry poisoned")
                .remove(task_id);
        }
        Ok(())
    }
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temporary, path)?;
    Ok(())
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
        if child.try_wait().ok().flatten().is_none() {
            let _ = killpg(group, Signal::SIGKILL);
        }
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

    #[test]
    fn workflow_names_are_single_safe_components() {
        assert!(validate_name("review-changes").is_ok());
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("nested/name").is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn launches_and_inspects_a_plugin_workflow() {
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
        let session = workspace.path().join(".phi/sessions/test");
        fs::create_dir_all(&session).unwrap();
        let mut plugins = HashMap::new();
        plugins.insert(
            "dynamic-workflows".into(),
            root.join("policy/tools/dynamic-workflows"),
        );
        let tasks = WorkflowTasks::default();
        let launched = tasks
            .launch(
                workspace.path(),
                home.path(),
                &session,
                &plugins,
                &json!({ "name": "example", "args": { "ok": true } }),
            )
            .await
            .unwrap();
        let task_id = launched["task_id"].as_str().unwrap();
        let output = tasks
            .output(&session, &json!({ "task_id": task_id, "wait_ms": 10_000 }))
            .await
            .unwrap();
        assert_eq!(output["status"], "completed");
        assert_eq!(output["result"]["value"], json!({ "ok": true }));
        assert!(
            output["progress"]
                .as_str()
                .unwrap()
                .contains("workflow_started")
        );
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
        let session = workspace.path().join(".phi/sessions/test");
        fs::create_dir_all(&session).unwrap();
        let mut plugins = HashMap::new();
        plugins.insert(
            "dynamic-workflows".into(),
            root.join("policy/tools/dynamic-workflows"),
        );
        let tasks = WorkflowTasks::default();
        let launched = tasks
            .launch(
                workspace.path(),
                home.path(),
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
