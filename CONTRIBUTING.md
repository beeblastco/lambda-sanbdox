# Contributing

Thanks for your interest in contributing to **Lambda Agent Sandbox**! This project is a Rust-based AWS Lambda custom runtime for running sandboxed code execution. We welcome contributions of all kinds — bug fixes, features, documentation, and tests.

---

## Getting Started

1. **Fork the repository** on GitHub.
2. **Clone your fork:**

   ```bash
   git clone https://github.com/beeblastco/lambda-sanbdox.git
   cd lambda-sanbdox
   ```

3. **Ensure you have the MSRV installed** (see `rust-version` in `Cargo.toml` — currently `1.96.0`).
4. **Create a feature branch:**

   ```bash
   git checkout -b feat/my-change
   ```

---

## Development Workflow

### Code quality checks

Before submitting, run all quality checks locally:

```bash
# Formatting
cargo fmt -- --check

# Linting
cargo clippy -- -D warnings

# Unit tests
cargo test
```

### Docker build (optional, for integration testing)

```bash
docker build -t lambda-agent-sandbox .
```

See the [README](./README.md) for instructions on running the Lambda RIE emulator locally.

---

## Pull Request Guidelines

- **Keep PRs focused** — one feature or fix per PR.
- **Write a clear description** explaining the motivation and approach.
- **Update documentation** if you change behaviour (README, doc comments, etc.).
- **Add tests** for new functionality where possible.
- **Ensure CI passes** — all lint, test, and Docker build jobs must be green.
- **Target the `main` branch.**

### PR title conventions

We loosely follow [Conventional Commits](https://www.conventionalcommits.org/):

| Prefix     | Description                              |
|------------|------------------------------------------|
| `feat:`    | A new feature                            |
| `fix:`     | A bug fix                                |
| `chore:`   | Maintenance, tooling, dependency bumps   |
| `docs:`    | Documentation changes                    |
| `ci:`      | CI/CD pipeline changes                   |
| `refactor:`| Code restructuring without changes       |
| `test:`    | Adding or updating tests                 |

---

## Code Style

- Run `cargo fmt` before committing — the CI pipeline enforces formatting.
- Follow `clippy` suggestions at the `-D warnings` level.
- Keep error handling idiomatic — prefer `anyhow` for application-level errors and use `thiserror` for library-style errors.
- Document public functions and types with doc comments.

---

## How to Report Issues

- **Bug reports:** Include the runtime, code snippet, timeout, and full response (stdout/stderr/exit code).
- **Feature requests:** Describe the use case and any prior art or alternatives considered.

---

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](./LICENSE).
