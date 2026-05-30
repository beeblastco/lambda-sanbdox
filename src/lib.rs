//! Lambda Agent Sandbox — core library for the AWS Lambda custom runtime handler.
//!
//! Spawns bash/python/node subprocesses in isolated workspaces, captures output,
//! enforces timeouts, and returns structured JSON responses.

use anyhow::{anyhow, Context};
use lambda_runtime::{Error, LambdaEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    #[serde(default = "default_runtime")]
    pub runtime: String,

    pub code: String,

    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    #[serde(default)]
    pub args: Vec<String>,

    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
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
}

fn default_runtime() -> String {
    "bash".to_string()
}

fn default_timeout_ms() -> u64 {
    30_000
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
            }));
        }
    };

    let workspace = PathBuf::from("/tmp/agent-workspace").join(Uuid::new_v4().to_string());
    if let Err(e) = fs::create_dir_all(&workspace).await {
        return Ok(json!(ExecResponse {
            ok: false,
            runtime: req.runtime.clone(),
            exit_code: None,
            timed_out: false,
            duration_ms: started.elapsed().as_millis(),
            stdout: "".to_string(),
            stderr: format!("failed to create workspace: {e}"),
            workspace: "".to_string(),
        }));
    }

    let result = match execute_request(&req, &workspace, started).await {
        Ok(resp) => resp,
        Err(e) => ExecResponse {
            ok: false,
            runtime: req.runtime.clone(),
            exit_code: None,
            timed_out: false,
            duration_ms: started.elapsed().as_millis(),
            stdout: "".to_string(),
            stderr: e.to_string(),
            workspace: workspace.display().to_string(),
        },
    };

    // Best-effort workspace cleanup; do not fail the response if removal fails.
    if let Err(e) = fs::remove_dir_all(&workspace).await {
        eprintln!(
            "warning: failed to remove workspace {}: {e}",
            workspace.display()
        );
    }

    Ok(json!(result))
}

async fn execute_request(
    req: &ExecRequest,
    workspace: &PathBuf,
    started: Instant,
) -> anyhow::Result<ExecResponse> {
    // Input size limits
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

    let child = command.output();

    let timeout_result = timeout(Duration::from_millis(req.timeout_ms), child).await;

    match timeout_result {
        Ok(output_result) => {
            let output = output_result.context("failed to run child process")?;

            let stdout_raw = String::from_utf8_lossy(&output.stdout);
            let stderr_raw = String::from_utf8_lossy(&output.stderr);

            let stdout = truncate_string(&stdout_raw, MAX_STDOUT_SIZE);
            let stderr = truncate_string(&stderr_raw, MAX_STDERR_SIZE);

            Ok(ExecResponse {
                ok: output.status.success(),
                runtime: req.runtime.clone(),
                exit_code: output.status.code(),
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                stdout,
                stderr,
                workspace: workspace.display().to_string(),
            })
        }
        Err(_) => Ok(ExecResponse {
            ok: false,
            runtime: req.runtime.clone(),
            exit_code: None,
            timed_out: true,
            duration_ms: started.elapsed().as_millis(),
            stdout: "".to_string(),
            stderr: format!("execution timed out after {} ms", req.timeout_ms),
            workspace: workspace.display().to_string(),
        }),
    }
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
