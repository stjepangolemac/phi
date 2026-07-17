use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    UserMessage {
        content: String,
    },
    CompactRequested,
    ModelSelected {
        model: String,
        reasoning: String,
        service_tier: String,
    },
    ProcessCompleted {
        success: bool,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        stdout_truncated: bool,
        stderr_truncated: bool,
    },
    ToolsCompleted {
        results: Vec<ToolResult>,
    },
    HttpCompleted {
        success: bool,
        status: u16,
        events: Vec<serde_json::Value>,
        error: String,
    },
    ContextCompactionStarted {
        job_id: String,
    },
    ContextCompactionCompleted {
        job_id: String,
        success: bool,
        status: u16,
        events: Vec<serde_json::Value>,
        error: String,
    },
    ContextWaitCompleted {
        call_id: String,
        job_ids: Vec<String>,
    },
    ContextCompactionsCancelled {
        job_ids: Vec<String>,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Effect {
    Process {
        program: String,
        args: Vec<String>,
        stdin: String,
        timeout_ms: u64,
    },
    RunTools {
        calls: Vec<ToolCall>,
    },
    HttpRequest {
        url: String,
        secret: String,
        headers: std::collections::BTreeMap<String, String>,
        body: serde_json::Value,
        timeout_ms: u64,
        #[serde(default)]
        stream: Vec<StreamRule>,
    },
    QueueContextCompaction {
        job_id: String,
        url: String,
        secret: String,
        headers: std::collections::BTreeMap<String, String>,
        body: serde_json::Value,
        timeout_ms: u64,
        #[serde(default)]
        stream: Vec<StreamRule>,
        next: Box<Effect>,
    },
    WaitForContextCompactions {
        call_id: String,
        job_ids: Vec<String>,
    },
    Continue,
    Finish {
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    #[serde(deserialize_with = "deserialize_tool_arguments")]
    pub arguments: serde_json::Value,
    #[serde(flatten)]
    pub execution: ToolExecution,
}

fn deserialize_tool_arguments<'de, D>(deserializer: D) -> Result<serde_json::Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let serde_json::Value::String(raw) = value else {
        return Ok(value);
    };
    Ok(serde_json::from_str(&raw)
        .unwrap_or_else(|_| serde_json::json!({ "malformed_arguments": raw })))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolExecution {
    Direct,
    Http {
        implementation: String,
        parallel: bool,
        url: String,
        secret: String,
        headers: std::collections::BTreeMap<String, String>,
        body: serde_json::Value,
        timeout_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamRule {
    #[serde(rename = "match")]
    pub matches: std::collections::BTreeMap<String, serde_json::Value>,
    pub emit: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyOutput {
    pub state: String,
    pub effects: Vec<Effect>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: String,
    pub usage: String,
    pub description: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelSpec {
    pub provider: String,
    pub id: String,
    pub model: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    pub context_window: u64,
    pub compaction_token_limit: u64,
    #[serde(default)]
    pub strict_json_schema_capable: bool,
    #[serde(default)]
    pub function_tools: bool,
    #[serde(default)]
    pub hosted_tools: Vec<String>,
    #[serde(default)]
    pub reasoning: Vec<PickerOptionSpec>,
    #[serde(default)]
    pub default_reasoning: String,
    #[serde(default)]
    pub service_tiers: Vec<PickerOptionSpec>,
    #[serde(default)]
    pub default_service_tier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum PickerOptionSpec {
    Simple(String),
    Detailed { id: String, description: String },
}

impl PickerOptionSpec {
    pub fn id(&self) -> &str {
        match self {
            Self::Simple(id) | Self::Detailed { id, .. } => id,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Simple(_) => "",
            Self::Detailed { description, .. } => description,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    pub name: String,
    pub arguments: String,
}

impl CommandInvocation {
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.strip_prefix('/')?;
        let (name, arguments) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
        (!name.is_empty()).then(|| Self {
            name: name.to_owned(),
            arguments: arguments.trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_slash_command_and_preserves_arguments() {
        assert_eq!(
            CommandInvocation::parse("/model   gpt-5.6-luna"),
            Some(CommandInvocation {
                name: "model".into(),
                arguments: "gpt-5.6-luna".into(),
            })
        );
        assert_eq!(CommandInvocation::parse("hello"), None);
        assert_eq!(CommandInvocation::parse("/"), None);
    }

    #[test]
    fn reads_legacy_string_picker_options() {
        let option: PickerOptionSpec = serde_json::from_str(r#""low""#).unwrap();
        assert_eq!(option.id(), "low");
        assert_eq!(option.description(), "");
    }
}
