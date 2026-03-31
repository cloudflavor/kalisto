# Repository Guidelines

## Project Structure & Module Organization

Kalisto is a Rust CLI and library. `src/main.rs` is the CLI entrypoint, and `src/lib.rs` exposes the shared modules. Keep backend-neutral orchestration in `src/cluster/`, backend implementations in `src/backend/`, config loading in `src/config/`, and platform probes in `src/probe/`. Lima-specific `k3s` helpers live in `src/lima_k3s/`.

Integration tests live in `tests/`. Artifact builders and release helpers live in `scripts/macos/`, `scripts/linux/`, and `scripts/image/`. Runtime and example artifacts are stored under `artifacts/`; longer-form notes and prerequisites live in `docs/` and `notes/`.

## Build, Test, and Development Commands

- `cargo build` — compile the library and CLI.
- `cargo run -- up` — start the default local cluster.
- `cargo run -- status --name kalisto` — inspect a cluster state.
- `cargo run -- bench --runs 3` — run lifecycle benchmarks.
- `cargo test` — run the default Rust test suite.
- `cargo fmt` and `cargo clippy --all-targets --all-features` — format and lint before opening a PR.
- Use the Opal MCP tools for pipeline work: `mcp__opal__opal_plan` to inspect jobs, `mcp__opal__opal_run` to execute the local pipeline flow, and `mcp__opal__opal_view` to inspect results and logs. Do not invoke `opal` through the shell for normal pipeline runs.

Live infrastructure tests are opt-in and require host prerequisites:

- `KALISTO_LIVE_LIMA_TESTS=1 cargo test --test macos_lima_live -- --nocapture`
- `KALISTO_LIVE_LIMA_K3S_TESTS=1 cargo test --test macos_lima_k3s_live -- --nocapture`

## Coding Style & Naming Conventions

Use standard Rust formatting with 4-space indentation and keep changes `rustfmt`-clean. Prefer small modules over large files. Use `snake_case` for files, modules, and functions; `PascalCase` for types and enums; `SCREAMING_SNAKE_CASE` for constants and environment variables. Match existing patterns: explicit structs for config/state, focused helper functions, and `Result`-based error flow instead of panics.

## Testing Guidelines

Place new integration coverage in `tests/` and name files after the behavior under test, for example `firecracker_live.rs`. Keep default tests host-safe; gate real Lima or `k3s` flows behind environment variables. When changing CLI, backend, or artifact resolution logic, run the narrowest relevant test first, then broader validation.

## Commit & Pull Request Guidelines

Follow the repository’s existing commit style: concise, imperative subjects with optional scopes such as `feat(cli): ...`, `fix(status): ...`, or `build(firecracker): ...`. Keep commits focused on one change.

PRs should describe the user-visible effect, note the platform/backend touched (`lima`, `firecracker`, `k3s`), list commands run, and include logs or screenshots when output changes materially. Link related issues or follow-up work when applicable.
