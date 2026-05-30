//! Lambda Agent Sandbox — core library for the AWS Lambda custom runtime handler.
//!
//! Spawns bash/python/node subprocesses in isolated workspaces, captures output,
//! enforces timeouts, and returns structured JSON responses.
//!
//! Supports two workspace modes:
//!   - Persistent: `namespace` provided → uses `{workspace_root}/{namespace}`, never cleaned up.
//!     The workspace_root defaults to the SANDBOX_WORKSPACE_MOUNT_PATH env var or /mnt/workspaces.
//!     Files written here survive across invocations (S3 Files mount).
//!   - Ephemeral: no `namespace` → creates a fresh /tmp/agent-workspace/<uuid>, cleaned up after.

use anyhow::{anyhow, Context};
use base64::Engine as _;
use lambda_runtime::{Error, LambdaEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
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
const READ_DIR_DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024; // 16 MB

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    /// Runtime to execute: "bash"/"sh", "python"/"python3"/"py", "node"/"nodejs"/"js",
    /// or "read-dir" to list workspace files.
    #[serde(default = "default_runtime")]
    pub runtime: String,

    /// Inline code to execute. Mutually exclusive with `file_path`.
    #[serde(default)]
    pub code: Option<String>,

    /// Path to an existing file in the workspace to execute, relative to the workspace root.
    /// Mutually exclusive with `code`. Requires `namespace`.
    #[serde(default)]
    pub file_path: Option<String>,

    /// Workspace namespace (must match `fs-[a-f0-9]{40}`). When set the workspace at
    /// `{workspace_root}/{namespace}` is used and never cleaned up — files persist across
    /// calls via the S3 Files mount. When omitted a fresh ephemeral workspace is used.
    #[serde(default)]
    pub namespace: Option<String>,

    /// Override the workspace root directory. Defaults to SANDBOX_WORKSPACE_MOUNT_PATH env
    /// var or /mnt/workspaces.
    #[serde(default)]
    pub workspace_root: Option<String>,

    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    #[serde(default)]
    pub args: Vec<String>,

    #[serde(default)]
    pub env: HashMap<String, String>,

    /// For `read-dir`: subdirectory path relative to the workspace root to list.
    /// Defaults to the workspace root itself.
    #[serde(default)]
    pub path: Option<String>,

    /// For `read-dir`: total byte cap across all file contents. Defaults to 16 MB.
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct FileEntry {
    pub path: String,
    pub base64: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
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

/// Resolve the workspace directory. Returns (path, ephemeral).
/// When ephemeral=true the caller must clean up after use.
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

/// Resolve a relative path safely within `workspace`, rejecting traversal attempts.
fn resolve_entry_path(workspace: &Path, entry: &str) -> Result<PathBuf, anyhow::Error> {
    let normalized = entry.trim_start_matches('/');
    // Manually normalise the path without hitting the filesystem.
    let mut resolved = workspace.to_path_buf();
    for component in Path::new(normalized).components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir | Component::RootDir => {}
            Component::Normal(part) => resolved.push(part),
            Component::Prefix(_) => {}
        }
    }
    if !resolved.starts_with(workspace) {
        return Err(anyhow!("invalid path: resolves outside workspace"));
    }
    Ok(resolved)
}

pub async fn handler(event: LambdaEvent<Value>) -> Result<Value, Error> {
    let started = Instant::now();

    let req: ExecRequest = match serde_json::from_value(event.payload) {
        Ok(r) => r,
        Err(e) => {
            return Ok(json!({
                "ok": false,
                "runtime": "unknown",
                "exit_code": null,
                "timed_out": false,
                "duration_ms": started.elapsed().as_millis(),
                "stdout": "",
                "stderr": format!("invalid request json: {e}"),
                "workspace": "",
            }));
        }
    };

    let runtime = req.runtime.to_lowercase();

    if runtime == "read-dir" {
        return Ok(handle_read_dir(&req, started).await);
    }

    let (workspace, ephemeral) = match resolve_workspace(&req) {
        Ok(w) => w,
        Err(e) => {
            return Ok(json!({
                "ok": false,
                "runtime": req.runtime,
                "exit_code": null,
                "timed_out": false,
                "duration_ms": started.elapsed().as_millis(),
                "stdout": "",
                "stderr": e.to_string(),
                "workspace": "",
            }));
        }
    };

    if let Err(e) = fs::create_dir_all(&workspace).await {
        return Ok(json!({
            "ok": false,
            "runtime": req.runtime,
            "exit_code": null,
            "timed_out": false,
            "duration_ms": started.elapsed().as_millis(),
            "stdout": "",
            "stderr": format!("failed to create workspace: {e}"),
            "workspace": "",
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
            files: None,
            truncated: None,
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

async fn handle_read_dir(req: &ExecRequest, started: Instant) -> Value {
    let (workspace, _) = match resolve_workspace(req) {
        Ok(w) => w,
        Err(e) => {
            return json!({
                "ok": false,
                "runtime": "read-dir",
                "timed_out": false,
                "duration_ms": started.elapsed().as_millis(),
                "stdout": "",
                "stderr": e.to_string(),
                "workspace": "",
                "files": [],
            });
        }
    };

    let sub_path = req.path.as_deref().unwrap_or(".");
    let target_dir = match resolve_entry_path(&workspace, sub_path) {
        Ok(p) => p,
        Err(e) => {
            return json!({
                "ok": false,
                "runtime": "read-dir",
                "timed_out": false,
                "duration_ms": started.elapsed().as_millis(),
                "stdout": "",
                "stderr": e.to_string(),
                "workspace": workspace.display().to_string(),
                "files": [],
            });
        }
    };

    let max_bytes = req.max_bytes.unwrap_or(READ_DIR_DEFAULT_MAX_BYTES);

    // Collect all file paths with a BFS, sorted at each level for deterministic output.
    let mut file_paths: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = vec![target_dir.clone()];
    while let Some(dir) = dirs.first().cloned() {
        dirs.remove(0);
        let mut rd = match fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => {
                return json!({
                    "ok": false,
                    "runtime": "read-dir",
                    "timed_out": false,
                    "duration_ms": started.elapsed().as_millis(),
                    "stdout": "",
                    "stderr": format!("failed to read directory: {e}"),
                    "workspace": workspace.display().to_string(),
                    "files": [],
                });
            }
        };
        let mut entries: Vec<PathBuf> = Vec::new();
        while let Ok(Some(entry)) = rd.next_entry().await {
            entries.push(entry.path());
        }
        entries.sort();
        for path in entries {
            let meta = match fs::metadata(&path).await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                dirs.push(path);
            } else if meta.is_file() {
                file_paths.push(path);
            }
        }
    }

    let mut files: Vec<Value> = Vec::new();
    let mut total_bytes: usize = 0;
    let mut truncated = false;

    for path in &file_paths {
        if truncated {
            break;
        }
        let bytes = match fs::read(path).await {
            Ok(b) => b,
            Err(_) => continue,
        };
        if total_bytes + bytes.len() > max_bytes {
            truncated = true;
            break;
        }
        total_bytes += bytes.len();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let rel = path
            .strip_prefix(&target_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        files.push(json!({ "path": rel, "base64": b64 }));
    }

    let mut resp = json!({
        "ok": true,
        "runtime": "read-dir",
        "timed_out": false,
        "duration_ms": started.elapsed().as_millis(),
        "stdout": "",
        "stderr": "",
        "workspace": workspace.display().to_string(),
        "files": files,
    });
    if truncated {
        resp["truncated"] = json!(true);
    }
    resp
}

async fn execute_request(
    req: &ExecRequest,
    workspace: &PathBuf,
    started: Instant,
) -> anyhow::Result<ExecResponse> {
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

    // Resolve the script to execute: either write inline code to the workspace,
    // or reference an existing file at file_path within the workspace.
    let script_path: PathBuf = match (&req.code, &req.file_path) {
        (Some(code), _) => {
            if code.len() > MAX_CODE_BYTES {
                return Err(anyhow!(
                    "code exceeds maximum size of {} bytes",
                    MAX_CODE_BYTES
                ));
            }
            let name = match runtime.as_str() {
                "bash" | "sh" => "main.sh",
                "python" | "python3" | "py" => "main.py",
                "node" | "nodejs" | "js" | "javascript" => "main.js",
                other => return Err(anyhow!("unsupported runtime: {other}")),
            };
            let p = workspace.join(name);
            fs::write(&p, code)
                .await
                .context("failed to write code file")?;
            p
        }
        (None, Some(fp)) => {
            match runtime.as_str() {
                "bash" | "sh" | "python" | "python3" | "py"
                | "node" | "nodejs" | "js" | "javascript" => {}
                other => return Err(anyhow!("unsupported runtime: {other}")),
            }
            resolve_entry_path(workspace, fp)?
        }
        (None, None) => {
            return Err(anyhow!("either code or file_path must be provided"));
        }
    };

    let path_str = script_path.display().to_string();
    let q = shlex::try_quote(&path_str).map_err(|e| anyhow!("invalid path: {e}"))?;

    let bash_command = match runtime.as_str() {
        "bash" | "sh" => {
            let mut cmd = format!("chmod +x {q} && {q}");
            for arg in &req.args {
                let aq = shlex::try_quote(arg).map_err(|e| anyhow!("invalid arg: {e}"))?;
                cmd.push(' ');
                cmd.push_str(&aq);
            }
            cmd
        }
        "python" | "python3" | "py" => {
            let mut cmd = format!("/usr/bin/python3 {q}");
            for arg in &req.args {
                let aq = shlex::try_quote(arg).map_err(|e| anyhow!("invalid arg: {e}"))?;
                cmd.push(' ');
                cmd.push_str(&aq);
            }
            cmd
        }
        "node" | "nodejs" | "js" | "javascript" => {
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

    let timeout_result = timeout(Duration::from_millis(req.timeout_ms), command.output()).await;

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
                files: None,
                truncated: None,
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
            files: None,
            truncated: None,
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
