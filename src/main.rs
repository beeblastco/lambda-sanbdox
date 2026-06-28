//! HTTP server entrypoint for the AWS Lambda MicroVM sandbox image.
//!
//! A MicroVM runs this container as a long-lived server (the old Lambda Invoke
//! bootstrap is gone). Two listeners, both bound to 0.0.0.0:
//!   - **:8080** — the exec API the proxy routes external 443 to. `POST /exec`
//!     takes the same JSON contract the Invoke handler used and returns the same
//!     response, so the harness's transport change is the only difference.
//!   - **:9000** — the lifecycle hooks Lambda calls (`/run` mounts the workspace
//!     S3 prefix; `/suspend`/`/terminate` flush it). Configured via `--hooks` at
//!     image-create time; both ports must be `EXPOSE`d in the Dockerfile.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, routing::get, routing::post, Json, Router};
use lambda_agent_sandbox::{mount, run_exec, ExecRequest, ExecResponse};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

const EXEC_PORT: u16 = 8080;
const HOOKS_PORT: u16 = 9000;
const HOOK_BASE: &str = "/aws/lambda-microvms/runtime/v1";

#[derive(Clone)]
struct AppState {
    // Serialize execs: cpu_usec is a delta of a process-wide counter, so overlapping
    // children would corrupt each other's accounting. One exec at a time keeps it exact.
    exec_lock: Arc<Mutex<()>>,
    // The workspace mount established by `/run`, so `/terminate` can flush it.
    mount_point: Arc<Mutex<Option<String>>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state = AppState {
        exec_lock: Arc::new(Mutex::new(())),
        mount_point: Arc::new(Mutex::new(None)),
    };

    let exec_app = Router::new()
        .route("/", get(health))
        .route("/healthz", get(health))
        .route("/exec", post(exec_handler))
        .with_state(state.clone());

    let hooks_app = Router::new()
        .route(&format!("{HOOK_BASE}/ready"), post(ok_hook))
        .route(&format!("{HOOK_BASE}/validate"), post(ok_hook))
        .route(&format!("{HOOK_BASE}/run"), post(run_hook))
        .route(&format!("{HOOK_BASE}/resume"), post(ok_hook))
        .route(&format!("{HOOK_BASE}/suspend"), post(suspend_hook))
        .route(&format!("{HOOK_BASE}/terminate"), post(terminate_hook))
        .with_state(state.clone());

    let exec_listener = TcpListener::bind(("0.0.0.0", EXEC_PORT)).await?;
    let hooks_listener = TcpListener::bind(("0.0.0.0", HOOKS_PORT)).await?;
    eprintln!("sandbox-server: exec on :{EXEC_PORT}, hooks on :{HOOKS_PORT}");

    // Run both servers; if either exits, propagate so the VM is recycled.
    tokio::try_join!(
        async {
            axum::serve(exec_listener, exec_app)
                .await
                .map_err(anyhow::Error::from)
        },
        async {
            axum::serve(hooks_listener, hooks_app)
                .await
                .map_err(anyhow::Error::from)
        },
    )?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

/// `POST /exec` — parse the request, run it under the exec lock, return the result.
/// Bad JSON yields a 200 + `ok:false` body (unchanged from the Invoke handler) so the
/// harness gets a structured error rather than an HTTP failure.
async fn exec_handler(State(state): State<AppState>, body: String) -> Json<ExecResponse> {
    let req: ExecRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return Json(invalid_request(format!("invalid request json: {e}"))),
    };
    let _guard = state.exec_lock.lock().await;
    Json(run_exec(req).await)
}

fn invalid_request(stderr: String) -> ExecResponse {
    ExecResponse {
        ok: false,
        runtime: "unknown".to_string(),
        exit_code: None,
        timed_out: false,
        duration_ms: 0,
        stdout: String::new(),
        stderr,
        workspace: String::new(),
        cpu_usec: None,
    }
}

async fn ok_hook() -> StatusCode {
    StatusCode::OK
}

/// `/run` — mount the workspace S3 prefix (if the payload carries one). A mount
/// failure returns 500 so the platform fails the run rather than dropping the agent
/// into an unmounted local directory where writes would be silently lost.
async fn run_hook(State(state): State<AppState>, body: String) -> StatusCode {
    match mount::parse_and_mount(&body).await {
        Ok(Some(point)) => {
            eprintln!("/run: mounted workspace at {point}");
            *state.mount_point.lock().await = Some(point);
            StatusCode::OK
        }
        Ok(None) => StatusCode::OK,
        Err(e) => {
            eprintln!("/run: workspace mount failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// `/suspend` — flush filesystem buffers before the snapshot. mountpoint-s3 uploads
/// on file close so most data is already durable; the sync is a cheap backstop.
async fn suspend_hook() -> StatusCode {
    flush();
    StatusCode::OK
}

/// `/terminate` — unmount the workspace to complete any in-flight uploads, then flush.
async fn terminate_hook(State(state): State<AppState>) -> StatusCode {
    if let Some(point) = state.mount_point.lock().await.take() {
        mount::unmount(&point).await;
    }
    flush();
    StatusCode::OK
}

fn flush() {
    // SAFETY: sync() takes no arguments and cannot fail; it flushes all filesystem
    // buffers. Best-effort durability backstop before suspend/terminate.
    unsafe {
        libc::sync();
    }
}
