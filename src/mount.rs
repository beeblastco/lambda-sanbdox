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
//! Those sessions expire in an hour, which a persistent VM outlives. So mountpoint-s3
//! never gets the keys directly: it reads them through a `credential_process` that
//! prints a local file, which `/run` writes at boot and the harness rewrites through
//! `POST /workspace/credentials`. The process is local, so it works during the
//! boot-time `/run` when no network endpoint is reachable, and the AWS CRT's
//! credential cache re-runs it as the session nears expiry. A mount given static
//! env keys instead keeps them for its whole life and fails with EIO an hour in.
//!
//! The mount prefix already encodes the namespace (`<prefix>/<namespace>/`), and the
//! local mount point also ends in the namespace, so the two stay aligned with the
//! exec engine's independent `{root}/{namespace}` join — no double-prefixing.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Path the harness posts refreshed mount credentials to.
pub const CREDENTIALS_PATH: &str = "/workspace/credentials";

/// Where the mount's credentials and the AWS config naming them live. Outside every
/// workspace, so they never sync to S3.
pub const CREDENTIALS_DIR: &str = "/run/sandbox-mount";

const AWS_CONFIG_FILE: &str = "aws-config";
const CREDENTIALS_FILE: &str = "credentials.json";

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
    /// RFC3339 expiry. Drives the credential cache's refresh clock; without it the
    /// session reads as non-expiring and is never re-fetched.
    #[serde(rename = "AWS_CREDENTIAL_EXPIRATION", default)]
    pub expiration: Option<String>,
}

/// What a `credential_process` prints, in the AWS CLI's documented shape.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct ProcessCredentials<'a> {
    version: u8,
    access_key_id: &'a str,
    secret_access_key: &'a str,
    session_token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    expiration: Option<String>,
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
pub async fn mount_workspace(ws: &Workspace, credentials_dir: &Path) -> anyhow::Result<String> {
    let point = mount_point(&ws.root, &ws.namespace);
    tokio::fs::create_dir_all(&point)
        .await
        .with_context(|| format!("create mount dir {point}"))?;
    // Before the mounted check: a retried `/run` still leaves the freshest session.
    if let Some(creds) = &ws.mount.env {
        write_credentials(credentials_dir, creds).await?;
    }

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
    cmd.env_clear()
        .envs(mount_env(ws.mount.env.as_ref().map(|_| credentials_dir)));

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

/// Environment for mount-s3. With scoped credentials, only `AWS_CONFIG_FILE`: static
/// keys in the environment would win the provider chain and never refresh. Without
/// them, the default chain resolves the MicroVM execution role.
pub fn mount_env(credentials_dir: Option<&Path>) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("HOME", "/root".to_string()),
        ("PATH", "/usr/local/bin:/usr/bin:/bin".to_string()),
    ];
    if let Some(dir) = credentials_dir {
        env.push((
            "AWS_CONFIG_FILE",
            dir.join(AWS_CONFIG_FILE).display().to_string(),
        ));
    }

    env
}

/// Stock the files mountpoint-s3's `credential_process` reads. Each file is replaced
/// by rename, so a refresh racing a read sees the old session or the new one, never
/// half of either.
pub async fn write_credentials(dir: &Path, creds: &MountCredentials) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("create credentials dir {}", dir.display()))?;
    tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    let credentials = ProcessCredentials {
        version: 1,
        access_key_id: &creds.access_key_id,
        secret_access_key: &creds.secret_access_key,
        session_token: &creds.session_token,
        expiration: creds.expiration.as_deref().map(whole_seconds),
    };
    let credentials_file = dir.join(CREDENTIALS_FILE);
    write_private(&credentials_file, &serde_json::to_vec(&credentials)?).await?;
    let config = format!(
        "[default]\ncredential_process = /bin/cat {}\n",
        credentials_file.display()
    );

    write_private(&dir.join(AWS_CONFIG_FILE), config.as_bytes()).await
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

// JavaScript's `toISOString` adds milliseconds. Keep the plain RFC3339 form the
// AWS CLI's `credential_process` examples use, so no parser has to handle them.
fn whole_seconds(expiration: &str) -> String {
    match (expiration.find('.'), expiration.rfind('Z')) {
        (Some(dot), Some(zone)) if dot < zone => {
            format!("{}{}", &expiration[..dot], &expiration[zone..])
        }
        _ => expiration.to_string(),
    }
}

async fn write_private(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let staged = PathBuf::from(format!("{}.tmp", path.display()));
    tokio::fs::write(&staged, contents)
        .await
        .with_context(|| format!("write {}", staged.display()))?;
    tokio::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600)).await?;
    tokio::fs::rename(&staged, path)
        .await
        .with_context(|| format!("replace {}", path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(access_key_id: &str) -> MountCredentials {
        MountCredentials {
            access_key_id: access_key_id.to_string(),
            secret_access_key: "secret".to_string(),
            session_token: "token".to_string(),
            expiration: Some("2026-09-17T14:00:00.000Z".to_string()),
        }
    }

    // What mountpoint-s3 gets when it asks for credentials: the configured
    // `credential_process`, run the way the CRT runs it.
    async fn run_credential_process(dir: &Path) -> serde_json::Value {
        let config = tokio::fs::read_to_string(dir.join(AWS_CONFIG_FILE))
            .await
            .expect("config");
        let command = config
            .lines()
            .find_map(|line| line.strip_prefix("credential_process = "))
            .expect("credential_process line");
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .await
            .expect("run credential_process");

        serde_json::from_slice(&output.stdout).expect("process output json")
    }

    // The mount outlives its one-hour session, so it must not pin static keys: they
    // would win the provider chain and the refreshed session would never be read.
    #[test]
    fn mount_env_reads_scoped_credentials_through_the_config_file() {
        let env = mount_env(Some(Path::new("/run/sandbox-mount")));

        assert!(env.iter().all(|(name, _)| !name.starts_with("AWS_ACCESS")
            && !name.starts_with("AWS_SECRET")
            && *name != "AWS_SESSION_TOKEN"));
        assert!(env.contains(&(
            "AWS_CONFIG_FILE",
            "/run/sandbox-mount/aws-config".to_string()
        )));
        assert!(mount_env(None)
            .iter()
            .all(|(name, _)| !name.starts_with("AWS_")));
    }

    #[tokio::test]
    async fn a_refreshed_session_reaches_the_credential_process() {
        let dir = std::env::temp_dir().join(format!("sandbox-mount-{}", uuid::Uuid::new_v4()));

        write_credentials(&dir, &session("AKIA_BOOT"))
            .await
            .expect("write boot");
        let boot = run_credential_process(&dir).await;
        write_credentials(&dir, &session("AKIA_REFRESHED"))
            .await
            .expect("write refresh");
        let refreshed = run_credential_process(&dir).await;

        assert_eq!(boot["Version"], 1);
        assert_eq!(boot["AccessKeyId"], "AKIA_BOOT");
        assert_eq!(boot["SessionToken"], "token");
        assert_eq!(boot["Expiration"], "2026-09-17T14:00:00Z");
        assert_eq!(refreshed["AccessKeyId"], "AKIA_REFRESHED");
        let mode = std::fs::metadata(dir.join(CREDENTIALS_FILE))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

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
