use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use phi_core::capability::Tool;
use serde_json::{Value, json};
use similar::TextDiff;

pub struct SubmitPolicyCandidate {
    pub active_policy: PathBuf,
    pub provider: PathBuf,
    pub compaction: PathBuf,
}

impl Tool for SubmitPolicyCandidate {
    fn name(&self) -> &'static str {
        "submit_policy_candidate"
    }

    fn execute(&self, workspace: &Path, arguments: Value) -> Result<Value> {
        let content = required_string(&arguments, "content")?;
        let hypothesis = required_string(&arguments, "hypothesis")?;
        let phi = workspace.join(".phi");
        fs::create_dir_all(&phi)?;
        let mut candidate = tempfile::Builder::new().suffix(".scm").tempfile_in(&phi)?;
        std::io::Write::write_all(&mut candidate, content.as_bytes())?;

        phi_steel::check(candidate.path(), &self.provider, &self.compaction)?;
        phi_steel::replay_smoke(candidate.path(), &self.provider, &self.compaction)?;
        gate(workspace, &["fmt", "--all", "--", "--check"])?;
        gate(workspace, &["test", "--workspace"])?;
        gate(
            workspace,
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        )?;

        let original = fs::read_to_string(&self.active_policy)?;
        let diff = TextDiff::from_lines(original.as_str(), content)
            .unified_diff()
            .header("active/agent.scm", "candidate/agent.scm")
            .to_string();
        let id = phi_core::policy_store::submit(&phi.join("policies"), candidate.path())?;
        Ok(json!({
            "candidate_id": id,
            "hypothesis": hypothesis,
            "diff": diff,
            "validation": "Steel load, replay fixture, cargo fmt, cargo test, and cargo clippy passed",
            "activation": "manual approval required"
        }))
    }
}

fn required_string<'a>(arguments: &'a Value, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing {key}"))
}

fn gate(workspace: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("cargo")
        .args(args)
        .current_dir(workspace)
        .output()?;
    if !output.status.success() {
        bail!(
            "cargo {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
