//! Stdio MCP servers hosted inside the VM, behind `POST /mcp`.
//!
//! The harness names a server and the command that starts it, plus one JSON-RPC
//! message. The first message for a name spawns the command and runs the MCP
//! `initialize` handshake; later ones reuse the same process, so a stateful
//! server (a browser session, say) keeps its state for as long as the VM lives.
//! A server whose process exited, or whose command changed, is started again.
//! Exec already runs arbitrary code as root here, so starting a command is no
//! new privilege; the MicroVM stays the isolation boundary.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

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

/// Every stdio server this VM is running, by name.
#[derive(Default)]
pub struct McpHost {
    servers: Mutex<HashMap<String, Arc<McpServer>>>,
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
        let mut servers = self.servers.lock().await;
        if let Some(existing) = servers.get(&request.server) {
            if existing.alive.load(Ordering::SeqCst) && existing.command == request.command {
                return Ok(existing.clone());
            }
        }
        let server = McpServer::start(request).await?;
        // Replacing the entry drops the old process, which kill_on_drop then stops.
        servers.insert(request.server.clone(), server.clone());

        Ok(server)
    }
}

struct McpServer {
    command: Vec<String>,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    _child: Child,
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
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        tokio::spawn(read_responses(stdout, pending.clone(), alive.clone()));
        let server = Arc::new(McpServer {
            command: request.command.clone(),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            alive,
            _child: child,
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
    /// collide, then hand the caller back its own id on the response.
    async fn forward(&self, mut message: Value, wait: Duration) -> anyhow::Result<Option<Value>> {
        let original_id = message.get("id").cloned();
        let receiver = match original_id {
            None => None,
            Some(_) => {
                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                message["id"] = json!(id);
                let (sender, receiver) = oneshot::channel();
                self.pending.lock().await.insert(id, sender);
                Some((id, receiver))
            }
        };
        let mut line = serde_json::to_vec(&message)?;
        line.push(b'\n');
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(&line)
                .await
                .context("write to MCP server")?;
            stdin.flush().await.context("write to MCP server")?;
        }
        let Some((id, receiver)) = receiver else {
            return Ok(None);
        };
        let mut response = match timeout(wait, receiver).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => bail!("MCP server exited before answering"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("MCP server did not answer within {} ms", wait.as_millis());
            }
        };
        response["id"] = original_id.unwrap_or(Value::Null);

        Ok(Some(response))
    }
}

/// Route each response line to the caller waiting on its id. Server-initiated
/// requests and notifications have no waiter and are dropped. On EOF the server is
/// marked dead and every waiter is released, so the next call starts it again.
async fn read_responses(
    stdout: tokio::process::ChildStdout,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    alive: Arc<AtomicBool>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message.get("method").is_some() {
            continue;
        }
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            if let Some(sender) = pending.lock().await.remove(&id) {
                let _ = sender.send(message);
            }
        }
    }
    alive.store(false, Ordering::SeqCst);
    pending.lock().await.clear();
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    // A stdio MCP server in a few lines of Python: answers initialize, counts
    // tools/call so a reused process is visible, and ignores notifications.
    const FAKE_SERVER: &str = r#"
import json, sys
calls = 0
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    if msg["method"] == "initialize":
        result = {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {}, "serverInfo": {"name": "fake", "version": "1"}}
    else:
        calls += 1
        result = {"content": [{"type": "text", "text": f"call {calls}"}]}
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
    async fn keeps_one_process_per_server_and_restores_the_caller_id() {
        let host = McpHost::default();
        let first = host
            .handle(call("fake", json!("a")))
            .await
            .unwrap()
            .unwrap();
        let second = host.handle(call("fake", json!(7))).await.unwrap().unwrap();

        assert_eq!(first["id"], json!("a"));
        assert_eq!(first["result"]["content"][0]["text"], json!("call 1"));
        assert_eq!(second["id"], json!(7));
        assert_eq!(second["result"]["content"][0]["text"], json!("call 2"));
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
