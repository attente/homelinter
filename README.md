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

## macOS app

`crates/desktop` is a Tauri 2 shell that packages the SPA and the service into one normal macOS `.app`.
It is a separate Cargo workspace (`crates/desktop/src-tauri`), so the normal Linux `cargo build` and `cargo test` never need WebKitGTK.
The shell starts the same in-process service on an ephemeral loopback port, loads `http://127.0.0.1:<port>/` in a WebView, and shuts the service down when the app exits; no child process is left behind.
The window is an ordinary resizable macOS window with a title bar, but no location bar, bookmarks, navigation buttons, tab strip, or extension UI.
The built SPA is bundled as `Contents/Resources/webui`, so a user only installs the app.

You do not need a physical Mac to produce the release.
`.github/workflows/macos-release.yml` builds on a GitHub-hosted macOS runner and creates a universal (Apple Silicon + Intel) app.
With no Apple secrets it produces an unsigned DMG that needs no Apple account; macOS asks the recipient to approve the app once, without disabling Gatekeeper globally.
If all six Apple secrets are set, the same workflow signs, notarizes, and staples the app so Gatekeeper opens it without any security bypass.
That optional signed route requires an Apple Developer Program membership and a `Developer ID Application` certificate.
Signed-route secrets: `APPLE_CERTIFICATE` (base64 `.p12`), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (app-specific password), and `APPLE_TEAM_ID`.
See [docs/macos-release.md](docs/macos-release.md) for the complete no-Mac checklist, including the unsigned approval steps and the optional certificate setup on Linux.

The app targets the hardened runtime but is not App Sandboxed: it must scan the home directory and move files wherever the user chooses, which is why it is distributed outside the Mac App Store.
Signed builds use the hardened runtime; an unsigned build has no signature for Gatekeeper to verify.
macOS will still show normal folder-access prompts (Desktop, Documents, Downloads), with purpose strings from `crates/desktop/src-tauri/Info.plist`.

To build the same artifacts manually on a Mac, run the Tauri CLI from `crates/desktop`; its `beforeBuildCommand` installs the SPA dependencies and rebuilds `webui/dist` first:

```
cd crates/desktop
pnpm dlx @tauri-apps/cli@2.11.1 build --target universal-apple-darwin
```

Without the Apple secrets this produces the unsigned build; with all six set it signs and notarizes.
The DMG is written to `crates/desktop/src-tauri/target/universal-apple-darwin/release/bundle/dmg/`.

## Configuration

Configuration is TOML, resolved by precedence: per-scan CLI options, then the config file, then values inferred from tools and your home layout, then defaults.

The default config path is `$HOME/.config/homelinter/config.toml` on Linux and macOS, and `%APPDATA%\homelinter\config.toml` on Windows; any file is merged over it (system-wide first, then user). Use `--config <path>` to select a specific file. The SPA shows the effective configuration read-only; the scan dialog overrides any scan-relevant option (destination paths, exclusions, providers, extraction commands, limits) for the next scan only, and durable settings are changed in the config file itself.

Key sections: `[target_paths]` maps each asset kind to a destination format string (for example `photo = "Photos/%Y-%m-%d-%H-%M-%S-%l-%s.%e"`); `[geocoder]` and `[description]` configure optional content-description and location providers (all disabled by default); `[scan]` sets `exclude_paths` globs and (on Windows) the hidden-file definition; `[output]` selects pretty, JSON, or TOML reporting.

The scan root and hidden-file selection are per-scan settings, not config values; `scan.root` and `scan.hidden` in a config file are rejected. API keys and provider secrets are never written to logs or suggestions.