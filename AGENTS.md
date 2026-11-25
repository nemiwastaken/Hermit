# Repository Guidelines

## Project Structure & Module Organization

- `core/src/` contains the Rust daemon for Telegram, the LLM, SQLite-backed jobs, and tool routing. It starts in `main.rs`. Numbered SQL migrations live in `core/migrations/`, and the systemd unit lives in `core/systemd/`.
- `sandbox/` runs offline command and file tools. `net/` handles web and remote MCP tools. `browser/` wraps Obscura's MCP tools. `compose.yml` wires up these Python services.
- `install.sh` and `install/` handle deployment. `behavior.example/` provides starter profile, memory, and skills. The English documentation is in `README.md`, with a Russian translation in `translations/ru.md`.

## Build, Test, and Development Commands

Run commands from the repository root.

- `cargo build --locked --manifest-path core/Cargo.toml` builds the daemon with locked dependencies.
- `cargo test --locked --manifest-path core/Cargo.toml` runs the Rust tests.
- `docker build -t hermit-net net` builds the network image. Run its tests with `docker run --rm --entrypoint python3 -v "$PWD/net:/app:ro" hermit-net -B -m unittest -v`.
- `docker build -t hermit-browser browser` builds the Obscura image. Run browser tests with `docker run --rm --network none --entrypoint python3 -v "$PWD/browser:/app:ro" -w /app hermit-browser -B -m unittest -v`.
- For a local run, copy `.env.example` to `.env`, set the Telegram and LLM credentials, and apply `chmod 600 .env`. Run `mkdir -p .dev/run/{sandbox,net,browser} .dev/workspace` and create `.dev/mcp.json` containing `[]` if it is missing. Start the services with `docker compose up -d --build`, then launch the core with `cargo run --manifest-path core/Cargo.toml`.

## Coding Style & Testing

Use four spaces for Rust and Python, `snake_case` for functions and modules, and `PascalCase` for types and classes. Check Rust formatting with `cargo fmt --manifest-path core/Cargo.toml -- --check`. Name new migrations with the next numeric prefix and a descriptive suffix.

Keep Rust unit tests beside the implementation in `#[cfg(test)]` modules. Use descriptive Rust test names and `test_*` methods in Python's `net/test_server.py`. Follow existing fixtures that use temporary SQLite databases, mocked DNS, and loopback HTTP servers.

## Commits & Pull Requests

Use short, imperative commit subjects. History commonly uses component prefixes such as `core:`, `sandbox:`, and `docs:`. In pull requests, summarize the behavior change, list the checks you ran, and link related issues when available. Include screenshots for changes to Telegram presentation when they help reviewers.
