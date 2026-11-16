# Repository Guidelines

## Project Structure & Module Organization

Kalisto is a Rust CLI and library. `src/main.rs` is the CLI entrypoint, and `src/lib.rs` exposes the shared modules. Keep backend-neutral orchestration in `src/cluster/`, backend implementations in `src/backend/`, config loading in `src/config/`, and platform probes in `src/probe/`. Lima-specific `k3s` helpers live in `src/lima_k3s/`.

Integration tests live in `tests/`. Artifact builders and release helpers live in `scripts/macos/`, `scripts/linux/`, and `scripts/image/`. Runtime and example artifacts are stored under `artifacts/`; longer-form notes and prerequisites live in `docs/` and `notes/`.

## Portable installation contract

- After installing the Kalisto binary and documented host prerequisites, `kalisto pull` prepares required images and managed runtime tools, then `kalisto up` works outside this checkout.
- Do not require a source checkout, Docker, host sudo, repository scripts, manually installed Helm/Crane/Lima, custom templates, or machine-specific environment overrides for the standard macOS flow. Apple's `container` CLI and a running compatible service are prerequisites; Homebrew is a supported installation method.
- Keep paths relative to the current user's configuration/data directories. Never ship a contributor's absolute build paths as runtime defaults.
- Startup after preparation must create fresh VMs and reach the requested fully functional three-node profile within 60 seconds, including authenticated API, full selected Cilium rollouts, worker DNS, and cross-node Service HTTP traffic. No downloads during `up`.
- Preserve explicit CNI selection; do not silently enable Flannel to satisfy the budget.

## macOS Storage

Keep Rust compilation, node-image builds, build outputs, caches, and large build temporaries on a configured external build volume. Verify that volume is mounted, configure `CARGO_TARGET_DIR` and build-cache/temporary directories there, and preserve any checkout `target` and `artifacts` symlinks to that volume. Linux VM runtime disks may use the local SSD (`~/.lima`) for boot performance; moving build output onto the internal SSD is not an allowed optimization. Builds and validation remain CI-only unless explicitly authorized locally.

## Build, Test, and Development Commands

- `cargo build` — compile the library and CLI.
- `cargo run -- up` — start the default local cluster.
- `cargo run -- status --name kalisto` — inspect a cluster state.
- `cargo run -- bench --runs 3` — run lifecycle benchmarks.
- `cargo test` — run the default Rust test suite.
- `cargo fmt` and `cargo clippy --all-targets --all-features` — format and lint before opening a PR.
- Use the configured CI pipeline to run checks and inspect job results and logs. Do not run builds, tests, linters, formatters, or benchmarks locally unless explicitly authorized.

Live infrastructure tests are opt-in and require host prerequisites:

- `KALISTO_LIVE_LIMA_TESTS=1 cargo test --test macos_lima_live -- --nocapture`
- `KALISTO_LIVE_LIMA_K3S_TESTS=1 cargo test --test macos_lima_k3s_live -- --nocapture`
- `KALISTO_LIVE_THREE_NODE_BOOT_BUDGET=1 cargo test --release --test macos_three_node_boot_budget -- --nocapture` — actual cold CLI startup, three Ready nodes, authenticated host API, and DNS probes from both workers within 60 seconds.

## Coding Style & Naming Conventions

Use standard Rust formatting with 4-space indentation and keep changes `rustfmt`-clean. Prefer small modules over large files. Use `snake_case` for files, modules, and functions; `PascalCase` for types and enums; `SCREAMING_SNAKE_CASE` for constants and environment variables. Match existing patterns: explicit structs for config/state, focused helper functions, and `Result`-based error flow instead of panics.

## Testing Guidelines

Place new integration coverage in `tests/` and name files after the behavior under test, for example `firecracker_live.rs`. Keep default tests host-safe; gate real Lima or `k3s` flows behind environment variables. When changing CLI, backend, or artifact resolution logic, select the narrowest relevant CI test first, then broader CI validation.

## Commit & Pull Request Guidelines

Follow the repository’s existing commit style: concise, imperative subjects with optional scopes such as `feat(cli): ...`, `fix(status): ...`, or `build(firecracker): ...`. Keep commits focused on one change.

PRs should describe the user-visible effect, note the platform/backend touched (`lima`, `firecracker`, `k3s`), list commands run, and include logs or screenshots when output changes materially. Link related issues or follow-up work when applicable.

## Searching for files and directories

Use available repository search tools; do not assume a particular MCP service is installed.
