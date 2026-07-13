use std::{collections::HashSet, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use phi_protocol::Event;
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

pub async fn run(
    workspace: &Path,
    allowed_programs: &HashSet<String>,
    program: &str,
    args: &[String],
    stdin: &str,
    timeout_ms: u64,
) -> Result<Event> {
    if timeout_ms > 60_000 {
        bail!("process timeout exceeds 60000 ms");
    }
    if !allowed_programs.contains(program) {
        bail!("process is not allowed: {program}");
    }
    let mut child = Command::new(program)
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {program}"))?;
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(stdin.as_bytes()).await?;
    }
    let output = tokio::select! {
        result = timeout(Duration::from_millis(timeout_ms), child.wait_with_output()) => {
            result.context("process timed out")??
        }
        result = tokio::signal::ctrl_c() => {
            result?;
            bail!("process cancelled");
        }
    };
    const MAX_OUTPUT: usize = 64 * 1024;
    Ok(Event::ProcessCompleted {
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(MAX_OUTPUT)])
            .into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(MAX_OUTPUT)])
            .into_owned(),
        stdout_truncated: output.stdout.len() > MAX_OUTPUT,
        stderr_truncated: output.stderr.len() > MAX_OUTPUT,
    })
}
