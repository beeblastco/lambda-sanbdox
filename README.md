# AWS Lambda MicroVM Agent Sandbox

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.96.0-orange?logo=rust)](https://www.rust-lang.org)
[![Architecture](https://img.shields.io/badge/architecture-arm64-blue?logo=docker)](https://www.docker.com)

A Rust **HTTP server** packaged specifically for [AWS Lambda MicroVMs](https://docs.aws.amazon.com/lambda/latest/dg/microvms-how-it-works.html). It executes arbitrary code inside a Firecracker-isolated MicroVM with `bash`, `python`, and `node`, timeout enforcement, output capture, and an S3-backed persistent workspace.

This repository targets **AWS Lambda MicroVMs only**. It is not an AWS Lambda function custom runtime, not a Lambda Runtime API/RIE project, and not a general-purpose multi-architecture container image. Local Docker builds exist only to reproduce and smoke-test the same arm64 container that Lambda MicroVMs build, snapshot, and run.

> **Migration note (v0.2):** this used to be a Lambda *custom runtime* invoked per request (`bootstrap` + the Runtime API / RIE). It now only supports the AWS Lambda MicroVM model: Lambda builds a MicroVM image from this source artifact, snapshots the running server, and resumes MicroVM instances from that image. The binary is `sandbox-server` and the wire protocol is HTTP.

---

## What It Does

A MicroVM boots from a snapshot of this image and runs `sandbox-server`, which listens on two ports:

- **`:8080` — exec API.** `POST /exec` takes a JSON request describing code to run, spawns it in an isolated workspace, captures stdout/stderr/exit code, and returns a structured JSON response. The MicroVM proxy routes external `443` here.
- **`:9000` — lifecycle hooks.** The endpoints AWS Lambda calls at MicroVM transitions (`/ready`, `/run`, `/resume`, `/suspend`, `/terminate`). `/run` mounts the workspace S3 prefix with [mountpoint-s3](https://github.com/awslabs/mountpoint-s3).

**Supported runtimes:** `bash`, `python` (python3), `node` (Node.js)

**Key features:**

- Isolated per-run workspace under `/tmp/agent-workspace/<uuid>/` (ephemeral) or `/mnt/workspaces/<namespace>/` (persistent, S3-backed)
- Configurable execution timeout (default 30s, 300s cap)
- stdout/stderr capture with truncation at 256 KB each
- Custom environment variables and command-line arguments
- Automatic ephemeral-workspace cleanup after each run
- `env_clear()` prevents AWS credential leakage into sandbox code
- Execs are serialized so the per-run `cpu_usec` accounting stays exact
- Input size limits: 10 MB code, 256 KB env, 64 args / 64 KB total

---

## Prerequisites

| Tool | Version | Purpose |
| ---- | ------- | ------- |
| [Rust](https://rustup.rs/) | 1.80+ | Build the `sandbox-server` binary |
| [Docker](https://docs.docker.com/get-docker/) | 24+ | Build and run the container locally |
| [AWS CLI](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html) | 2.x | Publish and run Lambda MicroVM images |

> **Note:** The Lambda MicroVM managed base image currently publishes `linux/arm64`, so Docker builds and CI target arm64. Docker Desktop and CI runners can run the image through emulation when the host is amd64.

---

## Local Development

### 1. Clone the repository

```bash
git clone <repo-url>
cd lambda-microvm-agent-sandbox
```

### 2. Build the Rust binary

```bash
cargo build --release --bin sandbox-server
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
docker build -t lambda-microvm-agent-sandbox .
```

The Dockerfile is a multi-stage build:

- **Builder stage:** `rust:1.96.0-bookworm` compiles the `sandbox-server` binary
- **Runtime stage:** `public.ecr.aws/lambda/microvms:al2023-minimal` (the MicroVM managed base) with bash, git, jq, python3, Node.js 22, `uv`, ripgrep, and `mount-s3`

---

## Run Locally (HTTP server)

The image runs the same `sandbox-server` locally as it does inside a MicroVM, so a plain `docker run` reproduces the runtime. A stateless exec needs no fuse/privileges; the S3 workspace mount (`/run` hook) does, so test that against a real MicroVM.

### Start the container

```bash
docker build -t lambda-microvm-agent-sandbox .
docker run -d --name sandbox -p 8080:8080 -p 9000:9000 lambda-microvm-agent-sandbox
sleep 3
```

### Send a test request

```bash
curl -s -X POST "http://localhost:8080/exec" \
  -H 'content-type: application/json' \
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

### Lifecycle hooks

```bash
# /ready returns 200 once the server is up
curl -i -X POST "http://localhost:9000/aws/lambda-microvms/runtime/v1/ready"
```

### Timeout test

```bash
curl -s -X POST "http://localhost:8080/exec" \
  -d '{"runtime": "bash", "code": "sleep 5", "timeout_ms": 1000}'
# => {"ok":false,"timed_out":true,"stderr":"execution timed out after 1000 ms", ...}
```

### Stop the container

```bash
docker rm -f sandbox
```

---

## Publish as a MicroVM image

A MicroVM image is **not** an ECR-image Lambda. AWS builds the image itself from a
zip containing the `Dockerfile` + sources, snapshots it, and versions it. CI does
this in [`.github/workflows/microvm-image.yml`](.github/workflows/microvm-image.yml);
the manual equivalent:

```bash
# 1. Package the code artifact (Dockerfile at the zip root + everything it COPYs)
zip -r artifact.zip Dockerfile Cargo.toml Cargo.lock src
aws s3 cp artifact.zip "s3://${ARTIFACT_BUCKET}/microvm-images/lambda-microvm-agent-sandbox/$(git rev-parse --short HEAD).zip"

# 2. Create (first time) the image, declaring the lifecycle hooks on port 9000
aws lambda-microvms create-microvm-image \
  --name lambda-microvm-agent-sandbox \
  --base-image-arn "$(aws lambda-microvms list-managed-microvm-images \
      --query 'reverse(sort_by(managedMicrovmImages,&imageArn))[0].imageArn' --output text)" \
  --build-role-arn "$MICROVM_BUILD_ROLE_ARN" \
  --code-artifact '{"uri":"s3://.../lambda-microvm-agent-sandbox/<sha>.zip"}' \
  --hooks '{"port":9000,"microvmImageHooks":{"ready":"ENABLED","readyTimeoutInSeconds":120},"microvmHooks":{"run":"ENABLED","runTimeoutInSeconds":30,"resume":"ENABLED","resumeTimeoutInSeconds":10,"suspend":"ENABLED","suspendTimeoutInSeconds":10,"terminate":"ENABLED","terminateTimeoutInSeconds":10}}'

# 3. Ship new code as a new version
aws lambda-microvms update-microvm-image \
  --image-identifier "arn:aws:lambda:<region>:<account>:microvm-image:lambda-microvm-agent-sandbox" \
  --base-image-arn "<base>" --build-role-arn "$MICROVM_BUILD_ROLE_ARN" \
  --code-artifact '{"uri":"s3://.../lambda-microvm-agent-sandbox/<sha>.zip"}' --hooks '<same as above>'
```

The build role, artifact bucket, and execution role must exist in the same region
as the MicroVM image. Downstream infrastructure should run MicroVMs from this image
via `MICROVM_IMAGE_IDENTIFIER`.

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

When `namespace` is **not** set, each exec request gets a fresh directory under `/tmp/agent-workspace/<uuid>/`. It is deleted automatically after the call. Good for stateless one-shot scripts.

```json
{
  "runtime": "bash",
  "code": "echo hello"
}
```

### Persistent (namespace)

When `namespace` is set, the MicroVM uses `{workspace_root}/{namespace}` as the working directory and **never cleans it up**. Files written in one call are still there in the next.

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

The namespace must match `fs-[a-f0-9]{40}`. In the MicroVM deployment path, the `/run` hook mounts the namespace-scoped S3 workspace with mountpoint-s3, so files persist in S3 across MicroVM suspend/resume and replacement.

`workspace_root` overrides where namespaced workspaces are rooted. The default is the `SANDBOX_WORKSPACE_MOUNT_PATH` environment variable (set by SST to `/mnt/workspaces`) or `/mnt/workspaces` if the variable is absent.

#### Write durability

After a persistent run finishes, the handler calls `sync(2)` to flush all dirty page-cache writes to the S3 mount before the MicroVM suspends or terminates. This makes files written by the script, including plain shell redirection like `echo data > out.txt`, durable across a later MicroVM.

The runtime script itself (`main.sh`/`main.py`/`main.js`) runs from the workspace — so `python`/`node` relative imports resolve against it as before — and is then removed after the run, so persistent workspaces never accumulate it.

---

## Security Notes

- **Workspace isolation:** Ephemeral runs get a fresh UUID-named directory under `/tmp/agent-workspace/`. It is deleted after execution. Persistent runs use a stable namespace path and are never cleaned up by the handler.
- **Credential isolation:** The child process environment is cleared (`env_clear()`) before setting explicit variables. MicroVM execution-role credentials are **not** leaked into sandbox code.
- **Input limits:** Code is capped at 10 MB, environment variables at 256 KB total, and arguments at 64 items / 64 KB total.
- **Timeout enforcement:** Uses `tokio::time::timeout` with `kill_on_drop` to terminate the child process if it exceeds the limit. Maximum configurable timeout is **300 seconds** (5 minutes).
- **Known limitation:** On timeout, `kill_on_drop` terminates the direct `bash` child but does not reliably kill grandchild processes spawned by the script. A production-hardened version should use process groups (`setpgid`) and kill the entire group.

---

## Shell Quoting with `curl`

When sending JSON with `curl -d`, your shell interprets the outer quotes **before** the JSON ever reaches the MicroVM HTTP server. A common mistake is nesting single quotes inside a single-quoted shell string:

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
    curl -s -X POST "http://localhost:8080/exec" \
      -d @/tmp/payload.json
    ```

2. Use a heredoc with `curl -d @-`

    ```bash
    curl -s -X POST "http://localhost:8080/exec" \
      -d @- << 'EOF'
    {"runtime":"bash","code":"echo 'hello from bash'","timeout_ms":30000}
    EOF
    ```

3. Use `jq` to build JSON safely

    ```bash
    jq -n '{runtime: "bash", code: "echo \'hello from bash\'", timeout_ms: 30000}' | \
      curl -s -X POST "http://localhost:8080/exec" -d @-
    ```

4. Escape single quotes inside double-quoted shell strings
    In bash, `'` inside `"..."` is literal, but `"` must be escaped with `\`:

    ```bash
    curl -s -X POST "http://localhost:8080/exec" \
      -d "{\"runtime\":\"bash\",\"code\":\"echo 'hello from bash'\",\"timeout_ms\":30000}"
    ```

> **Remember:** the MicroVM server runs bash behind the scenes, but the JSON must be valid **before** it gets there. The error `EOF while parsing a string` is a JSON deserialization failure in the HTTP request body, not a bash error.

---

## Troubleshooting

| Issue | Cause | Fix |
| ----- | ----- | --- |
| `curl: (7) Failed to connect` | Server not up yet | Wait a few seconds after `docker run`; check `docker logs sandbox` |
| Code returns `ok: false` with no output | Script error or non-zero exit | Check `stderr` and `exit_code` in the response |
| `uv` not found | `~/.local/bin` not on PATH | The Dockerfile moves `uv` to `/usr/local/bin/uv`; rebuild if missing |
| `502` from the MicroVM proxy | App not listening on 8080 yet (snapshot still warming) | The harness retries within its warm-up budget; if it persists, check `/ready` + that the server binds `0.0.0.0:8080` |
| `/run` returns 500 | `mount-s3` failed (bad creds, region, or bucket) | Check the MicroVM logs for the `mount-s3 failed` line; verify the scoped credentials and prefix in `runHookPayload` |
| MicroVM build `FAILED` | Dockerfile build or `/ready` hook failed | See the [troubleshooting page](https://docs.aws.amazon.com/lambda/latest/dg/microvms-troubleshooting.html); inspect `stateReason` via `get-microvm-image` |

---

## CI / CD

This repository uses **GitHub Actions** for continuous integration and delivery.

### On every Pull Request

- **Rust checks:** `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test`
- **Docker build:** Arm64 image (`linux/arm64`) with layer caching
- **Smoke test:** Starts the container and verifies `/ready` (200) + a stateless `POST /exec` returns `ok:true`

### On push to `main`

- All PR checks run first
- **MicroVM image publish** ([`microvm-image.yml`](.github/workflows/microvm-image.yml)): zips the Dockerfile + sources, uploads to the SST-provisioned artifact bucket, and creates/versions the MicroVM image (skipped until the required repo vars/secrets are set)
- The arm64 container image is also pushed to GHCR/ECR for local reproducibility

### Workflow files

See [`.github/workflows/ci.yml`](.github/workflows/ci.yml) and [`.github/workflows/microvm-image.yml`](.github/workflows/microvm-image.yml).

---

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](./CONTRIBUTING.md) for guidelines.

---

## License

Distributed under the [MIT License](./LICENSE). See [CHANGELOG.md](./CHANGELOG.md) for version history.
