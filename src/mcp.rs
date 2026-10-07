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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration};

use crate::{kill_group, MAX_REQUEST_BYTES};

/// The MCP revision the host offers in `initialize`; the server answers with its own.
const PROTOCOL_VERSION: &str = "2025-06-18";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Distinct server names one VM keeps running; each is a live process.
const MAX_SERVERS: usize = 16;
/// Messages queued for one server's stdin before a sender waits. Each can be as
/// large as a request, so the queue stays short.
const OUTBOX_LINES: usize = 4;

/// One server name's running process, locked while it starts.
type Slot = Arc<Mutex<Option<Arc<McpServer>>>>;
/// A line for the server's stdin, with the host id of the call that waits on it.
type Outgoing = (Option<u64>, Vec<u8>);

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
        if request
            .message
            .get("method")
            .and_then(Value::as_str)
            .is_none()
        {
            bail!("message must be a JSON-RPC request or notification");
        }
        let server = self.server(&request).await?;
        let wait = Duration::from_millis(request.timeout_ms.clamp(1, MAX_TIMEOUT_MS));

        server.forward(request.message, wait).await
    }

    async fn server(&self, request: &McpRequest) -> anyhow::Result<Arc<McpServer>> {
        let slot = {
            let mut slots = self.slots.lock().await;
            if !slots.contains_key(&request.server) && slots.len() >= MAX_SERVERS {
                // Reclaim names whose server never started or has exited, unless a
                // caller still holds the slot.
                slots.retain(|_, slot| {
                    Arc::strong_count(slot) > 1
                        || slot.try_lock().map_or(true, |server| {
                            server.as_ref().is_some_and(|server| server.link.is_alive())
                        })
                });
                if slots.len() >= MAX_SERVERS {
                    bail!("this VM already runs {MAX_SERVERS} MCP servers");
                }
            }
            slots.entry(request.server.clone()).or_default().clone()
        };
        let mut slot = slot.lock().await;
        if let Some(existing) = slot.take() {
            if existing.link.is_alive()
                && existing.command == request.command
                && existing.env == request.env
            {
                *slot = Some(existing.clone());

                return Ok(existing);
            }
            // Stop the old process and its children before the replacement starts:
            // they may hold something the new one needs. The slot stays empty if the
            // replacement fails, so no caller reuses the stopped server.
            existing.link.stop();
        }
        let server = McpServer::start(request).await?;
        *slot = Some(server.clone());

        Ok(server)
    }
}

struct McpServer {
    command: Vec<String>,
    env: HashMap<String, String>,
    outbox: mpsc::Sender<Outgoing>,
    link: Arc<Link>,
    next_id: AtomicU64,
}

/// What a server's reader, writer and exit watcher share. Whichever of them sees
/// the server go first stops it for all.
struct Link {
    pending: SyncMutex<HashMap<u64, oneshot::Sender<Value>>>,
    alive: AtomicBool,
    pid: Option<u32>,
}

impl Link {
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn is_waiting(&self, id: u64) -> bool {
        self.pending
            .lock()
            .is_ok_and(|pending| pending.contains_key(&id))
    }

    /// Mark the server dead, release every waiter and kill its process group, so
    /// nothing it started outlives it. Only the first call does anything.
    fn stop(&self) {
        if !self.alive.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
        if let Some(pid) = self.pid {
            kill_group(pid);
        }
    }
}

impl McpServer {
    async fn start(request: &McpRequest) -> anyhow::Result<Arc<Self>> {
        let mut child = Command::new(&request.command[0])
            .args(&request.command[1..])
            .envs(&request.env)
            .current_dir("/tmp")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The server's logs go to the VM's own output, which reaches CloudWatch.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start MCP server {:?}", request.command))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let (outbox, lines) = mpsc::channel(OUTBOX_LINES);
        let link = Arc::new(Link {
            pending: SyncMutex::default(),
            alive: AtomicBool::new(true),
            pid: child.id(),
        });
        tokio::spawn(write_lines(stdin, lines, link.clone()));
        tokio::spawn(read_messages(stdout, outbox.clone(), link.clone()));
        // A process can exit while a child it left keeps stdout open, so its exit,
        // not EOF, is what marks it dead.
        let watched = link.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            watched.stop();
        });
        let server = Arc::new(McpServer {
            command: request.command.clone(),
            env: request.env.clone(),
            outbox,
            link,
            next_id: AtomicU64::new(1),
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
    /// covers the queueing and the wait, so a server that stops reading cannot hang it.
    async fn forward(&self, mut message: Value, wait: Duration) -> anyhow::Result<Option<Value>> {
        let original_id = message.get("id").cloned();
        let waiter = original_id.as_ref().map(|_| {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            message["id"] = json!(id);
            let (sender, receiver) = oneshot::channel();
            self.link
                .pending
                .lock()
                .expect("pending lock")
                .insert(id, sender);
            (
                PendingGuard {
                    link: self.link.clone(),
                    id,
                },
                receiver,
            )
        });
        let host_id = waiter.as_ref().map(|(guard, _)| guard.id);
        let line = encode(&message)?;
        let exchange = async {
            self.outbox
                .send((host_id, line))
                .await
                .map_err(|_| anyhow!("MCP server stopped reading its input"))?;
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
    link: Arc<Link>,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.link.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

/// Route each response line to the caller waiting on its id. A server's own
/// request is answered here: `ping` with an empty result, anything else with
/// method-not-found, since the host offers no client features. A reply waits for
/// room in the outbox on its own task, so it never stalls the reader. EOF stops
/// the server, so the next call starts it again, and so does a line longer than a
/// request may be, rather than buffering a runaway server's output without bound.
async fn read_messages(
    stdout: tokio::process::ChildStdout,
    outbox: mpsc::Sender<Outgoing>,
    link: Arc<Link>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        // One byte past the cap tells an oversized line from one that fits.
        let read = (&mut reader)
            .take(MAX_REQUEST_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .await;
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) if line.len() > MAX_REQUEST_BYTES => break,
            Ok(_) => {}
        }
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
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
                if let Ok(line) = encode(&reply) {
                    let outbox = outbox.clone();
                    tokio::spawn(async move { outbox.send((None, line)).await.ok() });
                }
            }
            (None, Some(id)) => {
                let waiter = id.as_u64().and_then(|id| {
                    link.pending
                        .lock()
                        .ok()
                        .and_then(|mut pending| pending.remove(&id))
                });
                if let Some(sender) = waiter {
                    let _ = sender.send(message);
                }
            }
            _ => {}
        }
    }
    link.stop();
}

/// Own the server's stdin and write each queued line whole, so a call that times
/// out never leaves half a line for the next message to be glued onto. A request
/// whose caller already gave up is skipped, so it never runs after its timeout. A
/// server that closed its input is stopped, so the next call starts it again.
async fn write_lines(mut stdin: ChildStdin, mut lines: mpsc::Receiver<Outgoing>, link: Arc<Link>) {
    while let Some((id, line)) = lines.recv().await {
        if id.is_some_and(|id| !link.is_waiting(id)) {
            continue;
        }
        if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
            link.stop();
            break;
        }
    }
}

fn encode(message: &Value) -> anyhow::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');

    Ok(line)
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

    // A server that stops reading for a second after initialize, then echoes every
    // line it can parse. A call that times out mid-write must not leave half a line
    // in its input for the next call to be glued onto.
    const SLOW_READER: &str = r#"
import json, sys, time
first = True
for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    if "id" not in msg:
        continue
    if msg.get("method") == "initialize":
        result = {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {}, "serverInfo": {"name": "slow", "version": "1"}}
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
        time.sleep(1)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": {"size": len(line)}}), flush=True)
"#;

    // Answers initialize, then leaves a child holding its stdout and exits.
    const EXITS_EARLY: &str = r#"
import json, subprocess, sys
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    if msg["method"] == "initialize":
        result = {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {}, "serverInfo": {"name": "early", "version": "1"}}
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
    else:
        subprocess.Popen(["sleep", "30"])
        sys.exit(0)
"#;

    #[tokio::test]
    async fn a_server_that_exits_releases_its_callers_even_if_a_child_keeps_its_stdout() {
        let host = McpHost::default();
        let mut request = call("early", json!(1));
        request.command = vec!["python3".into(), "-c".into(), EXITS_EARLY.into()];
        let started = std::time::Instant::now();

        let error = host.handle(request).await.unwrap_err();

        assert!(format!("{error:#}").contains("exited before answering"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn names_whose_server_never_started_do_not_count_against_the_cap() {
        let host = McpHost::default();
        for n in 0..MAX_SERVERS {
            let mut broken = call(&format!("broken-{n}"), json!(1));
            broken.command = vec!["/nonexistent/mcp-server".into()];
            assert!(host.handle(broken).await.is_err());
        }

        assert!(host.handle(call("fake", json!(1))).await.is_ok());
    }

    // Answers initialize, then answers every request with one line of argv[1] bytes.
    const HUGE_REPLY: &str = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    if msg["method"] == "initialize":
        result = {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {}, "serverInfo": {"name": "huge", "version": "1"}}
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
    else:
        sys.stdout.write("x" * int(sys.argv[1]) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn an_oversized_response_stops_the_server_instead_of_buffering_it() {
        let host = McpHost::default();
        let mut request = call("huge", json!(1));
        request.command = vec![
            "python3".into(),
            "-c".into(),
            HUGE_REPLY.into(),
            (MAX_REQUEST_BYTES + 1).to_string(),
        ];

        let error = host.handle(request).await.unwrap_err();

        assert!(format!("{error:#}").contains("exited before answering"));
    }

    #[tokio::test]
    async fn a_timed_out_write_does_not_corrupt_the_next_message() {
        let host = McpHost::default();
        let mut big = call("slow", json!(1));
        big.command = vec!["python3".into(), "-c".into(), SLOW_READER.into()];
        big.message["params"] = json!({ "pad": "x".repeat(1 << 20) });
        big.timeout_ms = 200;
        assert!(host.handle(big).await.is_err());

        let mut small = call("slow", json!(2));
        small.command = vec!["python3".into(), "-c".into(), SLOW_READER.into()];
        small.timeout_ms = 5_000;
        let response = host.handle(small).await.unwrap().unwrap();

        assert_eq!(response["id"], json!(2));
    }
}
