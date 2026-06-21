# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
  the `bash` tool (e.g. shell redirection) survive a later cold container instead
  of being lost in the page cache (filthy-panty #46). Stopgap only — the durable
  fix is a unified shared-data layer (Archil-style) covering durability and
  multi-agent conflict, tracked in filthy-panty #64.
- Remove the runtime script after the run so persistent workspaces no longer
  accumulate a leftover `main.sh`/`main.py`/`main.js` (filthy-panty #66). The
  script still executes from the workspace, preserving `python`/`node` relative
  import resolution.

## [0.1.0] - 2026-05-30

### Added

- Initial release of the Lambda Agent Sandbox
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
