use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, bail};
pub use phi_protocol::ToolSpec;
use serde_json::{Value, json};

pub trait Tool {
    fn spec(&self) -> ToolSpec;
    fn execute(&self, workspace: &Path, arguments: Value) -> Result<Value>;
}

#[derive(Default)]
pub struct Registry {
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl Registry {
    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.spec().name, Box::new(tool));
    }

    pub fn execute(&self, workspace: &Path, name: &str, arguments: Value) -> Result<Value> {
        self.tools
            .get(name)
            .with_context(|| format!("unknown tool: {name}"))?
            .execute(workspace, arguments)
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }
}

pub struct ReadFile;

impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        Self::spec()
    }

    fn execute(&self, workspace: &Path, arguments: Value) -> Result<Value> {
        let relative = arguments
            .get("path")
            .and_then(Value::as_str)
            .context("read_file requires a path")?;
        let root = fs::canonicalize(workspace)?;
        let path = fs::canonicalize(root.join(relative))?;
        if !path.starts_with(&root) {
            bail!("path is outside workspace");
        }
        Ok(json!({
            "path": path.strip_prefix(&root)?.display().to_string(),
            "content": fs::read_to_string(&path)?,
            "revision": blake3::hash(&fs::read(&path)?).to_hex().to_string(),
        }))
    }
}

pub struct ReplaceFile;

impl Tool for ReplaceFile {
    fn spec(&self) -> ToolSpec {
        Self::spec()
    }

    fn execute(&self, workspace: &Path, arguments: Value) -> Result<Value> {
        let relative = arguments
            .get("path")
            .and_then(Value::as_str)
            .context("replace_file requires a path")?;
        let revision = arguments
            .get("revision")
            .and_then(Value::as_str)
            .context("replace_file requires a revision")?;
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .context("replace_file requires content")?;
        let root = fs::canonicalize(workspace)?;
        let path = fs::canonicalize(root.join(relative))?;
        if !path.starts_with(&root) {
            bail!("path is outside workspace");
        }
        let current = fs::read(&path)?;
        if blake3::hash(&current).to_hex().as_str() != revision {
            bail!("stale file revision");
        }
        let permissions = fs::metadata(&path)?.permissions();
        let parent = path.parent().context("file has no parent")?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(&mut temp, content.as_bytes())?;
        temp.as_file().set_permissions(permissions)?;
        temp.persist(&path).map_err(|error| error.error)?;
        Ok(json!({
            "path": path.strip_prefix(&root)?.display().to_string(),
            "revision": blake3::hash(content.as_bytes()).to_hex().to_string(),
        }))
    }
}

impl ReadFile {
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a UTF-8 file inside the workspace.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }
}

impl ReplaceFile {
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: "replace_file".into(),
            description: "Atomically replace an existing UTF-8 workspace file using the revision returned by read_file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "revision": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "revision", "content"],
                "additionalProperties": false
            }),
        }
    }
}

pub fn shell_spec() -> ToolSpec {
    ToolSpec {
        name: "shell".into(),
        description: "Run one allowlisted program in the workspace without shell expansion.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "program": { "type": "string" },
                "args": { "type": "array", "items": { "type": "string" } },
                "stdin": { "type": "string" },
                "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 60000 }
            },
            "required": ["program", "args", "stdin", "timeout_ms"],
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
        registry.register(ReadFile);
        let result = registry
            .execute(dir.path(), "read_file", json!({ "path": "a.txt" }))
            .unwrap();
        assert_eq!(result["content"], "hello");
        assert!(
            registry
                .execute(dir.path(), "hash_edit", json!({}))
                .is_err()
        );
    }

    #[test]
    fn replacement_requires_current_revision() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "old").unwrap();
        let mut registry = Registry::default();
        registry.register(ReadFile);
        registry.register(ReplaceFile);
        let read = registry
            .execute(dir.path(), "read_file", json!({ "path": "a.txt" }))
            .unwrap();
        registry
            .execute(
                dir.path(),
                "replace_file",
                json!({ "path": "a.txt", "revision": read["revision"], "content": "new" }),
            )
            .unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "new");
        assert!(
            registry
                .execute(
                    dir.path(),
                    "replace_file",
                    json!({ "path": "a.txt", "revision": read["revision"], "content": "again" }),
                )
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
    }
}
