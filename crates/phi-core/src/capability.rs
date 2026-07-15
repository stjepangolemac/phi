use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
pub use phi_protocol::ToolSpec;
use serde_json::{Value, json};

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn execute(&self, workspace: &Path, arguments: Value) -> Result<Value>;
    fn parallel_safe(&self) -> bool {
        false
    }
}

#[derive(Default)]
pub struct Registry {
    tools: BTreeMap<String, RegisteredTool>,
}

struct RegisteredTool {
    tool: Box<dyn Tool>,
    exposed: bool,
}

impl Registry {
    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.insert(tool, true);
    }

    pub fn register_hidden(&mut self, tool: impl Tool + 'static) {
        self.insert(tool, false);
    }

    fn insert(&mut self, tool: impl Tool + 'static, exposed: bool) {
        self.tools.insert(
            tool.spec().name,
            RegisteredTool {
                tool: Box::new(tool),
                exposed,
            },
        );
    }

    pub fn execute(&self, workspace: &Path, name: &str, arguments: Value) -> Result<Value> {
        self.tools
            .get(name)
            .with_context(|| format!("unknown tool: {name}"))?
            .tool
            .execute(workspace, arguments)
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .filter(|entry| entry.exposed)
            .map(|entry| entry.tool.spec())
            .collect()
    }

    pub fn parallel_safe(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .is_some_and(|entry| entry.tool.parallel_safe())
    }
}

pub struct ReadFile {
    pub full_access: bool,
    pub additional_root: Option<PathBuf>,
}

const DEFAULT_READ_LINES: usize = 200;
const MAX_READ_LINES: usize = 1_000;
const MAX_READ_BYTES: usize = 16 * 1024;

impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: if self.full_access {
                "Read a bounded range of complete UTF-8 lines from any file. Use next_line to continue."
            } else if self.additional_root.is_some() {
                "Read a bounded range of complete UTF-8 lines inside the workspace or Phi home. Use next_line to continue."
            } else {
                "Read a bounded range of complete UTF-8 lines inside the workspace. Use next_line to continue."
            }
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": ["integer", "null"], "minimum": 1, "description": "First line to read. Use null for line 1." },
                    "line_count": { "type": ["integer", "null"], "minimum": 1, "maximum": 1000, "description": "Maximum lines to read. Use null for 200." }
                },
                "required": ["path", "start_line", "line_count"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, workspace: &Path, arguments: Value) -> Result<Value> {
        let requested = arguments
            .get("path")
            .and_then(Value::as_str)
            .context("read_file requires a path")?;
        let root = fs::canonicalize(workspace)?;
        let path = fs::canonicalize(root.join(requested))?;
        let in_additional_root = self
            .additional_root
            .as_ref()
            .and_then(|root| fs::canonicalize(root).ok())
            .is_some_and(|root| path.starts_with(root));
        if !self.full_access && !path.starts_with(&root) && !in_additional_root {
            bail!("path is outside allowed roots");
        }

        let start_line = arguments
            .get("start_line")
            .filter(|value| !value.is_null())
            .map(|value| value.as_u64().context("start_line must be an integer"))
            .transpose()?
            .unwrap_or(1);
        if start_line == 0 {
            bail!("start_line must be at least 1");
        }

        let line_count = arguments
            .get("line_count")
            .filter(|value| !value.is_null())
            .map(|value| value.as_u64().context("line_count must be an integer"))
            .transpose()?
            .unwrap_or(DEFAULT_READ_LINES as u64);
        if !(1..=MAX_READ_LINES as u64).contains(&line_count) {
            bail!("line_count must be between 1 and {MAX_READ_LINES}");
        }

        let bytes = fs::read(&path)?;
        let content = std::str::from_utf8(&bytes).context("file is not UTF-8")?;
        let lines: Vec<_> = content.split_inclusive('\n').collect();
        let first = usize::try_from(start_line - 1)
            .unwrap_or(usize::MAX)
            .min(lines.len());
        let mut selected = String::new();
        let mut lines_read = 0;

        for (offset, line) in lines[first..].iter().take(line_count as usize).enumerate() {
            if line.len() > MAX_READ_BYTES {
                bail!(
                    "line {} exceeds the {MAX_READ_BYTES}-byte read limit",
                    first + offset + 1
                );
            }
            if selected.len() + line.len() > MAX_READ_BYTES {
                break;
            }
            selected.push_str(line);
            lines_read += 1;
        }

        let end_index = first + lines_read;
        let end_line = (lines_read > 0).then_some(end_index);
        let next_line = (end_index < lines.len()).then_some(end_index + 1);
        let display_path = path.strip_prefix(&root).map_or_else(
            |_| path.display().to_string(),
            |path| path.display().to_string(),
        );
        Ok(json!({
            "path": display_path,
            "content": selected,
            "start_line": start_line,
            "end_line": end_line,
            "next_line": next_line,
            "total_lines": lines.len(),
            "revision": blake3::hash(&bytes).to_hex().to_string(),
        }))
    }

    fn parallel_safe(&self) -> bool {
        true
    }
}

pub fn exec_command_spec() -> ToolSpec {
    ToolSpec {
        name: "exec_command".into(),
        description: "Run a command in a Phi-managed process. For background or long-running work, run the command directly: never use nohup, &, or disown. Set yield_time_ms to 0 to return a managed session immediately. Once a session_id is returned, the command is already running; do not start it again. Use write_stdin to poll or interact with it and list_processes to inspect it. Managed processes stop when Phi exits.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "Shell command to execute." },
                "workdir": { "type": ["string", "null"], "description": "Working directory relative to the workspace, or an absolute path in --yolo mode. Use null for the workspace root." },
                "shell": { "type": ["string", "null"], "description": "Shell binary. Use null for the user's shell." },
                "login": { "type": ["boolean", "null"], "description": "Use login-shell semantics. Use null for false." },
                "tty": { "type": ["boolean", "null"], "description": "Allocate a pseudo-terminal only for genuinely interactive programs. Use null for false." },
                "yield_time_ms": { "type": ["integer", "null"], "minimum": 0, "maximum": 30000, "description": "Milliseconds to wait before returning a running session. Use 0 for intentional background work; use null for 10000." },
                "max_output_tokens": { "type": ["integer", "null"], "minimum": 1, "maximum": 100000, "description": "Approximate output token budget. Use null for 10000." }
            },
            "required": ["cmd", "workdir", "shell", "login", "tty", "yield_time_ms", "max_output_tokens"],
            "additionalProperties": false
        }),
    }
}

pub fn reload_config_spec() -> ToolSpec {
    ToolSpec {
        name: "reload_config".into(),
        description: "Reload Phi's current configuration and plugins into this conversation after changing them. The reload is validated before it becomes active.".into(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    }
}

pub fn write_stdin_spec() -> ToolSpec {
    ToolSpec {
        name: "write_stdin".into(),
        description: "Write to or poll a managed exec_command session. Use null or an empty chars value to wait for new output. Do not start the command again after receiving a session_id.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "integer", "minimum": 1 },
                "chars": { "type": ["string", "null"], "description": "Characters to write. Use null or an empty string to poll." },
                "yield_time_ms": { "type": ["integer", "null"], "minimum": 1, "maximum": 300000, "description": "Milliseconds to wait for output or process completion. Use null for 15000 when polling, or 250 after sending input." },
                "max_output_tokens": { "type": ["integer", "null"], "minimum": 1, "maximum": 100000 }
            },
            "required": ["session_id", "chars", "yield_time_ms", "max_output_tokens"],
            "additionalProperties": false
        }),
    }
}

pub fn list_processes_spec() -> ToolSpec {
    ToolSpec {
        name: "list_processes".into(),
        description: "List shell processes managed by the current Phi session, including IDs, status, commands, and recent output.".into(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    }
}

pub fn terminate_process_spec() -> ToolSpec {
    ToolSpec {
        name: "terminate_process".into(),
        description: "Gracefully terminate one managed background process. Phi sends SIGINT, then SIGTERM, then SIGKILL if the process does not exit.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "integer", "minimum": 1 }
            },
            "required": ["session_id"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_dispatches_without_defining_an_editing_strategy() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let mut registry = Registry::default();
        registry.register(ReadFile {
            full_access: false,
            additional_root: None,
        });
        let result = registry
            .execute(dir.path(), "read_file", json!({ "path": "a.txt" }))
            .unwrap();
        assert_eq!(result["content"], "hello");
        assert_eq!(result["start_line"], 1);
        assert_eq!(result["end_line"], 1);
        assert_eq!(result["next_line"], Value::Null);
        assert_eq!(result["total_lines"], 1);
        assert!(
            registry
                .execute(dir.path(), "hash_edit", json!({}))
                .is_err()
        );
    }

    #[test]
    fn read_file_paginates_with_default_and_explicit_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (1..=250).map(|line| format!("line {line}\n")).collect();
        fs::write(dir.path().join("lines.txt"), content).unwrap();
        let tool = ReadFile {
            full_access: false,
            additional_root: None,
        };

        let first = tool
            .execute(dir.path(), json!({ "path": "lines.txt" }))
            .unwrap();
        assert_eq!(first["end_line"], 200);
        assert_eq!(first["next_line"], 201);
        assert_eq!(first["total_lines"], 250);

        let second = tool
            .execute(
                dir.path(),
                json!({ "path": "lines.txt", "start_line": 201, "line_count": 100 }),
            )
            .unwrap();
        assert_eq!(second["end_line"], 250);
        assert_eq!(second["next_line"], Value::Null);
        assert!(
            second["content"]
                .as_str()
                .unwrap()
                .starts_with("line 201\n")
        );
    }

    #[test]
    fn read_file_enforces_line_and_byte_limits() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!("{}\n", "x".repeat(8_191));
        fs::write(dir.path().join("large.txt"), line.repeat(3)).unwrap();
        let tool = ReadFile {
            full_access: false,
            additional_root: None,
        };

        let result = tool
            .execute(dir.path(), json!({ "path": "large.txt", "line_count": 3 }))
            .unwrap();
        assert_eq!(result["content"].as_str().unwrap().len(), MAX_READ_BYTES);
        assert_eq!(result["end_line"], 2);
        assert_eq!(result["next_line"], 3);

        assert!(
            tool.execute(
                dir.path(),
                json!({ "path": "large.txt", "line_count": 1001 }),
            )
            .is_err()
        );
        fs::write(dir.path().join("wide.txt"), "x".repeat(MAX_READ_BYTES + 1)).unwrap();
        assert!(
            tool.execute(dir.path(), json!({ "path": "wide.txt" }))
                .is_err()
        );
    }

    #[test]
    fn unrestricted_read_file_accepts_absolute_paths_outside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("outside.txt");
        fs::write(&path, "outside\n").unwrap();

        let restricted = ReadFile {
            full_access: false,
            additional_root: None,
        };
        assert!(
            restricted
                .execute(workspace.path(), json!({ "path": path }))
                .is_err()
        );
        let unrestricted = ReadFile {
            full_access: true,
            additional_root: None,
        };
        let result = unrestricted
            .execute(workspace.path(), json!({ "path": path }))
            .unwrap();
        assert_eq!(result["content"], "outside\n");
        assert_eq!(
            result["path"],
            fs::canonicalize(path).unwrap().display().to_string()
        );
    }
}
