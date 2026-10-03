//! Stdio MCP servers hosted inside the VM, behind `POST /mcp`.
//!
//! The harness names a server and the command that starts it, plus one JSON-RPC
//! message. The first message for a name spawns the command and runs the MCP
//! `initialize` handshake; later ones reuse the same process, so a stateful
//! server (a browser session, say) keeps its state for as long as the VM lives.
//! A server whose process exited, or whose command or environment changed, is
//! started again. Exec already runs arbitrary code as root here, so starting a
//! command is no new privilege; the MicroVM stays the isolation boundary.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex};

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::time::{timeout, Duration};

/// The MCP revision the host offers in `initialize`; the server answers with its own.
const PROTOCOL_VERSION: &str = "2025-06-18";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Distinct server names one VM keeps running; each is a live process.
const MAX_SERVERS: usize = 16;

type Pending = Arc<SyncMutex<HashMap<u64, oneshot::Sender<Value>>>>;
/// One server name's running process, locked while it starts.
type Slot = Arc<Mutex<Option<Arc<McpServer>>>>;

#[derive(Debug, Deserialize)]
pub struct McpRequest {
    /// Name the host keys the running process by.
    pub server: String,
    /// argv of the stdio server, e.g. `["obscura", "mcp"]`.
    pub command: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// One JSON-RPC request or notification, forwarded as is apart from its id.
    pub message: Value,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// Every stdio server this VM is running. Each name has its own slot, so a server
/// that is slow to start holds up only the calls for that name.
#[derive(Default)]
pub struct McpHost {
    slots: Mutex<HashMap<String, Slot>>,
}

impl McpHost {
    /// Forward one message and return the server's JSON-RPC response, or `None`
    /// for a notification, which has none.
    pub async fn handle(&self, request: McpRequest) -> anyhow::Result<Option<Value>> {
        if request.server.trim().is_empty() {
            bail!("server is required");
        }
        if request.command.is_empty() || request.command[0].trim().is_empty() {
            bail!("command is required");
        }
        let server = self.server(&request).await?;
        let wait = Duration::from_millis(request.timeout_ms.clamp(1, MAX_TIMEOUT_MS));

        server.forward(request.message, wait).await
    }

    async fn server(&self, request: &McpRequest) -> anyhow::Result<Arc<McpServer>> {
        let slot = {
            let mut slots = self.slots.lock().await;
            if !slots.contains_key(&request.server) && slots.len() >= MAX_SERVERS {
                bail!("this VM already runs {MAX_SERVERS} MCP servers");
            }
            slots.entry(request.server.clone()).or_default().clone()
        };
        let mut slot = slot.lock().await;
        if let Some(existing) = slot.as_ref() {
            if existing.alive.load(Ordering::SeqCst)
                && existing.command == request.command
                && existing.env == request.env
            {
                return Ok(existing.clone());
            }
            // Stop the old process before its replacement starts: it may hold
            // something the new one needs, and in-flight calls keep it referenced.
            existing.child.lock().await.start_kill().ok();
        }
        let server = McpServer::start(request).await?;
        *slot = Some(server.clone());

        Ok(server)
    }
}

struct McpServer {
    command: Vec<String>,
    env: HashMap<String, String>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    child: Mutex<Child>,
}

impl McpServer {
    async fn start(request: &McpRequest) -> anyhow::Result<Arc<Self>> {
        let mut child = Command::new(&request.command[0])
            .args(&request.command[1..])
            .envs(&request.env)
            .current_dir("/tmp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The server's logs go to the VM's own output, which reaches CloudWatch.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start MCP server {:?}", request.command))?;
        let stdin = Arc::new(Mutex::new(
            child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?,
        ));
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        tokio::spawn(read_messages(
            stdout,
            stdin.clone(),
            pending.clone(),
            alive.clone(),
        ));
        let server = Arc::new(McpServer {
            command: request.command.clone(),
            env: request.env.clone(),
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            alive,
            child: Mutex::new(child),
        });
        server.initialize().await?;

        Ok(server)
    }

    async fn initialize(&self) -> anyhow::Result<()> {
        let hello = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "sandbox-server", "version": env!("CARGO_PKG_VERSION") },
            },
        });
        let response = self
            .forward(hello, HANDSHAKE_TIMEOUT)
            .await?
            .ok_or_else(|| anyhow!("initialize got no response"))?;
        if let Some(error) = response.get("error") {
            bail!("initialize failed: {error}");
        }
        self.forward(
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            HANDSHAKE_TIMEOUT,
        )
        .await?;

        Ok(())
    }

    /// Send one message under a host-assigned id, so concurrent callers can never
    /// collide, then hand the caller back its own id on the response. One deadline
    /// covers the write and the wait, so a server that stops reading cannot hang it.
    async fn forward(&self, mut message: Value, wait: Duration) -> anyhow::Result<Option<Value>> {
        let original_id = message.get("id").cloned();
        let waiter = original_id.as_ref().map(|_| {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            message["id"] = json!(id);
            let (sender, receiver) = oneshot::channel();
            self.pending
                .lock()
                .expect("pending lock")
                .insert(id, sender);
            (
                PendingGuard {
                    pending: self.pending.clone(),
                    id,
                },
                receiver,
            )
        });
        let exchange = async {
            write_line(&self.stdin, &message).await?;
            let Some((_guard, receiver)) = waiter else {
                return Ok(None);
            };
            match receiver.await {
                Ok(response) => Ok(Some(response)),
                Err(_) => bail!("MCP server exited before answering"),
            }
        };
        let mut response = match timeout(wait, exchange).await {
            Ok(result) => result?,
            Err(_) => bail!("MCP server did not answer within {} ms", wait.as_millis()),
        };
        if let Some(response) = response.as_mut() {
            response["id"] = original_id.unwrap_or(Value::Null);
        }

        Ok(response)
    }
}

/// Drops a call's pending entry however the call ends: answered, failed, timed
/// out, or cancelled by a dropped request.
struct PendingGuard {
    pending: Pending,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

/// Route each response line to the caller waiting on its id. A server's own
/// request is answered here: `ping` with an empty result, anything else with
/// method-not-found, since the host offers no client features. On EOF the server
/// is marked dead and every waiter is released, so the next call starts it again.
async fn read_messages(
    stdout: tokio::process::ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    alive: Arc<AtomicBool>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        match (message.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => {
                let reply = if method == "ping" {
                    json!({ "jsonrpc": "2.0", "id": id, "result": {} })
                } else {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "method not found" } })
                };
                let _ = write_line(&stdin, &reply).await;
            }
            (None, Some(id)) => {
                let waiter = id
                    .as_u64()
                    .and_then(|id| pending.lock().ok().and_then(|mut p| p.remove(&id)));
                if let Some(sender) = waiter {
                    let _ = sender.send(message);
                }
            }
            _ => {}
        }
    }
    alive.store(false, Ordering::SeqCst);
    if let Ok(mut pending) = pending.lock() {
        pending.clear();
    }
}

async fn write_line(stdin: &Mutex<ChildStdin>, message: &Value) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(&line)
        .await
        .context("write to MCP server")?;
    stdin.flush().await.context("write to MCP server")?;

    Ok(())
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    // A stdio MCP server in a few lines of Python: answers initialize, pings the
    // host before each tool reply, counts tools/call so a reused process is
    // visible, and ignores notifications.
    const FAKE_SERVER: &str = r#"
import json, sys
calls = 0
for line in sys.stdin:
    msg = json.loads(line)
    if "method" not in msg or "id" not in msg:
        continue
    if msg["method"] == "initialize":
        result = {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {}, "serverInfo": {"name": "fake", "version": "1"}}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": "ping-1", "method": "ping"}), flush=True)
        pong = json.loads(sys.stdin.readline())
        calls += 1
        result = {"content": [{"type": "text", "text": f"call {calls} pong {pong.get('result') == {}}"}]}
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
"#;

    fn call(server: &str, id: Value) -> McpRequest {
        McpRequest {
            server: server.to_string(),
            command: vec!["python3".into(), "-c".into(), FAKE_SERVER.into()],
            env: HashMap::new(),
            message: json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {} }),
            timeout_ms: 10_000,
        }
    }

    #[tokio::test]
    async fn keeps_one_process_per_server_answers_pings_and_restores_the_caller_id() {
        let host = McpHost::default();
        let first = host
            .handle(call("fake", json!("a")))
            .await
            .unwrap()
            .unwrap();
        let second = host.handle(call("fake", json!(7))).await.unwrap().unwrap();

        assert_eq!(first["id"], json!("a"));
        assert_eq!(
            first["result"]["content"][0]["text"],
            json!("call 1 pong True")
        );
        assert_eq!(second["id"], json!(7));
        assert_eq!(
            second["result"]["content"][0]["text"],
            json!("call 2 pong True")
        );
    }

    #[tokio::test]
    async fn restarts_a_server_whose_environment_changed() {
        let host = McpHost::default();
        host.handle(call("fake", json!(1))).await.unwrap();
        let mut changed = call("fake", json!(2));
        changed.env.insert("MODE".into(), "other".into());
        let response = host.handle(changed).await.unwrap().unwrap();

        assert_eq!(
            response["result"]["content"][0]["text"],
            json!("call 1 pong True")
        );
    }

    #[tokio::test]
    async fn a_notification_gets_no_response() {
        let host = McpHost::default();
        let mut request = call("fake", json!(1));
        request.message = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled" });

        assert!(host.handle(request).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn refuses_a_request_without_a_command() {
        let host = McpHost::default();
        let mut request = call("fake", json!(1));
        request.command = vec![];

        assert!(host.handle(request).await.is_err());
    }
}
