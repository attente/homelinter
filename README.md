# homelinter

homelinter organizes home directories: it scans files, analyzes them, generates reorganization suggestions, and executes those suggestions automatically or interactively.

It never overwrites an existing object, never modifies file contents, and never executes a stale or colliding operation. Anything destructive requires explicit authorization; scanning, analysis, and planning are read-only.

Design details, the full configuration schema, and the service API are specified in `DESIGN.md`.

## What it does

homelinter classifies files (photos, documents, repositories, and so on), extracts metadata like dates and content hashes, and suggests `move` and `delete` operations with per-plan destinations and reasons. It works from a plain CLI, a full-screen TUI, and a web SPA, all clients of one shared service.

Duplicated files and broken symlinks are detected and reported for repair. Suggestions are validated as a batch before presentation and re-validated before execution, with collisions rejected rather than guessed at.

## How to run

From the repo root, `cargo run -p homelinter -- <subcommand>` runs the `homelinter` binary directly. Build with `cargo build --release`; the binary is `target/release/homelinter`.

- **Plain scan**: `cargo run -p homelinter` scans the default root (your home directory) and prints a summary pointing at the review and action entry points; nothing is executed or exported. Full-screen interactive review lives in `tui`.
- **Non-interactive**: `cargo run -p homelinter -- --execute` runs all suggestions immediately; `--export <path>` writes the plan to an executable script.
- **TUI**: `cargo run -p homelinter -- tui` starts the full-screen terminal interface (needs an 80x24 terminal).
- **Web**: `cargo run -p homelinter -- web` starts a localhost service on an ephemeral port and serves the SPA at the printed URL; pass `--port 8787` to pin the port (needed for the SPA dev loop). Session state is in memory, so a restart requires a new scan.

Per-scan options include `--config`, `--root`, `--hidden`, `--target-path kind=format`, and `--format pretty|json|toml`. `--execute`, `--export` are mutually exclusive.

### Developing the SPA

The SPA lives in `webui/`. For development, run `cargo run -p homelinter -- web --port 8787` in one terminal and `cd webui && pnpm install && pnpm dev` in another; Vite serves the app and proxies `/api` to the homelinter service on port 8787 (override the target with `HOMELINTER_WEB_PORT`).

For production-style serving, `pnpm build` in `webui/` produces `webui/dist`, which `homelinter web` serves alongside the API. The Rust workspace never builds the JS toolchain, so remember to `pnpm build` after frontend changes before serving.

## Configuration

Configuration is TOML, resolved by precedence: per-scan CLI options, then the config file, then values inferred from tools and your home layout, then defaults.

The default config path is `$HOME/.config/homelinter/config.toml` on Linux and macOS, and `%APPDATA%\homelinter\config.toml` on Windows; any file is merged over it (system-wide first, then user). Use `--config <path>` to select a specific file. The SPA shows the effective configuration read-only; the scan dialog overrides any scan-relevant option (destination paths, exclusions, providers, extraction commands, limits) for the next scan only, and durable settings are changed in the config file itself.

Key sections: `[target_paths]` maps each asset kind to a destination format string (for example `photo = "Pictures/%Y/%m/%f"`); `[geocoder]` and `[description]` configure optional content-description and location providers (all disabled by default); `[scan]` sets `exclude_paths` globs and (on Windows) the hidden-file definition; `[output]` selects pretty, JSON, or TOML reporting.

The scan root and hidden-file selection are per-scan settings, not config values; `scan.root` and `scan.hidden` in a config file are rejected. API keys and provider secrets are never written to logs or suggestions.