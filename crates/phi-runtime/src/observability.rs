use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::RuntimeEvent;

#[derive(Clone)]
pub struct Observability {
    inner: Arc<Inner>,
}

struct Inner {
    correlation_id: String,
    sink: Mutex<Sink>,
    context: Mutex<ContextIds>,
    tee_runtime_events: bool,
}

struct Sink {
    writer: Option<Box<dyn Write + Send>>,
    destination: String,
    warned: bool,
}

#[derive(Default)]
struct ContextIds {
    session_id: Option<String>,
    runtime_path: Option<PathBuf>,
}

impl Observability {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(destination) = std::env::var_os("PHI_LOG") else {
            return Ok(None);
        };
        let destination = destination
            .into_string()
            .map_err(|_| anyhow::anyhow!("PHI_LOG is not valid UTF-8"))?;
        if destination.is_empty() {
            bail!("PHI_LOG must be a file path or -");
        }
        let tee_runtime_events = std::env::var("PHI_RUNTIME_EVENTS")
            .ok()
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes"));
        Self::new(&destination, tee_runtime_events).map(Some)
    }

    pub fn new(destination: &str, tee_runtime_events: bool) -> Result<Self> {
        let writer: Box<dyn Write + Send> = if destination == "-" {
            Box::new(io::stdout())
        } else {
            let path = Path::new(destination);
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).with_context(|| {
                    format!("create PHI_LOG parent directory: {}", parent.display())
                })?;
            }
            Box::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("open PHI_LOG sink: {}", path.display()))?,
            )
        };
        Ok(Self {
            inner: Arc::new(Inner {
                correlation_id: Uuid::new_v4().to_string(),
                sink: Mutex::new(Sink {
                    writer: Some(writer),
                    destination: destination.to_owned(),
                    warned: false,
                }),
                context: Mutex::new(ContextIds::default()),
                tee_runtime_events,
            }),
        })
    }

    pub fn bind_session(&self, session_id: &str, session_dir: &Path) {
        let mut context = self.inner.context.lock().expect("log context poisoned");
        context.session_id = Some(session_id.to_owned());
        context.runtime_path = self
            .inner
            .tee_runtime_events
            .then(|| session_dir.join("runtime.jsonl"));
        drop(context);
        self.record("runtime.session_bound", "info", json!({}));
    }

    pub fn record(&self, event: &str, level: &str, fields: Value) {
        let context = self.inner.context.lock().expect("log context poisoned");
        let mut record = Map::from_iter([
            ("timestamp_ms".into(), json!(now_ms())),
            ("level".into(), json!(level)),
            ("event".into(), json!(event)),
            ("correlation_id".into(), json!(self.inner.correlation_id)),
        ]);
        if let Some(session_id) = &context.session_id {
            record.insert("session_id".into(), json!(session_id));
        }
        drop(context);
        if let Some(fields) = fields.as_object() {
            record.extend(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), sanitize_field(key, value))),
            );
        }
        self.write_sink(&Value::Object(record));
    }

    pub fn runtime_event(&self, event: &RuntimeEvent) {
        let path = self
            .inner
            .context
            .lock()
            .expect("log context poisoned")
            .runtime_path
            .clone();
        let Some(path) = path else { return };
        let record = json!({
            "timestamp_ms": now_ms(),
            "correlation_id": self.inner.correlation_id,
            "event": sanitize_runtime_event(event),
        });
        if let Err(error) = append_json(&path, &record) {
            self.disable(format!("write {}: {error:#}", path.display()));
        }
    }

    fn write_sink(&self, record: &Value) {
        let result = (|| -> io::Result<()> {
            let mut sink = self.inner.sink.lock().expect("log sink poisoned");
            let Some(writer) = sink.writer.as_mut() else {
                return Ok(());
            };
            serde_json::to_writer(&mut *writer, record)?;
            writer.write_all(b"\n")?;
            writer.flush()
        })();
        if let Err(error) = result {
            self.disable(error.to_string());
        }
    }

    fn disable(&self, error: String) {
        let mut sink = self.inner.sink.lock().expect("log sink poisoned");
        sink.writer = None;
        if !sink.warned {
            eprintln!(
                "phi: disabling observability sink {} after write failure: {error}",
                sink.destination
            );
            sink.warned = true;
        }
    }
}

fn sanitize_runtime_event(event: &RuntimeEvent) -> Value {
    match event {
        RuntimeEvent::Session { id } => json!({ "type": "session", "id": id }),
        RuntimeEvent::History { messages } => {
            json!({ "type": "history", "message_count": messages.len() })
        }
        RuntimeEvent::UserMessage { content } => {
            json!({ "type": "user_message", "content_bytes": content.len() })
        }
        RuntimeEvent::QueuedMessagesInjected { contents } => json!({
            "type": "queued_messages_injected",
            "message_count": contents.len()
        }),
        RuntimeEvent::ContextUpdated { .. }
        | RuntimeEvent::CatalogUpdated { .. }
        | RuntimeEvent::ActivityChanged { .. }
        | RuntimeEvent::ContextCompactionStatus { .. }
        | RuntimeEvent::ToolRouteSelected { .. } => sanitize_serialized(event),
        RuntimeEvent::ApprovalRequested { name, .. } => json!({
            "type": "approval_requested", "name": name, "detail": "[redacted]"
        }),
        RuntimeEvent::ModelDelta { content } => {
            json!({ "type": "model_delta", "content_bytes": content.len() })
        }
        RuntimeEvent::CommentaryDelta { content } => {
            json!({ "type": "commentary_delta", "content_bytes": content.len() })
        }
        RuntimeEvent::CommentaryStarted => json!({ "type": "commentary_started" }),
        RuntimeEvent::ReasoningSummaryDelta { content } => json!({
            "type": "reasoning_summary_delta",
            "content_bytes": content.len()
        }),
        RuntimeEvent::ToolStarted { call_id, name, .. } => json!({
            "type": "tool_started", "call_id": call_id, "name": name,
            "arguments": "[redacted]"
        }),
        RuntimeEvent::ToolOutput {
            call_id,
            name,
            content,
        } => json!({
            "type": "tool_output", "call_id": call_id, "name": name,
            "content_bytes": content.len()
        }),
        RuntimeEvent::ToolCompleted { call_id, name, .. } => json!({
            "type": "tool_completed", "call_id": call_id, "name": name,
            "result": "[redacted]"
        }),
        RuntimeEvent::Finished { content } => {
            json!({ "type": "finished", "content_bytes": content.len() })
        }
        RuntimeEvent::Error { message } => json!({
            "type": "error", "message": redact_error(message)
        }),
    }
}

fn sanitize_serialized(value: &impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|_| json!({ "type": "serialization_error" }))
}

fn sanitize_field(key: &str, value: &Value) -> Value {
    let key = key.to_ascii_lowercase();
    if [
        "arguments",
        "authorization",
        "body",
        "content",
        "encrypted_content",
        "headers",
        "reasoning",
        "result",
        "secret",
        "stderr",
        "stdin",
        "stdout",
        "token",
    ]
    .iter()
    .any(|sensitive| key.contains(sensitive))
    {
        return json!("[redacted]");
    }
    if matches!(key.as_str(), "error" | "message" | "detail") {
        return value
            .as_str()
            .map(redact_error)
            .map(Value::String)
            .unwrap_or_else(|| json!("[redacted]"));
    }
    value.clone()
}

fn redact_error(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if [
        "authorization",
        "bearer ",
        "api_key",
        "token",
        "secret",
        "encrypted_content",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        "sensitive error details redacted".into()
    } else {
        message.chars().take(512).collect()
    }
}

fn append_json(path: &Path, value: &Value) -> Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_structured_records_and_redacts_runtime_payloads_in_order() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("logs/phi.jsonl");
        let session = root.path().join("session");
        fs::create_dir(&session).unwrap();
        let observer = Observability::new(log.to_str().unwrap(), true).unwrap();
        observer.bind_session("session-1", &session);
        observer.record(
            "provider.attempt",
            "info",
            json!({
                "attempt": 1,
                "task_id": "task-1",
                "authorization": "Bearer private-token",
                "error": "request failed with token private-token",
            }),
        );
        observer.runtime_event(&RuntimeEvent::ToolStarted {
            call_id: "call-1".into(),
            name: "exec_command".into(),
            arguments: json!({ "token": "secret", "cmd": "cat ~/.ssh/id_rsa" }),
        });
        observer.runtime_event(&RuntimeEvent::ApprovalRequested {
            name: "exec_command".into(),
            detail: "approve cat ~/.ssh/id_rsa with secret".into(),
        });
        observer.runtime_event(&RuntimeEvent::Finished {
            content: "private answer".into(),
        });

        let records = fs::read_to_string(log).unwrap();
        assert!(
            records
                .lines()
                .all(|line| serde_json::from_str::<Value>(line).is_ok())
        );
        assert!(records.contains("provider.attempt"));
        assert!(records.contains("session-1"));
        assert!(!records.contains("private-token"));
        let runtime = fs::read_to_string(session.join("runtime.jsonl")).unwrap();
        let values = runtime
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values[0]["event"]["type"], "tool_started");
        assert_eq!(values[1]["event"]["type"], "approval_requested");
        assert_eq!(values[2]["event"]["type"], "finished");
        assert!(!runtime.contains("secret"));
        assert!(!runtime.contains("id_rsa"));
        assert!(!runtime.contains("private answer"));
    }

    #[test]
    fn rejects_empty_paths_and_creates_parent_directories() {
        assert!(Observability::new("", false).is_err());
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested/phi.jsonl");
        Observability::new(path.to_str().unwrap(), false)
            .unwrap()
            .record("test", "info", json!({}));
        assert!(path.is_file());
    }
}
