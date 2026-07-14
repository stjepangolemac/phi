use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use phi_protocol::Event;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::mpsc,
    time::{Instant, timeout},
};

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

const DEFAULT_YIELD_MS: u64 = 10_000;
const DEFAULT_POLL_MS: u64 = 5_000;
const DEFAULT_OUTPUT_TOKENS: u64 = 10_000;
const BYTES_PER_TOKEN: usize = 4;
const TERMINATION_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug)]
enum Stream {
    Stdout,
    Stderr,
}

struct ReaderMessage {
    stream: Stream,
    bytes: Vec<u8>,
}

const OUTPUT_CHANNEL_CAPACITY: usize = 256;
const RECENT_OUTPUT_BYTES: usize = 4 * 1024;

#[derive(Default)]
struct RecentOutput(Vec<u8>);

impl RecentOutput {
    fn push(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
        if self.0.len() > RECENT_OUTPUT_BYTES {
            self.0.drain(..self.0.len() - RECENT_OUTPUT_BYTES);
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0).replace("\r\n", "\n")
    }
}

struct CollectedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    max_bytes: usize,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

enum Child {
    Plain(tokio::process::Child),
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
}

enum Input {
    Plain(tokio::process::ChildStdin),
    Pty(Box<dyn Write + Send>),
}

struct Session {
    child: Child,
    input: Option<Input>,
    output: mpsc::Receiver<ReaderMessage>,
    readers: usize,
    readers_done: Arc<AtomicUsize>,
    stdout_dropped: Arc<AtomicBool>,
    stderr_dropped: Arc<AtomicBool>,
    process_group: Option<i32>,
    _master: Option<Box<dyn MasterPty + Send>>,
    terminated: bool,
}

struct SessionEntry {
    session: tokio::sync::Mutex<Session>,
    command: String,
    workdir: PathBuf,
    started: std::time::Instant,
    recent_output: Arc<Mutex<RecentOutput>>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProcessInfo {
    pub session_id: u64,
    pub command: String,
    pub workdir: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    pub recent_output: String,
}

impl Session {
    fn try_wait(&mut self) -> Result<Option<Option<i32>>> {
        match &mut self.child {
            Child::Plain(child) => Ok(child.try_wait()?.map(|status| status.code())),
            Child::Pty(child) => Ok(child
                .try_wait()?
                .map(|status| Some(status.exit_code() as i32))),
        }
    }

    async fn write(&mut self, chars: &str) -> Result<()> {
        let Some(input) = &mut self.input else {
            bail!("process stdin is closed");
        };
        match input {
            Input::Plain(input) => input.write_all(chars.as_bytes()).await?,
            Input::Pty(input) => {
                input.write_all(chars.as_bytes())?;
                input.flush()?;
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn signal(&self, signal: nix::sys::signal::Signal) -> Result<()> {
        let Some(group) = self.process_group else {
            return Ok(());
        };
        match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn kill(&mut self) {
        if self.terminated {
            return;
        }
        #[cfg(unix)]
        if let Some(group) = self.process_group {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(group),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        match &mut self.child {
            Child::Plain(child) => {
                let _ = child.start_kill();
            }
            Child::Pty(child) => {
                let _ = child.kill();
            }
        }
        self.terminated = true;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill();
    }
}

pub struct ShellSessions {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<u64, Arc<SessionEntry>>>,
}

impl Default for ShellSessions {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

impl ShellSessions {
    pub async fn exec<F>(
        &self,
        workspace: &Path,
        arguments: &serde_json::Value,
        on_output: F,
    ) -> Result<serde_json::Value>
    where
        F: FnMut(&str),
    {
        let command = arguments
            .get("cmd")
            .and_then(serde_json::Value::as_str)
            .context("exec_command requires cmd")?;
        let workdir = resolve_workdir(workspace, arguments.get("workdir"))?;
        let shell = arguments
            .get("shell")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(default_shell);
        let login = arguments
            .get("login")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let tty = arguments
            .get("tty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let yield_ms = arguments
            .get("yield_time_ms")
            .and_then(json_u64)
            .unwrap_or(DEFAULT_YIELD_MS);
        let yield_ms = if yield_ms == 0 {
            0
        } else {
            yield_ms.clamp(250, 30_000)
        };
        let max_output_tokens = output_tokens(arguments);

        let recent_output = Arc::new(Mutex::new(RecentOutput::default()));
        let session = if tty {
            spawn_pty(&shell, command, login, &workdir, Arc::clone(&recent_output))?
        } else {
            spawn_plain(&shell, command, login, &workdir, Arc::clone(&recent_output))?
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.sessions
            .lock()
            .expect("shell session registry poisoned")
            .insert(
                id,
                Arc::new(SessionEntry {
                    session: tokio::sync::Mutex::new(session),
                    command: command.into(),
                    workdir,
                    started: std::time::Instant::now(),
                    recent_output,
                }),
            );
        self.collect(id, yield_ms, max_output_tokens, on_output)
            .await
    }

    pub async fn write_stdin<F>(
        &self,
        arguments: &serde_json::Value,
        on_output: F,
    ) -> Result<serde_json::Value>
    where
        F: FnMut(&str),
    {
        let id = arguments
            .get("session_id")
            .and_then(json_u64)
            .context("write_stdin requires session_id")?;
        let chars = arguments
            .get("chars")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let default_yield = if chars.is_empty() {
            DEFAULT_POLL_MS
        } else {
            250
        };
        let max_yield = if chars.is_empty() { 300_000 } else { 30_000 };
        let yield_ms = arguments
            .get("yield_time_ms")
            .and_then(json_u64)
            .unwrap_or(default_yield)
            .clamp(1, max_yield);
        let max_output_tokens = output_tokens(arguments);
        let session = self.session(id)?;
        session.session.lock().await.write(chars).await?;
        self.collect(id, yield_ms, max_output_tokens, on_output)
            .await
    }

    pub async fn terminate(&self, arguments: &serde_json::Value) -> Result<serde_json::Value> {
        let id = arguments
            .get("session_id")
            .and_then(json_u64)
            .context("terminate_process requires session_id")?;
        let entry = self.session(id)?;
        let mut process = entry.session.lock().await;
        let outcome = terminate_gracefully(&mut process).await?;
        drop(process);
        self.sessions
            .lock()
            .expect("shell session registry poisoned")
            .remove(&id);
        Ok(serde_json::json!({
            "session_id": id,
            "status": outcome.status,
            "signal": outcome.signal,
            "exit_code": outcome.exit_code,
        }))
    }

    async fn collect<F>(
        &self,
        id: u64,
        yield_ms: u64,
        max_output_tokens: u64,
        mut on_output: F,
    ) -> Result<serde_json::Value>
    where
        F: FnMut(&str),
    {
        let deadline = Instant::now() + Duration::from_millis(yield_ms);
        let mut output = CollectedOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            max_bytes: (max_output_tokens as usize).saturating_mul(BYTES_PER_TOKEN),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let mut exit_status = None;

        let session = self.session(id)?;
        let mut process = session.session.lock().await;
        loop {
            while let Ok(message) = process.output.try_recv() {
                consume_message(message, &mut output, &mut on_output);
            }
            output.stdout_truncated |= process.stdout_dropped.swap(false, Ordering::Relaxed);
            output.stderr_truncated |= process.stderr_dropped.swap(false, Ordering::Relaxed);
            if exit_status.is_none() {
                exit_status = process.try_wait()?;
            }
            let output_done = process.readers_done.load(Ordering::Relaxed) >= process.readers
                && process.output.is_empty();
            if exit_status.is_some() && output_done {
                process.terminated = true;
                break;
            }
            if Instant::now() >= deadline {
                return Ok(process_result(
                    Some(id),
                    None,
                    output.stdout,
                    output.stderr,
                    output.stdout_truncated,
                    output.stderr_truncated,
                ));
            }
            tokio::select! {
                message = process.output.recv() => {
                    if let Some(message) = message {
                        consume_message(message, &mut output, &mut on_output);
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        drop(process);
        self.sessions
            .lock()
            .expect("shell session registry poisoned")
            .remove(&id);
        Ok(process_result(
            None,
            exit_status.flatten(),
            output.stdout,
            output.stderr,
            output.stdout_truncated,
            output.stderr_truncated,
        ))
    }

    pub fn list(&self) -> Result<Vec<ProcessInfo>> {
        let sessions = self
            .sessions
            .lock()
            .expect("shell session registry poisoned")
            .iter()
            .map(|(id, session)| (*id, Arc::clone(session)))
            .collect::<Vec<_>>();
        let mut processes = Vec::with_capacity(sessions.len());
        for (session_id, entry) in sessions {
            let (status, exit_code) = match entry.session.try_lock() {
                Ok(mut session) => match session.try_wait()? {
                    Some(exit_code) => {
                        if session.readers_done.load(Ordering::Relaxed) >= session.readers {
                            session.terminated = true;
                        }
                        ("exited", exit_code)
                    }
                    None => ("running", None),
                },
                Err(_) => ("running", None),
            };
            processes.push(ProcessInfo {
                session_id,
                command: entry.command.clone(),
                workdir: entry.workdir.display().to_string(),
                status: status.into(),
                exit_code,
                elapsed_ms: entry
                    .started
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
                recent_output: entry
                    .recent_output
                    .lock()
                    .expect("recent process output poisoned")
                    .text(),
            });
        }
        processes.sort_by_key(|process| process.session_id);
        Ok(processes)
    }

    pub fn stop_all(&self) -> usize {
        let sessions = std::mem::take(
            &mut *self
                .sessions
                .lock()
                .expect("shell session registry poisoned"),
        );
        let count = sessions.len();
        terminate_all_blocking(sessions.into_values().collect());
        count
    }

    pub async fn shutdown(&self) -> usize {
        let sessions = std::mem::take(
            &mut *self
                .sessions
                .lock()
                .expect("shell session registry poisoned"),
        );
        let count = sessions.len();
        futures_util::future::join_all(sessions.into_values().map(|session| async move {
            let mut process = session.session.lock().await;
            let _ = terminate_gracefully(&mut process).await;
        }))
        .await;
        count
    }

    fn force_stop_all(&self) {
        let sessions = std::mem::take(
            &mut *self
                .sessions
                .lock()
                .expect("shell session registry poisoned"),
        );
        for session in sessions.into_values() {
            if let Ok(mut process) = session.session.try_lock() {
                process.kill();
            }
        }
    }

    fn session(&self, id: u64) -> Result<Arc<SessionEntry>> {
        self.sessions
            .lock()
            .expect("shell session registry poisoned")
            .get(&id)
            .cloned()
            .with_context(|| format!("unknown process session {id}"))
    }
}

impl Drop for ShellSessions {
    fn drop(&mut self) {
        self.force_stop_all();
    }
}

struct TerminationOutcome {
    status: &'static str,
    signal: Option<&'static str>,
    exit_code: Option<i32>,
}

async fn terminate_gracefully(process: &mut Session) -> Result<TerminationOutcome> {
    if let Some(exit_code) = process.try_wait()? {
        process.terminated = true;
        return Ok(TerminationOutcome {
            status: "exited",
            signal: None,
            exit_code,
        });
    }

    #[cfg(unix)]
    for (signal, name) in [
        (nix::sys::signal::Signal::SIGINT, "SIGINT"),
        (nix::sys::signal::Signal::SIGTERM, "SIGTERM"),
    ] {
        process.signal(signal)?;
        if let Some(exit_code) = wait_for_exit(process).await? {
            return Ok(TerminationOutcome {
                status: "terminated",
                signal: Some(name),
                exit_code,
            });
        }
    }

    process.kill();
    Ok(TerminationOutcome {
        status: "terminated",
        signal: Some("SIGKILL"),
        exit_code: None,
    })
}

async fn wait_for_exit(process: &mut Session) -> Result<Option<Option<i32>>> {
    let deadline = Instant::now() + TERMINATION_GRACE;
    loop {
        if let Some(exit_code) = process.try_wait()? {
            process.terminated = true;
            return Ok(Some(exit_code));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn terminate_all_blocking(sessions: Vec<Arc<SessionEntry>>) {
    #[cfg(unix)]
    for signal in [
        nix::sys::signal::Signal::SIGINT,
        nix::sys::signal::Signal::SIGTERM,
    ] {
        for session in &sessions {
            if let Ok(mut process) = session.session.try_lock()
                && !mark_exited(&mut process)
            {
                let _ = process.signal(signal);
            }
        }
        let deadline = std::time::Instant::now() + TERMINATION_GRACE;
        while std::time::Instant::now() < deadline {
            if sessions.iter().all(|session| {
                session
                    .session
                    .try_lock()
                    .is_ok_and(|mut process| mark_exited(&mut process))
            }) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    for session in sessions {
        if let Ok(mut process) = session.session.try_lock()
            && !mark_exited(&mut process)
        {
            process.kill();
        }
    }
}

fn mark_exited(process: &mut Session) -> bool {
    if matches!(process.try_wait(), Ok(Some(_))) {
        process.terminated = true;
        true
    } else {
        false
    }
}

fn consume_message<F>(message: ReaderMessage, output: &mut CollectedOutput, on_output: &mut F)
where
    F: FnMut(&str),
{
    let used = output.stdout.len().saturating_add(output.stderr.len());
    let (target, truncated) = match message.stream {
        Stream::Stdout => (&mut output.stdout, &mut output.stdout_truncated),
        Stream::Stderr => (&mut output.stderr, &mut output.stderr_truncated),
    };
    let remaining = output.max_bytes.saturating_sub(used);
    let accepted = message.bytes.len().min(remaining);
    if accepted > 0 {
        target.extend_from_slice(&message.bytes[..accepted]);
        on_output(&String::from_utf8_lossy(&message.bytes[..accepted]));
    }
    *truncated |= accepted < message.bytes.len();
}

fn process_result(
    session_id: Option<u64>,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
) -> serde_json::Value {
    let normalize = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
    serde_json::json!({
        "session_id": session_id,
        "exit_code": exit_code,
        "stdout": normalize(stdout),
        "stderr": normalize(stderr),
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
    })
}

fn spawn_plain(
    shell: &str,
    command: &str,
    login: bool,
    workdir: &Path,
    recent_output: Arc<Mutex<RecentOutput>>,
) -> Result<Session> {
    let mut process = Command::new(shell);
    process
        .arg(if login { "-lc" } else { "-c" })
        .arg(command)
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process.spawn().with_context(|| format!("spawn {shell}"))?;
    let process_group = child.id().map(|id| id as i32);
    let input = child.stdin.take().map(Input::Plain);
    let stdout = child.stdout.take().context("process stdout unavailable")?;
    let stderr = child.stderr.take().context("process stderr unavailable")?;
    let readers_done = Arc::new(AtomicUsize::new(0));
    let stdout_dropped = Arc::new(AtomicBool::new(false));
    let stderr_dropped = Arc::new(AtomicBool::new(false));
    let (sender, output) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    tokio::task::spawn_local(read_async(
        stdout,
        Stream::Stdout,
        sender.clone(),
        Arc::clone(&readers_done),
        Arc::clone(&stdout_dropped),
        Arc::clone(&recent_output),
    ));
    tokio::task::spawn_local(read_async(
        stderr,
        Stream::Stderr,
        sender,
        Arc::clone(&readers_done),
        Arc::clone(&stderr_dropped),
        recent_output,
    ));
    Ok(Session {
        child: Child::Plain(child),
        input,
        output,
        readers: 2,
        readers_done,
        stdout_dropped,
        stderr_dropped,
        process_group,
        _master: None,
        terminated: false,
    })
}

fn spawn_pty(
    shell: &str,
    command: &str,
    login: bool,
    workdir: &Path,
    recent_output: Arc<Mutex<RecentOutput>>,
) -> Result<Session> {
    let pair = native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut process = CommandBuilder::new(shell);
    process.arg(if login { "-lc" } else { "-c" });
    process.arg(command);
    process.cwd(workdir);
    let child = pair.slave.spawn_command(process)?;
    drop(pair.slave);
    let process_group = pair.master.process_group_leader();
    let input = Some(Input::Pty(pair.master.take_writer()?));
    let mut reader = pair.master.try_clone_reader()?;
    let readers_done = Arc::new(AtomicUsize::new(0));
    let stdout_dropped = Arc::new(AtomicBool::new(false));
    let stderr_dropped = Arc::new(AtomicBool::new(false));
    let (sender, output) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    let done = Arc::clone(&readers_done);
    let dropped = Arc::clone(&stdout_dropped);
    std::thread::spawn(move || {
        let mut buffer = vec![0; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let bytes = buffer[..read].to_vec();
                    recent_output
                        .lock()
                        .expect("recent process output poisoned")
                        .push(&bytes);
                    if sender
                        .try_send(ReaderMessage {
                            stream: Stream::Stdout,
                            bytes,
                        })
                        .is_err()
                    {
                        dropped.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        done.fetch_add(1, Ordering::Relaxed);
    });
    Ok(Session {
        child: Child::Pty(child),
        input,
        output,
        readers: 1,
        readers_done,
        stdout_dropped,
        stderr_dropped,
        process_group,
        _master: Some(pair.master),
        terminated: false,
    })
}

async fn read_async<R>(
    mut reader: R,
    stream: Stream,
    sender: mpsc::Sender<ReaderMessage>,
    readers_done: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
    recent_output: Arc<Mutex<RecentOutput>>,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0; 4096];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let bytes = buffer[..read].to_vec();
                recent_output
                    .lock()
                    .expect("recent process output poisoned")
                    .push(&bytes);
                if sender.try_send(ReaderMessage { stream, bytes }).is_err() {
                    dropped.store(true, Ordering::Relaxed);
                }
            }
        }
    }
    readers_done.fetch_add(1, Ordering::Relaxed);
}

fn resolve_workdir(workspace: &Path, value: Option<&serde_json::Value>) -> Result<PathBuf> {
    let root = std::fs::canonicalize(workspace)?;
    let requested = value
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let path = if requested.is_absolute() {
        requested
    } else {
        root.join(requested)
    };
    let path = std::fs::canonicalize(path).context("command workdir does not exist")?;
    if !path.starts_with(&root) {
        bail!("command workdir is outside workspace");
    }
    Ok(path)
}

fn output_tokens(arguments: &serde_json::Value) -> u64 {
    arguments
        .get("max_output_tokens")
        .and_then(json_u64)
        .unwrap_or(DEFAULT_OUTPUT_TOKENS)
        .clamp(1, 100_000)
}

fn json_u64(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        let number = value.as_f64()?;
        (number.is_finite() && number >= 0.0 && number <= u64::MAX as f64 && number.fract() == 0.0)
            .then_some(number as u64)
    })
}

fn default_shell() -> String {
    if cfg!(windows) {
        "powershell.exe".into()
    } else {
        "/bin/sh".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::future::Future;

    async fn local<T>(future: impl Future<Output = T>) -> T {
        tokio::task::LocalSet::new().run_until(future).await
    }

    #[cfg(unix)]
    async fn wait_for_file(path: &Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {}", path.display());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runs_compound_commands_in_a_relative_workdir() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            std::fs::create_dir(workspace.path().join("nested")).unwrap();
            let sessions = ShellSessions::default();
            let result = sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "printf first > a.txt && printf second > b.txt",
                        "workdir": "nested"
                    }),
                    |_| {},
                )
                .await
                .unwrap();

            assert_eq!(result["exit_code"], 0);
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("nested/a.txt")).unwrap(),
                "first"
            );
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("nested/b.txt")).unwrap(),
                "second"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn yields_and_can_be_polled() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            let first = sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "printf start; sleep 0.4; printf end",
                        "yield_time_ms": 250.0
                    }),
                    |_| {},
                )
                .await
                .unwrap();
            let id = first["session_id"].as_u64().unwrap();
            assert_eq!(first["stdout"], "start");

            let second = sessions
                .write_stdin(
                    &json!({ "session_id": id as f64, "yield_time_ms": 1_000.0 }),
                    |_| {},
                )
                .await
                .unwrap();
            assert_eq!(second["session_id"], serde_json::Value::Null);
            assert_eq!(second["exit_code"], 0);
            assert_eq!(second["stdout"], "end");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn zero_yield_starts_a_managed_background_session() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            let result = sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "sleep 10",
                        "yield_time_ms": 0
                    }),
                    |_| {},
                )
                .await
                .unwrap();

            let id = result["session_id"].as_u64().unwrap();
            assert_eq!(sessions.list().unwrap()[0].session_id, id);
            sessions.stop_all();
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn terminate_process_starts_with_sigint() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            let result = sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "trap 'printf interrupted > marker; exit 0' INT; : > ready; read _",
                        "yield_time_ms": 0
                    }),
                    |_| {},
                )
                .await
                .unwrap();
            wait_for_file(&workspace.path().join("ready")).await;
            let id = result["session_id"].as_u64().unwrap();
            let result = sessions
                .terminate(&json!({ "session_id": id }))
                .await
                .unwrap();

            assert_eq!(result["status"], "terminated");
            assert_eq!(result["signal"], "SIGINT");
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("marker")).unwrap(),
                "interrupted"
            );
            assert!(sessions.list().unwrap().is_empty());
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn terminate_process_escalates_to_sigkill() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            let result = sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "trap '' INT TERM; : > ready; read _",
                        "yield_time_ms": 0
                    }),
                    |_| {},
                )
                .await
                .unwrap();
            wait_for_file(&workspace.path().join("ready")).await;
            let result = sessions
                .terminate(&json!({ "session_id": result["session_id"] }))
                .await
                .unwrap();

            assert_eq!(result["signal"], "SIGKILL");
            assert!(sessions.list().unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lists_a_yielded_process_until_its_output_is_collected() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            let first = sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "printf start; sleep 0.4; printf end",
                        "yield_time_ms": 250
                    }),
                    |_| {},
                )
                .await
                .unwrap();
            let id = first["session_id"].as_u64().unwrap();
            let running = sessions.list().unwrap();
            assert_eq!(running.len(), 1);
            assert_eq!(running[0].session_id, id);
            assert_eq!(running[0].status, "running");
            assert!(running[0].recent_output.contains("start"));

            tokio::time::sleep(Duration::from_millis(250)).await;
            let exited = sessions.list().unwrap();
            assert_eq!(exited[0].status, "exited");
            assert_eq!(exited[0].exit_code, Some(0));
            assert!(exited[0].recent_output.contains("startend"));

            let completed = sessions
                .write_stdin(&json!({ "session_id": id, "yield_time_ms": 1_000 }), |_| {})
                .await
                .unwrap();
            assert_eq!(completed["stdout"], "end");
            assert!(sessions.list().unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stops_all_background_processes() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            let result = sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "sleep 0.5; printf done > marker",
                        "yield_time_ms": 250
                    }),
                    |_| {},
                )
                .await
                .unwrap();
            assert!(result["session_id"].is_number());
            assert_eq!(sessions.stop_all(), 1);
            assert!(sessions.list().unwrap().is_empty());
            tokio::time::sleep(Duration::from_millis(350)).await;
            assert!(!workspace.path().join("marker").exists());
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_starts_with_sigint() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "trap 'printf shutdown > marker; exit 0' INT; : > ready; read _",
                        "yield_time_ms": 0
                    }),
                    |_| {},
                )
                .await
                .unwrap();
            wait_for_file(&workspace.path().join("ready")).await;

            assert_eq!(sessions.shutdown().await, 1);
            wait_for_file(&workspace.path().join("marker")).await;
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("marker")).unwrap(),
                "shutdown"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_the_registry_stops_background_processes() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "sleep 0.5; printf done > marker",
                        "yield_time_ms": 250
                    }),
                    |_| {},
                )
                .await
                .unwrap();
            drop(sessions);
            tokio::time::sleep(Duration::from_millis(350)).await;
            assert!(!workspace.path().join("marker").exists());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bounds_recent_background_output() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "sleep 0.3; i=0; while [ $i -lt 2000 ]; do printf 1234567890; i=$((i+1)); done; sleep 1",
                        "yield_time_ms": 250
                    }),
                    |_| {},
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            let processes = sessions.list().unwrap();
            assert_eq!(processes.len(), 1);
            assert!(processes[0].recent_output.len() <= RECENT_OUTPUT_BYTES);
            sessions.stop_all();
        })
        .await;
    }

    #[test]
    fn accepts_integral_numbers_from_steel_json() {
        assert_eq!(json_u64(&json!(1.0)), Some(1));
        assert_eq!(json_u64(&json!(1.5)), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runs_independent_commands_concurrently() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            let first = json!({
                "cmd": ": > first.ready; while [ ! -e second.ready ]; do sleep 0.01; done"
            });
            let second = json!({
                "cmd": ": > second.ready; while [ ! -e first.ready ]; do sleep 0.01; done"
            });
            let (first, second) = tokio::join!(
                sessions.exec(workspace.path(), &first, |_| {}),
                sessions.exec(workspace.path(), &second, |_| {}),
            );

            assert_eq!(first.unwrap()["exit_code"], 0);
            assert_eq!(second.unwrap()["exit_code"], 0);
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn writes_to_a_pty_session() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            let first = sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "printf 'name? '; read value; printf 'hi %s\\n' \"$value\"",
                        "tty": true,
                        "yield_time_ms": 250
                    }),
                    |_| {},
                )
                .await
                .unwrap();
            let id = first["session_id"].as_u64().unwrap();
            assert!(first["stdout"].as_str().unwrap().contains("name?"));

            let second = sessions
                .write_stdin(
                    &json!({
                        "session_id": id,
                        "chars": "Ada\n",
                        "yield_time_ms": 1_000
                    }),
                    |_| {},
                )
                .await
                .unwrap();
            assert_eq!(second["exit_code"], 0);
            assert!(second["stdout"].as_str().unwrap().contains("hi Ada"));
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn truncates_returned_and_streamed_output() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            let mut streamed = String::new();
            let result = sessions
                .exec(
                    workspace.path(),
                    &json!({
                        "cmd": "i=0; while [ $i -lt 100 ]; do printf 1234567890; i=$((i+1)); done",
                        "max_output_tokens": 10.0
                    }),
                    |chunk| streamed.push_str(chunk),
                )
                .await
                .unwrap();

            assert_eq!(result["stdout"].as_str().unwrap().len(), 40);
            assert_eq!(streamed.len(), 40);
            assert_eq!(result["stdout_truncated"], true);
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_a_workdir_outside_the_workspace() {
        local(async {
            let workspace = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let sessions = ShellSessions::default();
            let error = sessions
                .exec(
                    workspace.path(),
                    &json!({ "cmd": "pwd", "workdir": outside.path() }),
                    |_| {},
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("outside workspace"));
        })
        .await;
    }
}
