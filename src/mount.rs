//! S3 workspace mount for the `/run` lifecycle hook.
//!
//! The harness delivers the mount target plus short-lived, namespace-scoped
//! credentials in the MicroVM `runHookPayload` (the body of `POST /run`). We mount
//! the bucket prefix at `{root}/{namespace}` with mountpoint-s3 so the exec
//! engine's persistent-workspace path (`{workspace_root}/{namespace}`) lands on S3.
//! This mirrors the daytona/workdir mount-s3 model: the harness's broad runtime
//! credentials never reach the VM — only the prefix-scoped session credentials do,
//! and any code the agent runs can read this env, so nothing wider may be passed.
//!
//! The mount prefix already encodes the namespace (`<prefix>/<namespace>/`), and the
//! local mount point also ends in the namespace, so the two stay aligned with the
//! exec engine's independent `{root}/{namespace}` join — no double-prefixing.

use anyhow::{anyhow, Context};
use serde::Deserialize;
use tokio::process::Command;

/// Body of `POST /aws/lambda-microvms/runtime/v1/run`. Lambda injects `microvmId`
/// alongside the `runHookPayload` we passed to `RunMicrovm`; we only read `workspace`.
#[derive(Debug, Deserialize)]
pub struct RunHookPayload {
    #[serde(default)]
    pub workspace: Option<Workspace>,
}

#[derive(Debug, Deserialize)]
pub struct Workspace {
    pub namespace: String,
    pub root: String,
    pub mount: Mount,
}

#[derive(Debug, Deserialize)]
pub struct Mount {
    pub bucket: String,
    pub prefix: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Short-lived STS credentials scoped to `bucket/prefix*`. Absent => fall back to
    /// mountpoint-s3's default chain (the MicroVM execution role via IMDSv2).
    #[serde(default)]
    pub env: Option<MountCredentials>,
}

#[derive(Debug, Deserialize)]
pub struct MountCredentials {
    #[serde(rename = "AWS_ACCESS_KEY_ID")]
    pub access_key_id: String,
    #[serde(rename = "AWS_SECRET_ACCESS_KEY")]
    pub secret_access_key: String,
    #[serde(rename = "AWS_SESSION_TOKEN")]
    pub session_token: String,
}

/// Local mount point for a workspace: `{root}/{namespace}` with `root`'s trailing
/// slashes trimmed. Kept pure so it can be unit-tested without touching the FS.
pub fn mount_point(root: &str, namespace: &str) -> String {
    format!("{}/{}", root.trim_end_matches('/'), namespace)
}

/// Parse the `/run` body and, if it carries a workspace, mount it. Returns the
/// mount point on success, `None` for a stateless run (no workspace), or an error
/// the hook turns into a non-200 so the platform fails the run loudly rather than
/// silently dropping the agent into an unmounted local directory.
pub async fn parse_and_mount(body: &str) -> anyhow::Result<Option<String>> {
    let payload: RunHookPayload =
        serde_json::from_str(body).context("invalid /run hook payload json")?;
    match payload.workspace {
        Some(ws) => Ok(Some(mount_workspace(&ws).await?)),
        None => Ok(None),
    }
}

/// Mount `ws.mount.bucket` at `{root}/{namespace}` via mountpoint-s3. Idempotent:
/// `/run` may be retried, and a path already mounted is left as-is.
pub async fn mount_workspace(ws: &Workspace) -> anyhow::Result<String> {
    let point = mount_point(&ws.root, &ws.namespace);
    tokio::fs::create_dir_all(&point)
        .await
        .with_context(|| format!("create mount dir {point}"))?;

    if is_mounted(&point).await {
        return Ok(point);
    }

    let mut cmd = Command::new("mount-s3");
    cmd.arg(&ws.mount.bucket)
        .arg(&point)
        .arg("--prefix")
        .arg(&ws.mount.prefix)
        .arg("--allow-delete")
        .arg("--allow-overwrite");
    if let Some(region) = &ws.mount.region {
        cmd.arg("--region").arg(region);
    }
    if let Some(endpoint) = &ws.mount.endpoint {
        cmd.arg("--endpoint-url").arg(endpoint);
    }

    // Clear inherited env so only the scoped mount credentials reach mountpoint-s3.
    // When no credentials are passed, mountpoint-s3 uses its default provider chain
    // (the MicroVM execution role via IMDSv2) — the intended path for persistent
    // VMs whose short-lived runHookPayload credentials would have expired.
    cmd.env_clear()
        .env("HOME", "/root")
        .env("PATH", "/usr/local/bin:/usr/bin:/bin");
    if let Some(creds) = &ws.mount.env {
        cmd.env("AWS_ACCESS_KEY_ID", &creds.access_key_id)
            .env("AWS_SECRET_ACCESS_KEY", &creds.secret_access_key)
            .env("AWS_SESSION_TOKEN", &creds.session_token);
    }

    let output = cmd.output().await.context("spawn mount-s3")?;
    if !output.status.success() {
        return Err(anyhow!(
            "mount-s3 failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(point)
}

/// Best-effort unmount, used by `/terminate` to flush mountpoint-s3's in-flight
/// uploads before the VM is destroyed. Never fails the caller.
pub async fn unmount(point: &str) {
    let _ = Command::new("umount").arg(point).output().await;
}

/// True if `point` is already a mount point (so `/run` retries don't double-mount).
async fn is_mounted(point: &str) -> bool {
    Command::new("mountpoint")
        .arg("-q")
        .arg(point)
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_point_trims_trailing_slashes() {
        assert_eq!(
            mount_point("/mnt/workspaces", "fs-abc"),
            "/mnt/workspaces/fs-abc"
        );
        assert_eq!(
            mount_point("/mnt/workspaces/", "fs-abc"),
            "/mnt/workspaces/fs-abc"
        );
    }

    #[test]
    fn parses_workspace_payload_with_credentials() {
        let body = r#"{
            "workspace": {
                "namespace": "fs-0123456789abcdef0123456789abcdef01234567",
                "root": "/mnt/workspaces",
                "mount": {
                    "bucket": "my-bucket",
                    "prefix": "sandbox/fs-0123456789abcdef0123456789abcdef01234567/",
                    "region": "us-east-1",
                    "env": {
                        "AWS_ACCESS_KEY_ID": "AKIA",
                        "AWS_SECRET_ACCESS_KEY": "secret",
                        "AWS_SESSION_TOKEN": "token"
                    }
                }
            },
            "microvmId": "microvm-123"
        }"#;
        let payload: RunHookPayload = serde_json::from_str(body).expect("parse");
        let ws = payload.workspace.expect("workspace present");
        assert_eq!(ws.mount.bucket, "my-bucket");
        assert_eq!(ws.mount.region.as_deref(), Some("us-east-1"));
        assert!(ws.mount.endpoint.is_none());
        assert_eq!(ws.mount.env.as_ref().unwrap().access_key_id, "AKIA");
        assert_eq!(
            mount_point(&ws.root, &ws.namespace),
            "/mnt/workspaces/fs-0123456789abcdef0123456789abcdef01234567"
        );
    }

    #[test]
    fn stateless_payload_has_no_workspace() {
        let payload: RunHookPayload =
            serde_json::from_str(r#"{"microvmId":"microvm-1"}"#).expect("parse");
        assert!(payload.workspace.is_none());
    }
}
