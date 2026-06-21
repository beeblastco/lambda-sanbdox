//! Lambda Agent Sandbox — core library for the AWS Lambda custom runtime handler.
//!
//! Spawns bash/python/node subprocesses in isolated workspaces, captures output,
//! enforces timeouts, and returns structured JSON responses.
//!
//! Workspace modes:
//!   - Persistent: `namespace` provided → uses `{workspace_root}/{namespace}`, never cleaned up.
//!     Files persist across calls via the S3 Files mount.
//!   - Ephemeral: no `namespace` → fresh /tmp/agent-workspace/<uuid>, cleaned up after.

use anyhow::{anyhow, Context};
use lambda_runtime::{Error, LambdaEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
use tokio::fs;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

const MAX_STDOUT_SIZE: usize = 256 * 1024; // 256 KB
const MAX_STDERR_SIZE: usize = 256 * 1024; // 256 KB
const MAX_CODE_BYTES: usize = 10 * 1024 * 1024; // 10 MB
const MAX_TOTAL_ENV_BYTES: usize = 256 * 1024; // 256 KB
const MAX_ARGS_COUNT: usize = 64;
const MAX_ARGS_TOTAL_BYTES: usize = 64 * 1024; // 64 KB
const MAX_TIMEOUT_MS: u64 = 300_000; // 5 minutes cap
const DEFAULT_WORKSPACE_ROOT: &str = "/mnt/workspaces";

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    #[serde(default = "default_runtime")]
    pub runtime: String,

    pub code: String,

    /// Workspace namespace (`fs-[a-f0-9]{40}`). When set, uses a persistent workspace
    /// at `{workspace_root}/{namespace}` backed by the S3 Files mount. When omitted,
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

    /// CPU time (user + system, including descendants) charged to the sandboxed
    /// process, in microseconds. Measured via a `getrusage(RUSAGE_CHILDREN)`
    /// delta around the run. Omitted when no child was reaped (validation
    /// errors, timeouts) so the caller simply skips the CPU sample.
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

pub async fn handler(event: LambdaEvent<Value>) -> Result<Value, Error> {
    let started = Instant::now();

    let req: ExecRequest = match serde_json::from_value(event.payload) {
        Ok(r) => r,
        Err(e) => {
            return Ok(json!(ExecResponse {
                ok: false,
                runtime: "unknown".to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                stdout: "".to_string(),
                stderr: format!("invalid request json: {e}"),
                workspace: "".to_string(),
                cpu_usec: None,
            }));
        }
    };

    let (workspace, ephemeral) = match resolve_workspace(&req) {
        Ok(w) => w,
        Err(e) => {
            return Ok(json!(ExecResponse {
                ok: false,
                runtime: req.runtime,
                exit_code: None,
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                stdout: "".to_string(),
                stderr: e.to_string(),
                workspace: "".to_string(),
                cpu_usec: None,
            }));
        }
    };

    if let Err(e) = fs::create_dir_all(&workspace).await {
        return Ok(json!(ExecResponse {
            ok: false,
            runtime: req.runtime,
            exit_code: None,
            timed_out: false,
            duration_ms: started.elapsed().as_millis(),
            stdout: "".to_string(),
            stderr: format!("failed to create workspace: {e}"),
            workspace: "".to_string(),
            cpu_usec: None,
        }));
    }

    let result = match execute_request(&req, &workspace, started).await {
        Ok(resp) => json!(resp),
        Err(e) => json!(ExecResponse {
            ok: false,
            runtime: req.runtime.clone(),
            exit_code: None,
            timed_out: false,
            duration_ms: started.elapsed().as_millis(),
            stdout: "".to_string(),
            stderr: e.to_string(),
            workspace: workspace.display().to_string(),
            cpu_usec: None,
        }),
    };

    if ephemeral {
        if let Err(e) = fs::remove_dir_all(&workspace).await {
            eprintln!(
                "warning: failed to remove workspace {}: {e}",
                workspace.display()
            );
        }
    }

    Ok(result)
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

    let script_name = match runtime.as_str() {
        "bash" | "sh" => "main.sh",
        "python" | "python3" | "py" => "main.py",
        "node" | "nodejs" | "js" | "javascript" => "main.js",
        other => return Err(anyhow!("unsupported runtime: {other}")),
    };

    // The script lives in the workspace so it runs with the same cwd and, for
    // python/node, the same module-resolution root as the workspace (relative
    // imports / `require('./x')` resolve against the script's own directory). It
    // is removed after the run so persistent workspaces never accumulate a
    // leftover main.sh/main.py/main.js for the model to see (issue #66).
    let script_path = workspace.join(script_name);
    fs::write(&script_path, &req.code)
        .await
        .context("failed to write code file")?;

    let path_str = script_path.display().to_string();

    let bash_command = match runtime.as_str() {
        "bash" | "sh" => {
            let q = shlex::try_quote(&path_str).map_err(|e| anyhow!("invalid path: {e}"))?;
            let mut cmd = format!("chmod +x {q} && {q}");
            for arg in &req.args {
                let aq = shlex::try_quote(arg).map_err(|e| anyhow!("invalid arg: {e}"))?;
                cmd.push(' ');
                cmd.push_str(&aq);
            }
            cmd
        }
        "python" | "python3" | "py" => {
            let q = shlex::try_quote(&path_str).map_err(|e| anyhow!("invalid path: {e}"))?;
            let mut cmd = format!("/usr/bin/python3 {q}");
            for arg in &req.args {
                let aq = shlex::try_quote(arg).map_err(|e| anyhow!("invalid arg: {e}"))?;
                cmd.push(' ');
                cmd.push_str(&aq);
            }
            cmd
        }
        "node" | "nodejs" | "js" | "javascript" => {
            let q = shlex::try_quote(&path_str).map_err(|e| anyhow!("invalid path: {e}"))?;
            let mut cmd = format!("/usr/bin/node {q}");
            for arg in &req.args {
                let aq = shlex::try_quote(arg).map_err(|e| anyhow!("invalid arg: {e}"))?;
                cmd.push(' ');
                cmd.push_str(&aq);
            }
            cmd
        }
        _ => unreachable!(),
    };

    let mut command = Command::new("bash");
    command
        .arg("-lc")
        .arg(&bash_command)
        .current_dir(workspace)
        .env_clear()
        .env("HOME", workspace)
        .env("TMPDIR", workspace)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin:/opt/bin")
        .kill_on_drop(true);

    for (key, value) in &req.env {
        command.env(key, value);
    }

    let cpu_before = children_cpu_usec();
    let timeout_result = timeout(Duration::from_millis(req.timeout_ms), command.output()).await;
    // Read immediately after the child is reaped (output() awaits the wait), before
    // the parent-side script removal / sync below touch anything.
    let cpu_usec = children_cpu_usec().saturating_sub(cpu_before);

    // Remove the runtime script so it never lingers in a persistent workspace
    // (issue #66). Best-effort: the sync below flushes the unlink too.
    let _ = fs::remove_file(&script_path).await;

    // Flush the bash tool's workspace writes (and the script removal above) to the
    // S3 Files mount before the Lambda freezes. Without this, files written via
    // shell redirection live only in the page cache and are silently lost on the
    // next cold container (issue #46). Ephemeral workspaces are deleted right
    // after the run, so skip them.
    //
    // STOPGAP: this is a coarse per-run flush that only prevents silent data loss
    // on cold containers — it does not address cross-provider durability, hop-2
    // S3 visibility lag, or multi-agent write conflicts. The intended final fix is
    // a unified shared-data layer (Archil-style elastic POSIX FS, mountable across
    // sandboxes) that owns durability + conflict resolution in one place. Tracked
    // in filthy-panty #64; remove this flush once that layer lands.
    if req.namespace.is_some() {
        // SAFETY: sync() takes no arguments and has no failure mode; it flushes
        // all filesystem buffers (including the NFS-backed workspace mount).
        unsafe {
            libc::sync();
        }
    }

    match timeout_result {
        Ok(output_result) => {
            let output = output_result.context("failed to run child process")?;
            let stdout = truncate_string(&String::from_utf8_lossy(&output.stdout), MAX_STDOUT_SIZE);
            let stderr = truncate_string(&String::from_utf8_lossy(&output.stderr), MAX_STDERR_SIZE);
            Ok(ExecResponse {
                ok: output.status.success(),
                runtime: req.runtime.clone(),
                exit_code: output.status.code(),
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                stdout,
                stderr,
                workspace: workspace.display().to_string(),
                cpu_usec: Some(cpu_usec),
            })
        }
        // On timeout the child is SIGKILLed via kill_on_drop and reaped
        // asynchronously, so RUSAGE_CHILDREN may not yet reflect it — omit the
        // sample rather than report a misleading partial number.
        Err(_) => Ok(ExecResponse {
            ok: false,
            runtime: req.runtime.clone(),
            exit_code: None,
            timed_out: true,
            duration_ms: started.elapsed().as_millis(),
            stdout: "".to_string(),
            stderr: format!("execution timed out after {} ms", req.timeout_ms),
            workspace: workspace.display().to_string(),
            cpu_usec: None,
        }),
    }
}

/// Total CPU (user + system) charged to reaped child processes so far, in
/// microseconds. Read once before and once after a child runs; the delta is that
/// child's CPU time. RUSAGE_CHILDREN accumulates process-wide and rolls up the
/// whole descendant tree (bash waits on its subprocess before exiting), and the
/// Lambda runs exactly one child per invocation, so the delta is unambiguous.
fn children_cpu_usec() -> u64 {
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
    use super::children_cpu_usec;

    /// A reaped CPU-burning child must register a non-trivial RUSAGE_CHILDREN
    /// delta — this is the measurement that backs ExecResponse.cpu_usec.
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
}
