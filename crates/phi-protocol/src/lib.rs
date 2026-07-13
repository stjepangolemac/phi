use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    UserMessage {
        content: String,
    },
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
    ToolCompleted {
        name: String,
        result: serde_json::Value,
    },
    HttpCompleted {
        success: bool,
        status: u16,
        events: Vec<serde_json::Value>,
        error: String,
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
    RunTool {
        name: String,
        arguments: serde_json::Value,
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
    Finish {
        content: String,
    },
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
