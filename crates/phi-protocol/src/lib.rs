use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    UserMessage {
        content: String,
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
    },
    Finish {
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyOutput {
    pub state: String,
    pub effects: Vec<Effect>,
}
