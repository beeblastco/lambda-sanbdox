//! S3 workspace mount for the `/run` lifecycle hook.
//!
//! The harness delivers the mount target plus short-lived, namespace-scoped
//! credentials in the MicroVM `runHookPayload` (the body of `POST /run`). We mount
//! the bucket prefix at `{root}/{namespace}` with mountpoint-s3 so the exec
//! engine's persistent-workspace path (`{workspace_root}/{namespace}`) lands on S3.
//! This mirrors the daytona/workdir mount-s3 model: the harness's broad runtime
//! credentials never reach the VM — only the prefix-scoped session credentials do,
//! and any code the agent runs can read them, so nothing wider may be passed.
//!
//! Those sessions expire in an hour, which a persistent VM outlives. The mount takes
//! the static keys (the credential endpoint is not reachable during the boot-time
//! `/run`), and the harness remounts with a fresh session before they expire.
//!
//! The mount prefix already encodes the namespace (`<prefix>/<namespace>/`), and the
//! local mount point also ends in the namespace, so the two stay aligned with the
//! exec engine's independent `{root}/{namespace}` join — no double-prefixing.

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Path the local credential endpoint is served on. The harness keeps it stocked so
/// a remount always has a live session to use.
pub const CREDENTIALS_PATH: &str = "/workspace/credentials";

/// Body of `POST /aws/lambda-microvms/runtime/v1/run`. Lambda does not spread what
/// we handed `RunMicrovm` at the top level — it nests it under `runHookPayload`, as a
/// JSON *string*, beside `microvmId`. Reading only a top-level `workspace` made every
/// mount a silent no-op: the hook found nothing to do and answered 200.
#[derive(Debug, Deserialize)]
pub struct RunHookPayload {
    #[serde(default)]
    pub workspace: Option<Workspace>,
    #[serde(default, rename = "runHookPayload")]
    pub run_hook_payload: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Workspace {
    pub namespace: String,
    pub root: String,
    pub mount: Mount,
}

#[derive(Clone, Debug, Deserialize)]
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

#[derive(Clone, Debug, Deserialize)]
pub struct MountCredentials {
    #[serde(rename = "AWS_ACCESS_KEY_ID")]
    pub access_key_id: String,
    #[serde(rename = "AWS_SECRET_ACCESS_KEY")]
    pub secret_access_key: String,
    #[serde(rename = "AWS_SESSION_TOKEN")]
    pub session_token: String,
    /// RFC3339 expiry. Drives mountpoint-s3's refresh clock; without it the SDK
    /// treats the session as non-expiring and never re-fetches.
    #[serde(rename = "AWS_CREDENTIAL_EXPIRATION", default)]
    pub expiration: Option<String>,
}

/// The container-credential-provider shape mountpoint-s3 expects from
/// `AWS_CONTAINER_CREDENTIALS_FULL_URI`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<String>,
}

impl From<&MountCredentials> for ContainerCredentials {
    fn from(creds: &MountCredentials) -> Self {
        Self {
            access_key_id: creds.access_key_id.clone(),
            secret_access_key: creds.secret_access_key.clone(),
            token: creds.session_token.clone(),
            expiration: creds.expiration.clone(),
        }
    }
}

/// Local mount point for a workspace: `{root}/{namespace}` with `root`'s trailing
/// slashes trimmed. Kept pure so it can be unit-tested without touching the FS.
pub fn mount_point(root: &str, namespace: &str) -> String {
    format!("{}/{}", root.trim_end_matches('/'), namespace)
}

/// Parse the `/run` body. `None` is a stateless run (no workspace); an error is
/// turned into a non-200 by the hook so the platform fails the run loudly rather
/// than silently dropping the agent into an unmounted local directory.
pub fn parse_payload(body: &str) -> anyhow::Result<Option<Workspace>> {
    let payload: RunHookPayload =
        serde_json::from_str(body).context("invalid /run hook payload json")?;
    if payload.workspace.is_some() {
        return Ok(payload.workspace);
    }
    let Some(nested) = payload.run_hook_payload else {
        return Ok(None);
    };
    let inner: RunHookPayload = match nested {
        serde_json::Value::String(raw) => {
            serde_json::from_str(&raw).context("invalid nested runHookPayload json")?
        }
        other => serde_json::from_value(other).context("invalid nested runHookPayload")?,
    };

    Ok(inner.workspace)
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
    // These are the session's static keys: the credential endpoint is not reachable
    // during the boot-time `/run`, and a mount that cannot resolve credentials fails
    // the whole VM. The session expires in an hour, so the harness remounts before
    // then (see remount on `/resume` and the harness-side refresh).
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
    // mount-s3 daemonizes, so a zero exit only means the parent forked cleanly.
    // Assert the mount is really live rather than let the agent write to the plain
    // directory underneath it, where every file would be silently lost.
    if !is_mounted(&point).await {
        return Err(anyhow!("mount-s3 exited 0 but {point} is not a mountpoint"));
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
        assert!(parse_payload(r#"{"microvmId":"microvm-1"}"#)
            .expect("parse")
            .is_none());
    }

    // The shape Lambda actually delivers. Parsing only the flat one turned every
    // workspace mount into a silent no-op that still answered 200.
    #[test]
    fn parses_the_payload_lambda_nests_as_a_string() {
        let inner = r#"{"workspace":{"namespace":"fs-abc","root":"/mnt/workspaces","mount":{"bucket":"b","prefix":"fs-abc/"}}}"#;
        let body =
            serde_json::json!({ "microvmId": "microvm-1", "runHookPayload": inner }).to_string();
        let ws = parse_payload(&body).expect("parse").expect("workspace");
        assert_eq!(ws.mount.bucket, "b");
        assert_eq!(
            mount_point(&ws.root, &ws.namespace),
            "/mnt/workspaces/fs-abc"
        );
    }

    #[test]
    fn parses_the_nested_payload_when_delivered_as_an_object() {
        let body = serde_json::json!({
            "microvmId": "microvm-1",
            "runHookPayload": {
                "workspace": {
                    "namespace": "fs-abc",
                    "root": "/mnt/workspaces",
                    "mount": { "bucket": "b", "prefix": "fs-abc/" }
                }
            }
        })
        .to_string();
        let ws = parse_payload(&body).expect("parse").expect("workspace");
        assert_eq!(ws.mount.bucket, "b");
    }

    #[test]
    fn a_malformed_nested_payload_fails_loudly() {
        let body = serde_json::json!({ "runHookPayload": "{not json" }).to_string();
        assert!(parse_payload(&body).is_err());
    }
}
