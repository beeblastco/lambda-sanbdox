# Lambda Agent Sandbox

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.96.0-orange?logo=rust)](https://www.rust-lang.org)
[![Architecture](https://img.shields.io/badge/architecture-arm64%20%7C%20amd64-blue?logo=docker)](https://www.docker.com)

A Rust-based AWS Lambda custom runtime that executes arbitrary code in a sandboxed environment. Supports `bash`, `python`, and `node` runtimes with timeout enforcement, output capture, and workspace isolation.

---

## What It Does

This project provides an AWS Lambda function that accepts JSON events describing code to run, spawns the code in an isolated temporary workspace, captures stdout/stderr/exit code, and returns a structured JSON response.

**Supported runtimes:** `bash`, `python` (python3), `node` (Node.js)

**Key features:**

- Isolated per-run workspace under `/tmp/agent-workspace/<uuid>/`
- Configurable execution timeout (default 30s)
- stdout/stderr capture with truncation at 256 KB each
- Custom environment variables and command-line arguments
- Automatic workspace cleanup after each run
- `env_clear()` prevents AWS credential leakage into sandbox code
- Input size limits: 10 MB code, 256 KB env, 64 args / 64 KB total

---

## Prerequisites

| Tool | Version | Purpose |
| ---- | ------- | ------- |
| [Rust](https://rustup.rs/) | 1.80+ | Build the bootstrap binary |
| [Docker](https://docs.docker.com/get-docker/) | 24+ | Build and run the container locally |
| [AWS CLI](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html) | 2.x | Deploy to Lambda (optional) |

> **Note:** The project builds for both `linux/amd64` and `linux/arm64`. Docker Desktop handles cross-arch builds automatically on macOS. The CI pipeline produces multi-arch images for both platforms.

---

## Local Development

### 1. Clone the repository

```bash
git clone <repo-url>
cd lambda-agent-sandbox
```

### 2. Build the Rust binary

```bash
cargo build --release --bin bootstrap
```

### 3. Run Rust quality checks

```bash
# Format check
cargo fmt -- --check

# Lint
cargo clippy -- -D warnings

# Unit tests
cargo test
```

---

## Build the Docker Image

```bash
docker build -t lambda-agent-sandbox .
```

The Dockerfile is a multi-stage build:

- **Builder stage:** `rust:1-bookworm` compiles the `bootstrap` binary
- **Runtime stage:** `public.ecr.aws/lambda/provided:al2023` with bash, git, jq, curl, python3, Node.js 22, and `uv`

---

## Run Locally (Lambda Runtime Interface Emulator)

The image includes the AWS Lambda Runtime Interface Emulator (RIE) so you can test locally without deploying to AWS.

### Start the container

```bash
docker run -d --name lambda-test -p 9000:8080 lambda-agent-sandbox
sleep 5
```

> **Important:** The RIE does not support concurrent requests. Send requests **one at a time** or the emulator will panic.

### Send a test request

```bash
curl -s -X POST "http://localhost:9000/2015-03-31/functions/function/invocations" \
  -d '{
    "runtime": "bash",
    "code": "echo hello from bash; node -v; python3 --version; curl -sI https://example.com | head -n 1",
    "timeout_ms": 30000
  }'
```

**Expected response:**

```json
{
  "ok": true,
  "runtime": "bash",
  "exit_code": 0,
  "timed_out": false,
  "duration_ms": 1234,
  "stdout": "hello from bash\nv22.x.x\nPython 3.x.x\nHTTP/2 200",
  "stderr": "",
  "workspace": "/tmp/agent-workspace/<uuid>",
  "cpu_usec": 84210
}
```

### Python example

```bash
cat > /tmp/py.json << 'EOF'
{
  "runtime": "python",
  "code": "import sys, os, subprocess\nprint(sys.version)\nprint(os.getcwd())\nprint(subprocess.check_output(['bash', '-lc', 'echo nested bash works']).decode())",
  "timeout_ms": 30000
}
EOF
curl -s -X POST "http://localhost:9000/2015-03-31/functions/function/invocations" \
  -d @/tmp/py.json
```

### Node.js example

```bash
cat > /tmp/node.json << 'EOF'
{
  "runtime": "node",
  "code": "const { execSync } = require('child_process'); console.log(process.version); console.log(execSync('python3 --version').toString());",
  "timeout_ms": 30000
}
EOF
curl -s -X POST "http://localhost:9000/2015-03-31/functions/function/invocations" \
  -d @/tmp/node.json
```

### Timeout test

```bash
curl -s -X POST "http://localhost:9000/2015-03-31/functions/function/invocations" \
  -d '{"runtime": "bash", "code": "sleep 5", "timeout_ms": 1000}'
```

**Expected response:**

```json
{
  "ok": false,
  "timed_out": true,
  "stderr": "execution timed out after 1000 ms",
  ...
}
```

### Stop the container

```bash
docker rm -f lambda-test
```

---

## Deploy to AWS Lambda

### 1. Push the image to Amazon ECR

```bash
# Create the ECR repository (once)
aws ecr create-repository --repository-name lambda-agent-sandbox --region us-east-1

# Get the login token
aws ecr get-login-password --region us-east-1 | \
  docker login --username AWS --password-stdin <account-id>.dkr.ecr.us-east-1.amazonaws.com

# Tag and push
docker tag lambda-agent-sandbox:latest \
  <account-id>.dkr.ecr.us-east-1.amazonaws.com/lambda-agent-sandbox:latest

docker push \
  <account-id>.dkr.ecr.us-east-1.amazonaws.com/lambda-agent-sandbox:latest
```

### 2. Create the Lambda function

```bash
aws lambda create-function \
  --function-name agent-sandbox \
  --package-type Image \
  --code ImageUri=<account-id>.dkr.ecr.us-east-1.amazonaws.com/lambda-agent-sandbox:latest \
  --role arn:aws:iam::<account-id>:role/lambda-agent-sandbox-role \
  --timeout 30 \
  --memory-size 512 \
  --architectures arm64
```

> **Note:** The Lambda execution role needs the basic Lambda execution policy. Create it in IAM if it doesn't exist.

### 3. Invoke the function

```bash
aws lambda invoke \
  --function-name agent-sandbox \
  --payload '{"runtime":"bash","code":"echo hello from lambda","timeout_ms":10000}' \
  response.json

cat response.json
```

### 4. Update an existing function

After rebuilding and pushing a new image:

```bash
aws lambda update-function-code \
  --function-name agent-sandbox \
  --image-uri <account-id>.dkr.ecr.us-east-1.amazonaws.com/lambda-agent-sandbox:latest
```

---

## API Reference

### Request format

| Field | Type | Required | Default | Description |
| ----- | ---- | -------- | ------- | ----------- |
| `runtime` | string | No | `bash` | One of: `bash`, `python`, `node` |
| `code` | string | **Yes** | — | The code to execute |
| `namespace` | string | No | — | Persistent workspace namespace (see below) |
| `workspace_root` | string | No | `SANDBOX_WORKSPACE_MOUNT_PATH` env var or `/mnt/workspaces` | Root directory for persistent workspaces |
| `timeout_ms` | integer | No | `30000` | Max execution time in milliseconds |
| `args` | string[] | No | `[]` | Command-line arguments passed to the script |
| `env` | object | No | `{}` | Custom environment variables (key → value) |

### Response format

| Field | Type | Description |
| ----- | ---- | ----------- |
| `ok` | boolean | `true` if the process exited with code 0 |
| `runtime` | string | The runtime that was used |
| `exit_code` | integer \| null | Process exit code, or `null` if timed out |
| `timed_out` | boolean | `true` if execution exceeded `timeout_ms` |
| `duration_ms` | integer | Wall-clock execution time in milliseconds |
| `stdout` | string | Captured stdout (truncated to 256 KB) |
| `stderr` | string | Captured stderr (truncated to 256 KB) |
| `workspace` | string | Path to the workspace directory used for this run |
| `cpu_usec` | integer | CPU time (user + system, incl. descendants) charged to the run, in microseconds. Omitted on validation errors and timeouts. |

---

## Workspace Modes

### Ephemeral (default)

When `namespace` is **not** set, each invocation gets a fresh directory under `/tmp/agent-workspace/<uuid>/`. It is deleted automatically after the call. Good for stateless one-shot scripts.

```json
{
  "runtime": "bash",
  "code": "echo hello"
}
```

### Persistent (namespace)

When `namespace` is set, the Lambda uses `{workspace_root}/{namespace}` as the working directory and **never cleans it up**. Files written in one call are still there in the next.

```json
{
  "runtime": "bash",
  "code": "echo hello > greeting.txt",
  "namespace": "fs-a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
}
```

Then in the next call:

```json
{
  "runtime": "bash",
  "code": "cat greeting.txt",
  "namespace": "fs-a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
}
```

```json
{ "ok": true, "stdout": "hello\n", ... }
```

The namespace must match `fs-[a-f0-9]{40}`. When this Lambda is deployed with an S3 Files filesystem mount (e.g. via SST with `fileSystemConfig`), the workspace directory maps directly to that mount, so files persist in S3 across cold starts and Lambda replacements.

`workspace_root` overrides where namespaced workspaces are rooted. The default is the `SANDBOX_WORKSPACE_MOUNT_PATH` environment variable (set by SST to `/mnt/workspaces`) or `/mnt/workspaces` if the variable is absent.

#### Write durability

After a persistent run finishes, the handler calls `sync(2)` to flush all dirty page-cache writes to the S3 Files mount before the Lambda freezes. This makes files written by the script — including plain shell redirection like `echo data > out.txt` — durable across a later cold container, not just the page cache of the current invocation.

The runtime script itself (`main.sh`/`main.py`/`main.js`) runs from the workspace — so `python`/`node` relative imports resolve against it as before — and is then removed after the run, so persistent workspaces never accumulate it.

---

## Security Notes

- **Workspace isolation:** Ephemeral runs get a fresh UUID-named directory under `/tmp/agent-workspace/`. It is deleted after execution. Persistent runs use a stable namespace path and are never cleaned up by the handler.
- **Credential isolation:** The child process environment is cleared (`env_clear()`) before setting explicit variables. AWS Lambda credentials are **not** leaked into sandbox code.
- **Input limits:** Code is capped at 10 MB, environment variables at 256 KB total, and arguments at 64 items / 64 KB total.
- **Timeout enforcement:** Uses `tokio::time::timeout` with `kill_on_drop` to terminate the child process if it exceeds the limit. Maximum configurable timeout is **300 seconds** (5 minutes).
- **Known limitation:** On timeout, `kill_on_drop` terminates the direct `bash` child but does not reliably kill grandchild processes spawned by the script. A production-hardened version should use process groups (`setpgid`) and kill the entire group.

---

## Shell Quoting with `curl`

When sending JSON with `curl -d`, your shell interprets the outer quotes **before** the JSON ever reaches the Lambda function. A common mistake is nesting single quotes inside a single-quoted shell string:

```bash
# ❌ BROKEN — bash terminates the string at the inner '
curl -d '{
  "code": "echo 'hello from bash'"
}'
# JSON actually sent: {"code": "echo   (unterminated string)
```

### Solutions

1. Use a JSON file (cleanest)

    ```bash
    cat > /tmp/payload.json << 'EOF'
    {
      "runtime": "bash",
      "code": "echo 'hello from bash'",
      "timeout_ms": 30000
    }
    EOF
    curl -s -X POST "http://localhost:9000/2015-03-31/functions/function/invocations" \
      -d @/tmp/payload.json
    ```

2. Use a heredoc with `curl -d @-`

    ```bash
    curl -s -X POST "http://localhost:9000/2015-03-31/functions/function/invocations" \
      -d @- << 'EOF'
    {"runtime":"bash","code":"echo 'hello from bash'","timeout_ms":30000}
    EOF
    ```

3. Use `jq` to build JSON safely

    ```bash
    jq -n '{runtime: "bash", code: "echo \'hello from bash\'", timeout_ms: 30000}' | \
      curl -s -X POST "http://localhost:9000/2015-03-31/functions/function/invocations" -d @-
    ```

4. Escape single quotes inside double-quoted shell strings
    In bash, `'` inside `"..."` is literal, but `"` must be escaped with `\`:

    ```bash
    curl -s -X POST "http://localhost:9000/2015-03-31/functions/function/invocations" \
      -d "{\"runtime\":\"bash\",\"code\":\"echo 'hello from bash'\",\"timeout_ms\":30000}"
    ```

> **Remember:** The Lambda function *is* running bash behind the scenes, but the JSON must be valid **before** it gets there. The error `EOF while parsing a string` is a JSON deserialization failure in the Lambda Runtime, not a bash error.

---

## Troubleshooting

| Issue | Cause | Fix |
| ----- | ----- | --- |
| `curl: (7) Failed to connect` | Container not running or RIE not ready | Wait 5 seconds after `docker run`; check `docker logs lambda-test` |
| RIE panic / crash | Concurrent requests sent to local emulator | Send requests **sequentially**, one at a time |
| `exec format error` | Wrong architecture RIE binary | The Dockerfile maps `aarch64` → `arm64`; should work on ARM64 |
| Code returns `ok: false` with no output | Script error or non-zero exit | Check `stderr` and `exit_code` in the response |
| `uv` not found | `~/.local/bin` not on PATH | The Dockerfile symlinks `uv` to `/usr/local/bin/uv`; rebuild if missing |
| `EOF while parsing a string` | Invalid JSON sent to RIE (usually bad shell quoting) | See **Shell Quoting with curl** above |

---

## CI / CD

This repository uses **GitHub Actions** for continuous integration and delivery.

### On every Pull Request

- **Rust checks:** `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test`
- **Docker build:** Multi-arch image (`linux/amd64`, `linux/arm64`) with layer caching
- **Security scan:** Trivy vulnerability scanner uploads SARIF results to the GitHub Security tab
- **Smoke test:** Spins up the container locally and verifies bash + node + python execution via RIE

### On push to `main`

- All PR checks run first
- The multi-arch image is pushed to **GitHub Container Registry (GHCR)**:
  - `ghcr.io/<owner>/lambda-agent-sandbox:latest`
  - `ghcr.io/<owner>/lambda-agent-sandbox:<sha>`

### Workflow file

See [`.github/workflows/ci.yml`](.github/workflows/ci.yml) for the full configuration.

---

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](./CONTRIBUTING.md) for guidelines.

---

## License

Distributed under the [MIT License](./LICENSE). See [CHANGELOG.md](./CHANGELOG.md) for version history.
