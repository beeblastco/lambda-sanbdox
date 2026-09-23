//! Agent sandbox — core exec engine for the AWS Lambda MicroVM sandbox image.
//!
//! Spawns bash/python/node subprocesses in isolated workspaces, captures output,
//! enforces timeouts, and returns structured JSON responses. `run_exec` is driven
//! by the long-lived MicroVM HTTP server in `main.rs` (`POST /exec`). The
//! persistent-workspace S3 mount is set up by the `/run` lifecycle hook via the
//! [`mount`] module.
//!
//! Workspace modes:
//!   - Persistent: `namespace` provided → uses `{workspace_root}/{namespace}`, never cleaned up.
//!     Files persist across calls via the S3 mount the `/run` hook established.
//!   - Ephemeral: no `namespace` → fresh /tmp/agent-workspace/<uuid>, cleaned up after.

pub mod mount;

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Instant;
use tokio::fs;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

const MAX_STDOUT_SIZE: usize = 256 * 1024; // 256 KB
const MAX_STDERR_SIZE: usize = 256 * 1024; // 256 KB
const MAX_CODE_BYTES: usize = 10 * 1024 * 1024; // 10 MB
const MAX_TOTAL_ENV_BYTES: usize = 256 * 1024; // 256 KB
const MAX_ARGS_COUNT: usize = 64;
const MAX_ARGS_TOTAL_BYTES: usize = 64 * 1024; // 64 KB
const MAX_TIMEOUT_MS: u64 = 600_000; // 10 minutes, the broods lambda provider's ceiling

// How long to keep reading output after the child exits. Anything it backgrounded
// inherits the pipes and can hold them open for as long as it runs.
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);

/// Largest `/exec` body the server buffers: the 10 MB code cap plus headroom for
/// the other fields and ordinary JSON escaping.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024; // 16 MB
const DEFAULT_WORKSPACE_ROOT: &str = "/mnt/workspaces";

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    #[serde(default = "default_runtime")]
    pub runtime: String,

    pub code: String,

    /// Workspace namespace (`fs-[a-f0-9]{40}`). When set, uses a persistent workspace
    /// at `{workspace_root}/{namespace}` backed by the MicroVM S3 mount. When omitted,
    /// an ephemeral /tmp workspace is used and cleaned up after the call.
    #[serde(default)]
    pub namespace: Option<String>,

    /// Override the workspace root. Defaults to the SANDBOX_WORKSPACE_MOUNT_PATH
    /// environment variable, or /mnt/workspaces.
    #[serde(default)]
    pub workspace_root: Option<String>,

    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    #[serde(default)]
    pub args: Vec<String>,

    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct ExecResponse {
    pub ok: bool,
    pub runtime: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u128,
    pub stdout: String,
    pub stderr: String,
    pub workspace: String,

    /// True when the returned stdout or stderr was cut to 256 KB. The cut text ends
    /// in `...[truncated]`, so callers decoding it must check this first.
    pub truncated: bool,

    /// CPU time (user + system, including descendants) charged to the sandboxed
    /// process, in microseconds. Measured as a delta around the run off the
    /// cgroup v2 `cpu.stat` `usage_usec` counter (microsecond resolution), with a
    /// `getrusage(RUSAGE_CHILDREN)` fallback. Omitted when no child was reaped
    /// (validation errors, timeouts) so the caller simply skips the CPU sample.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_usec: Option<u64>,
}

fn default_runtime() -> String {
    "bash".to_string()
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn validate_namespace(ns: &str) -> bool {
    if !ns.starts_with("fs-") {
        return false;
    }
    let hex = &ns[3..];
    hex.len() == 40 && hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// The interpreter for `runtime` and a unique script name to run it on. Bash
/// scripts go through the interpreter too: mountpoint-s3 rejects chmod, so a script
/// on the workspace mount can't be made executable.
fn runtime_program(runtime: &str) -> anyhow::Result<(&'static str, String)> {
    let (interpreter, extension) = match runtime {
        "bash" | "sh" => ("/bin/bash", "sh"),
        "python" | "python3" | "py" => ("/usr/bin/python3", "py"),
        "node" | "nodejs" | "js" | "javascript" => ("/usr/bin/node", "js"),
        other => return Err(anyhow!("unsupported runtime: {other}")),
    };

    Ok((
        interpreter,
        format!(".broods-exec-{}.{}", Uuid::new_v4(), extension),
    ))
}

fn resolve_workspace(req: &ExecRequest) -> Result<(PathBuf, bool), anyhow::Error> {
    match &req.namespace {
        Some(ns) => {
            if !validate_namespace(ns) {
                return Err(anyhow!("invalid namespace: must match fs-[a-f0-9]{{40}}"));
            }
            let root = req
                .workspace_root
                .clone()
                .or_else(|| std::env::var("SANDBOX_WORKSPACE_MOUNT_PATH").ok())
                .unwrap_or_else(|| DEFAULT_WORKSPACE_ROOT.to_string());
            Ok((PathBuf::from(root).join(ns), false))
        }
        None => Ok((
            PathBuf::from("/tmp/agent-workspace").join(Uuid::new_v4().to_string()),
            true,
        )),
    }
}

/// Build an `ok: false` response for a failure that aborted before (or instead of)
/// running a child. `workspace` is the resolved path when known, else empty.
fn error_response(
    runtime: &str,
    started: Instant,
    stderr: String,
    workspace: String,
) -> ExecResponse {
    ExecResponse {
        ok: false,
        runtime: runtime.to_string(),
        exit_code: None,
        timed_out: false,
        duration_ms: started.elapsed().as_millis(),
        stdout: String::new(),
        stderr,
        workspace,
        truncated: false,
        cpu_usec: None,
    }
}

/// Run one exec request end to end: resolve the workspace, create it, execute the
/// code, and clean up an ephemeral workspace afterwards. Internal failures are
/// mapped into an `ok: false` ExecResponse (this never returns an Err) so the HTTP
/// layer always has a structured body to send back from the MicroVM HTTP endpoint.
pub async fn run_exec(req: ExecRequest) -> ExecResponse {
    let started = Instant::now();

    let (workspace, ephemeral) = match resolve_workspace(&req) {
        Ok(w) => w,
        Err(e) => return error_response(&req.runtime, started, e.to_string(), String::new()),
    };

    if let Err(e) = fs::create_dir_all(&workspace).await {
        return error_response(
            &req.runtime,
            started,
            format!("failed to create workspace: {e}"),
            String::new(),
        );
    }

    let result = match execute_request(&req, &workspace, started).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            &req.runtime,
            started,
            e.to_string(),
            workspace.display().to_string(),
        ),
    };

    if ephemeral {
        if let Err(e) = fs::remove_dir_all(&workspace).await {
            eprintln!(
                "warning: failed to remove workspace {}: {e}",
                workspace.display()
            );
        }
    }

    result
}

async fn execute_request(
    req: &ExecRequest,
    workspace: &PathBuf,
    started: Instant,
) -> anyhow::Result<ExecResponse> {
    if req.code.len() > MAX_CODE_BYTES {
        return Err(anyhow!(
            "code exceeds maximum size of {} bytes",
            MAX_CODE_BYTES
        ));
    }

    let total_env_size: usize = req.env.iter().map(|(k, v)| k.len() + v.len()).sum();
    if total_env_size > MAX_TOTAL_ENV_BYTES {
        return Err(anyhow!(
            "total env size exceeds maximum of {} bytes",
            MAX_TOTAL_ENV_BYTES
        ));
    }

    if req.args.len() > MAX_ARGS_COUNT {
        return Err(anyhow!("args exceeds maximum count of {}", MAX_ARGS_COUNT));
    }
    let total_args_size: usize = req.args.iter().map(|a| a.len()).sum();
    if total_args_size > MAX_ARGS_TOTAL_BYTES {
        return Err(anyhow!(
            "total args size exceeds maximum of {} bytes",
            MAX_ARGS_TOTAL_BYTES
        ));
    }

    if req.timeout_ms > MAX_TIMEOUT_MS {
        return Err(anyhow!(
            "timeout_ms exceeds maximum of {} ms",
            MAX_TIMEOUT_MS
        ));
    }

    let runtime = req.runtime.to_lowercase();

    let (interpreter, script_name) = runtime_program(&runtime)?;

    // A unique script in the workspace root preserves python/node relative imports
    // without letting parallel MicroVMs overwrite or remove each other's program.
    let script_path = workspace.join(&script_name);
    fs::write(&script_path, &req.code)
        .await
        .context("failed to write code file")?;

    let path_str = script_path.display().to_string();
    let mut bash_command = interpreter.to_string();
    for word in std::iter::once(&path_str).chain(&req.args) {
        let quoted = shlex::try_quote(word).map_err(|e| anyhow!("invalid argument: {e}"))?;
        bash_command.push(' ');
        bash_command.push_str(&quoted);
    }

    let mut command = Command::new("bash");
    command
        .arg("-lc")
        .arg(&bash_command)
        .current_dir(workspace)
        .env_clear()
        .env("HOME", workspace)
        .env("TMPDIR", workspace)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin:/opt/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so a timeout can kill everything the script started.
        .process_group(0)
        .kill_on_drop(true);

    for (key, value) in &req.env {
        command.env(key, value);
    }

    let run = run_child(command, req.timeout_ms).await;

    // Remove this run's unique script so it never lingers in a persistent workspace.
    // Best-effort: the sync below flushes the unlink too.
    let _ = fs::remove_file(&script_path).await;

    // Flush the bash tool's workspace writes (and the script removal above) to the
    // S3 mount before the MicroVM suspends or terminates. Without this, files
    // written via shell redirection can live only in the page cache and be lost on
    // a replacement MicroVM. Ephemeral workspaces are deleted right after the run,
    // so skip them.
    //
    // STOPGAP: this is a coarse per-run flush that only prevents silent data loss
    // on replacement MicroVMs — it does not address cross-provider durability, hop-2
    // S3 visibility lag, or multi-agent write conflicts. The intended final fix is
    // a unified shared-data layer (Archil-style elastic POSIX FS, mountable across
    // sandboxes) that owns durability + conflict resolution in one place. Tracked
    // in filthy-panty #64; remove this flush once that layer lands.
    if req.namespace.is_some() {
        // SAFETY: sync() takes no arguments and has no failure mode; it flushes
        // all filesystem buffers.
        unsafe {
            libc::sync();
        }
    }

    let run = run?;
    let (stdout, stdout_cut) = capped_text(&run.stdout, MAX_STDOUT_SIZE);
    let (mut stderr, stderr_cut) = capped_text(&run.stderr, MAX_STDERR_SIZE);
    let Some(status) = run.status else {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(&format!("execution timed out after {} ms", req.timeout_ms));
        // The killed child is reaped asynchronously, so RUSAGE_CHILDREN may not yet
        // reflect it. Omit the sample rather than report a misleading partial number.
        return Ok(ExecResponse {
            ok: false,
            runtime: req.runtime.clone(),
            exit_code: None,
            timed_out: true,
            duration_ms: started.elapsed().as_millis(),
            stdout,
            stderr,
            workspace: workspace.display().to_string(),
            truncated: stdout_cut || stderr_cut,
            cpu_usec: None,
        });
    };

    Ok(ExecResponse {
        ok: status.success(),
        runtime: req.runtime.clone(),
        exit_code: status.code(),
        timed_out: false,
        duration_ms: started.elapsed().as_millis(),
        stdout,
        stderr,
        workspace: workspace.display().to_string(),
        truncated: stdout_cut || stderr_cut,
        cpu_usec: Some(run.cpu_usec),
    })
}

/// What one child run produced. `status` is `None` when the run timed out.
struct ChildRun {
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    cpu_usec: u64,
}

/// Spawn `command`, wait up to `timeout_ms` for it, and collect its capped output.
/// A timeout kills the child's whole process group.
async fn run_child(mut command: Command, timeout_ms: u64) -> anyhow::Result<ChildRun> {
    let cpu_before = children_cpu_usec();
    let mut child = command.spawn().context("failed to run child process")?;
    let stdout = CappedReader::spawn(
        child.stdout.take().context("stdout not piped")?,
        MAX_STDOUT_SIZE,
    );
    let stderr = CappedReader::spawn(
        child.stderr.take().context("stderr not piped")?,
        MAX_STDERR_SIZE,
    );
    let waited = timeout(Duration::from_millis(timeout_ms), child.wait()).await;
    // Read right after the child is reaped, before the caller removes the script.
    let cpu_usec = children_cpu_usec().saturating_sub(cpu_before);
    let status = match waited {
        Ok(status) => Some(status.context("failed to wait for child process")?),
        // No wait after the kill: a child stuck in I/O on a hung FUSE mount ignores
        // SIGKILL. kill_on_drop hands it to tokio's background reaper instead.
        Err(_) => {
            kill_process_group(&child);
            None
        }
    };
    let (stdout, stderr) = tokio::join!(stdout.finish(), stderr.finish());

    Ok(ChildRun {
        status,
        stdout,
        stderr,
        cpu_usec,
    })
}

/// A child's stdout or stderr, read to EOF on its own task so the child never
/// blocks on a full pipe. Keeps one byte past `cap`, so `capped_text` can tell
/// a cut stream from one that fits exactly, and discards the rest.
struct CappedReader {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Vec<u8>>,
}

impl CappedReader {
    fn spawn(pipe: impl AsyncRead + Unpin + Send + 'static, cap: usize) -> Self {
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(read_capped(pipe, cap + 1, stopped));

        Self { stop, task }
    }

    /// The bytes read once the pipe closes, or after `OUTPUT_DRAIN_GRACE` if a
    /// process the child backgrounded still holds it open.
    async fn finish(mut self) -> Vec<u8> {
        if let Ok(read) = timeout(OUTPUT_DRAIN_GRACE, &mut self.task).await {
            return read.unwrap_or_default();
        }
        let _ = self.stop.send(());

        self.task.await.unwrap_or_default()
    }
}

/// Decode captured output and cut it at `cap` bytes. Returns the text and whether
/// it was cut. Invalid UTF-8 decodes to 3-byte U+FFFD, so binary output can be cut
/// below `cap` raw bytes.
fn capped_text(bytes: &[u8], cap: usize) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);

    (truncate_string(&text, cap), text.len() > cap)
}

/// SIGKILL the child's process group. A process that called `setsid` has left the
/// group and survives, as broods's detached jobs intend.
fn kill_process_group(child: &Child) {
    if let Some(pid) = child.id() {
        // SAFETY: killpg only sends a signal. The child was spawned with
        // process_group(0), so its pid is the group id.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// Body of a `CappedReader` task: reads `pipe` until EOF or `stop`, keeping the
/// first `keep` bytes.
async fn read_capped(
    mut pipe: impl AsyncRead + Unpin,
    keep: usize,
    mut stop: oneshot::Receiver<()>,
) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = tokio::select! {
            read = pipe.read(&mut chunk) => read,
            _ = &mut stop => break,
        };
        match read {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = keep.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }

    kept
}

#[cfg(test)]
mod runtime_script_tests {
    use super::runtime_program;

    #[test]
    fn creates_unique_hidden_script_names() {
        let (bash, first) = runtime_program("bash").expect("bash program");
        let (_, second) = runtime_program("bash").expect("bash program");

        assert_eq!(bash, "/bin/bash");
        assert_ne!(first, second);
        assert!(first.starts_with(".broods-exec-"));
        assert!(first.ends_with(".sh"));
        let (python, script) = runtime_program("python3").expect("python program");
        assert_eq!(python, "/usr/bin/python3");
        assert!(script.ends_with(".py"));
        let (node, script) = runtime_program("node").expect("node program");
        assert_eq!(node, "/usr/bin/node");
        assert!(script.ends_with(".js"));
    }

    #[test]
    fn rejects_unsupported_runtimes() {
        let error = runtime_program("ruby").expect_err("unsupported runtime");

        assert_eq!(error.to_string(), "unsupported runtime: ruby");
    }
}

#[cfg(test)]
mod exec_tests {
    use super::{run_exec, ExecRequest, MAX_STDOUT_SIZE};
    use std::collections::HashMap;
    use std::time::Duration;

    fn bash(code: &str, timeout_ms: u64) -> ExecRequest {
        ExecRequest {
            runtime: "bash".to_string(),
            code: code.to_string(),
            namespace: None,
            workspace_root: None,
            timeout_ms,
            args: Vec::new(),
            env: HashMap::new(),
        }
    }

    // A backgrounded process inherits the pipes. The run used to wait on them until
    // its timeout and lose the output of a script that had long exited.
    #[tokio::test]
    async fn a_backgrounded_process_does_not_hold_the_run_open() {
        let response = run_exec(bash("sleep 3 & echo done", 10_000)).await;

        assert!(response.ok, "{}", response.stderr);
        assert!(!response.timed_out);
        assert_eq!(response.stdout, "done\n");
        assert!(
            response.duration_ms < 2_000,
            "took {}ms",
            response.duration_ms
        );
    }

    #[tokio::test]
    async fn a_timeout_kills_the_process_group_and_keeps_partial_output() {
        let response = run_exec(bash("sleep 30 & echo $!; sleep 30", 500)).await;
        let orphan: libc::pid_t = response.stdout.trim().parse().expect("orphan pid");

        assert!(response.timed_out);
        assert!(response
            .stderr
            .ends_with("execution timed out after 500 ms"));
        // The orphan's new parent reaps it asynchronously, so give it a moment.
        let mut alive = true;
        for _ in 0..20 {
            // SAFETY: signal 0 only checks that the pid exists.
            alive = unsafe { libc::kill(orphan, 0) } == 0;
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!alive, "backgrounded sleep {orphan} survived the timeout");
    }

    #[tokio::test]
    async fn flags_output_cut_at_the_cap() {
        let cut = run_exec(bash("head -c 300000 /dev/zero | tr '\\0' a", 10_000)).await;
        let fits = run_exec(bash("head -c 1000 /dev/zero | tr '\\0' a", 10_000)).await;

        assert!(cut.truncated);
        assert!(cut.stdout.starts_with(&"a".repeat(MAX_STDOUT_SIZE)));
        assert!(cut.stdout.ends_with("...[truncated]"));
        assert!(!fits.truncated);
        assert_eq!(fits.stdout.len(), 1000);
    }
}

/// CPU time charged to this execution environment so far, in microseconds. Read
/// once before and once after a child runs; the delta is that child's CPU time
/// (the server serializes execs and blocks on each child, so the runtime's own CPU
/// between the two reads is negligible).
///
/// Prefers the cgroup v2 `cpu.stat` `usage_usec` counter, which the kernel tracks
/// at microsecond resolution from the scheduler's runtime accounting, so even a
/// sub-10ms command — the common case for an agent's shell calls — is counted.
/// `getrusage(RUSAGE_CHILDREN)` is the fallback: its child-time accounting is
/// clock-tick granular and silently rounds short commands down to zero, so it is
/// used only where the cgroup counter is unavailable.
fn children_cpu_usec() -> u64 {
    if let Some(usec) = cgroup_cpu_usec() {
        return usec;
    }
    rusage_children_cpu_usec()
}

/// cgroup v2 cumulative CPU (`usage_usec`) for this environment, if exposed.
fn cgroup_cpu_usec() -> Option<u64> {
    let contents = std::fs::read_to_string("/sys/fs/cgroup/cpu.stat").ok()?;
    parse_cpu_stat_usage_usec(&contents)
}

/// Extract the `usage_usec` value (microseconds) from a cgroup v2 `cpu.stat` body.
fn parse_cpu_stat_usage_usec(contents: &str) -> Option<u64> {
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("usage_usec ") {
            return rest.trim().parse::<u64>().ok();
        }
    }
    None
}

/// Total CPU (user + system) charged to reaped child processes so far, in
/// microseconds. RUSAGE_CHILDREN accumulates process-wide and rolls up the whole
/// descendant tree (bash waits on its subprocess before exiting).
fn rusage_children_cpu_usec() -> u64 {
    // SAFETY: getrusage only writes into the provided rusage; reading a zeroed
    // struct back is sound and RUSAGE_CHILDREN has no failure mode for a valid
    // pointer. On the unexpected error path we report 0 rather than panic.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) } != 0 {
        return 0;
    }
    timeval_usec(usage.ru_utime) + timeval_usec(usage.ru_stime)
}

fn timeval_usec(tv: libc::timeval) -> u64 {
    (tv.tv_sec.max(0) as u64) * 1_000_000 + (tv.tv_usec.max(0) as u64)
}

pub fn truncate_string(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut truncated = s[..boundary].to_string();
    truncated.push_str("\n...[truncated]");
    truncated
}

#[cfg(test)]
mod cpu_tests {
    use super::{children_cpu_usec, parse_cpu_stat_usage_usec};

    /// A reaped CPU-burning child must register a non-trivial delta — this is the
    /// measurement that backs ExecResponse.cpu_usec.
    #[test]
    fn children_cpu_usec_counts_a_busy_child() {
        let before = children_cpu_usec();
        // A burst of pure CPU; output() waits (and thus reaps) the child.
        let result = std::process::Command::new("bash")
            .arg("-c")
            .arg("n=0; while [ $n -lt 5000000 ]; do n=$((n+1)); done")
            .output()
            .expect("spawn busy child");
        assert!(result.status.success());
        let delta = children_cpu_usec() - before;
        assert!(delta > 0, "expected child CPU to be counted, got {delta}us");
    }

    /// The microsecond `usage_usec` line is what we read for accurate sub-tick CPU.
    #[test]
    fn parses_usage_usec_from_cpu_stat() {
        let sample = "usage_usec 1234567\nuser_usec 1000000\nsystem_usec 234567\n";
        assert_eq!(parse_cpu_stat_usage_usec(sample), Some(1_234_567));
    }

    /// A body without the counter (e.g. a non-cgroup-v2 host) yields None so the
    /// caller falls back to getrusage instead of reporting a bogus zero.
    #[test]
    fn returns_none_when_usage_usec_absent() {
        assert_eq!(
            parse_cpu_stat_usage_usec("nr_periods 0\nnr_throttled 0\n"),
            None
        );
        assert_eq!(parse_cpu_stat_usage_usec(""), None);
    }
}
