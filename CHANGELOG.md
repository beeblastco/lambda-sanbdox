# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- The timeout cap is 600 s, matching the broods lambda provider. Anything over
  300 s used to fail.
- `/exec` accepts bodies up to 16 MB. axum's 2 MB default rejected requests well
  under the 10 MB code cap with a plain-text 413.

### Added

- Optional `browser` build target: the runtime image plus Playwright's Chrome
  Headless Shell (arm64) on PATH as `chromium`, for screenshots, DOM dumps and
  CDP automation. `docker build --target browser` builds it; the default target
  still builds the base image.

### Fixed

- A run that backgrounds a process (`cmd &`) now returns when its script exits.
  The background process inherited stdout and held the pipe open, so the run
  waited out its full timeout and came back empty. A background process that
  keeps writing to those pipes now gets SIGPIPE; redirect its output to keep it.
- A timeout now kills the run's whole process group, not just its direct child,
  and returns the output captured so far.
- The response carries `truncated: true` when stdout or stderr passed its 256 KB
  cap. The cut text ends in `...[truncated]`, which base64 callers decoded into
  silently corrupt files.
- Output is capped while it is read, so a runaway writer no longer grows the
  server's memory without bound.
- A persistent workspace mount no longer dies with `Input/output error` an hour
  after boot. mount-s3 was started with the session's static keys, so the
  refreshed session the harness posts to `/workspace/credentials` never reached
  it. It now reads credentials through a `credential_process` that prints a
  local file, which `/run` writes and each refresh rewrites. The unused
  `GET /workspace/credentials` endpoint is gone.
- Parallel executions against one workspace now use unique hidden runtime script
  names, preventing one MicroVM from removing another call's program.
- CI no longer attempts to build an unsupported `linux/amd64` image from the
  arm64-only `public.ecr.aws/lambda/microvms:al2023-minimal` base image.
- Repository documentation and local artifact names now identify the project as
  AWS Lambda MicroVM-only instead of a generic Lambda/custom-runtime sandbox.

## [0.2.0] - 2026-06-28

### Changed

- **BREAKING: run as an AWS Lambda MicroVM HTTP server instead of a Lambda custom
  runtime.** A MicroVM runs the container as a long-lived server, so the Lambda
  Invoke entrypoint (`bootstrap` + the Runtime API / RIE) is gone. The binary is
  now `sandbox-server`, which listens on:
  - **`:8080`** — the exec API. `POST /exec` takes the **same** request JSON and
    returns the **same** response JSON as the old Invoke handler; only the
    transport changed (broods #78). The MicroVM proxy routes external 443 here.
  - **`:9000`** — the lifecycle hooks (`/ready`, `/run`, `/resume`, `/suspend`,
    `/terminate`) under `/aws/lambda-microvms/runtime/v1`.
- Execs are now serialized with a mutex so the process-wide `cpu_usec` delta is
  never corrupted by overlapping children (the old RIE handled one request at a
  time implicitly).
- Base image switched to `public.ecr.aws/lambda/microvms:al2023-minimal`; the
  image now ships `mount-s3` + `fuse`, targets arm64, and `EXPOSE`s 8080/9000.

### Added

- **`/run` hook mounts the workspace from S3 with mountpoint-s3.** The harness
  delivers the namespace-scoped bucket prefix plus short-lived, prefix-scoped STS
  credentials in `runHookPayload`; the hook mounts it at `/mnt/workspaces/<namespace>`.
  This replaces the old S3 Files NFS mount (which was Lambda *function* config, not
  a MicroVM capability). Falls back to the execution role via IMDSv2 when no
  credentials are passed. `/terminate` unmounts to flush in-flight uploads.
- `microvm-image.yml` workflow: packages the Dockerfile + sources and creates /
  versions the MicroVM image (gated on the SST-provisioned build role + artifact
  bucket).

### Added (carried from the prior unreleased entry)

- Report `cpu_usec` in the response: CPU time (user + system, including
  descendants) charged to the sandboxed run, measured as a delta around the child
  off the cgroup v2 `cpu.stat` `usage_usec` counter (microsecond resolution, so
  sub-10ms commands — the common agent case — are counted), falling back to a
  `getrusage(RUSAGE_CHILDREN)` delta where the cgroup counter is unavailable. Lets
  the harness attribute real sandbox CPU per provider (filthy-panty #8). Omitted
  on validation errors and timeouts.

  > Note: an earlier iteration measured only via `getrusage(RUSAGE_CHILDREN)`,
  > whose child-time accounting is clock-tick granular and rounded short commands
  > down to zero — the cgroup `usage_usec` read fixes that under-count.

### Fixed

- Flush workspace writes with `sync(2)` after persistent runs so files created by
  the `bash` tool (e.g. shell redirection) survive a later MicroVM replacement
  instead of being lost in the page cache (filthy-panty #46). Stopgap only — the durable
  fix is a unified shared-data layer (Archil-style) covering durability and
  multi-agent conflict, tracked in filthy-panty #64.
- Remove the runtime script after the run so persistent workspaces no longer
  accumulate a leftover `main.sh`/`main.py`/`main.js` (filthy-panty #66). The
  script still executes from the workspace, preserving `python`/`node` relative
  import resolution.

## [0.1.0] - 2026-05-30

### Added

- Initial release of the Lambda Agent Sandbox custom runtime
- AWS Lambda custom runtime that executes arbitrary code in a sandboxed environment
- Support for `bash`, `python`, and `node` runtimes
- Isolated per-run workspace under `/tmp/agent-workspace/<uuid>/`
- Configurable execution timeout (default 30s)
- stdout/stderr capture with truncation at 256 KB each
- Custom environment variables and command-line arguments
- Automatic workspace cleanup after each run
- `env_clear()` prevents AWS credential leakage into sandbox code
- Input size limits: 10 MB code, 256 KB env, 64 args / 64 KB total
- Docker multi-stage build with ARM64/AMD64 support
- CI/CD pipeline with GitHub Actions (Rust checks, Docker build, smoke tests)
- Multi-arch Docker image pushed to GitHub Container Registry (GHCR)

### CI/CD

- Initial CI workflow with Rust formatting, linting, and unit tests
- Docker multi-arch build with layer caching
- Smoke tests via Lambda Runtime Interface Emulator (RIE)
- ECR push support with OIDC assume-role for AWS deployment
- Path filtering to skip Docker builds on unrelated changes
- Rust toolchain bumped to 1.96.0
