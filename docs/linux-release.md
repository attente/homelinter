# Building and running a Linux release

`homelinter` ships on Linux as a single AppImage.
It is produced by GitHub Actions, so no Linux packaging toolchain is needed locally.

## What is in the bundle

The AppImage is not a Tauri build.
It contains `homelinter-app`, a small launcher in `crates/launcher` that runs the same `homelinter-service` in-process, waits for it to answer `GET /api/v1/health`, and then opens the SPA in an app-mode window of the browser that is already installed.
The bundle therefore carries no WebKitGTK, GTK, or GStreamer, and its only library dependencies are glibc and libgcc.

That is why the AppImage is small, and why it behaves on distributions that do not ship an FHS layout, NixOS in particular.
The price is that a browser must exist: the launcher prefers the Chromium family, where `--app=<url>` gives a real app window with no location bar, bookmarks, back/forward buttons, or tabs, and falls back to Firefox in kiosk mode.

## 1. Bump the version

The version lives in the root `Cargo.toml` under `[workspace.package]`.
The tag only names the artifact, so keep the two in step by hand:

```sh
# edit Cargo.toml
cargo metadata --format-version 1 > /dev/null   # refresh Cargo.lock
git add Cargo.toml Cargo.lock
git commit -m "release: 0.2.0"
git tag v0.2.0
git push origin main
git push origin v0.2.0
```

The `v*` tag starts `.github/workflows/linux-release.yml`.

You can also run the workflow manually from the Actions tab with Run workflow.
A manual run uploads the AppImage as a workflow artifact but does not create a GitHub release, because the ref is not a tag.

## 2. What the workflow produces

For a tag it publishes two files on the GitHub release:

- `homelinter-<version>-x86_64.AppImage`, the application.
- `INSTALL-Linux.txt`, the short instructions a recipient needs.

The workflow runs on `ubuntu-22.04`, so the AppImage needs glibc 2.35 or newer: Ubuntu 22.04+, Debian 12+, Fedora 36+, or any current rolling distribution.
It builds the SPA with pnpm, builds `homelinter-app` with the pinned Rust 1.85.0 toolchain, assembles the AppDir, and runs `appimagetool` 1.9.1, pinned by SHA-256.
Before uploading, a smoke test runs the finished AppImage with `HOMELINTER_BROWSER=true`, which checks the bundled SPA lookup, the in-process service, the health check, and the shutdown path without needing a display.

## 3. Running it

```sh
chmod +x homelinter-0.2.0-x86_64.AppImage
./homelinter-0.2.0-x86_64.AppImage
```

The launcher searches the `PATH` for, in order, `chromium`, `chromium-browser`, `google-chrome-stable`, `google-chrome`, `brave-browser`, `microsoft-edge-stable`, `vivaldi-stable`, `firefox`, and `firefox-esr`.
Chromium-based browsers get `--app=<url>` plus the reserved profile, window size, no-first-run, and no-default-browser-check, so no tabs, bookmarks, or location bar appear.
Firefox gets `--kiosk <url>` with the same reserved profile.

The app keeps its own browser profile in `~/.local/share/homelinter/browser` (`$XDG_DATA_HOME/homelinter/browser`), next to the service's cache and audit data.
It is not your everyday browsing profile, so the app window starts clean and keeps no session shared with your own browsing, and it is not thrown away between runs, so the browser does not set itself up from scratch on every launch.
Set `HOMELINTER_BROWSER_DATA_DIR` to put it elsewhere, and delete that directory to reset the app's browser state (site permissions, window placement) to a first launch.

Because there is exactly one profile, only one homelinter window runs at a time; a second start reports `homelinter is already running` and exits with status 1 instead of hijacking the open window.
The guard is an `flock` on `~/.local/share/homelinter/browser.lock`, released by the kernel when the app exits, so a crash never blocks the next start.

Closing the window stops the service and exits the launcher; Ctrl-C in the foreground or a `SIGTERM` does the same and also stops the browser tree.

### Running it detached

To keep the app running after the terminal closes, start it in its own session with `setsid`:

```sh
setsid ./homelinter-0.2.0-x86_64.AppImage >~/homelinter.log 2>&1 </dev/null &
```

`setsid` starts the bundle in a new session with no controlling terminal, so closing the terminal or losing the SSH session delivers no hangup at all, and the launcher ignores `SIGHUP` on top of that.
The command returns at once and the window opens a moment later.
The log names the URL it opened:

```sh
grep -o 'http://127.0.0.1:[0-9]*' ~/homelinter.log | tail -n 1
```

Confirm the launcher is still up after the terminal is gone:

```sh
pgrep -af usr/bin/homelinter-app
```

A detached launch stops the same ways as a foreground one: close the app window, or send the launcher `SIGTERM`, which closes the browser tree and the service with it:

```sh
pkill -TERM -x homelinter-app
```

Do not `SIGKILL` the launcher or kill the AppImage wrapper process: the browser runs in its own process group, so it would survive as an orphan window pointed at a stopped service.

On NixOS the AppImage runs through `appimage-run`, whose bubblewrap supervisor exits on `SIGHUP` before the launcher handles it.
`nohup ... &` is not enough there: `nohup` only ignores `SIGHUP` in the process it starts, while the shell resends `SIGHUP` to the whole job process group on hangup, and the supervisor is in that group.
Use `setsid ... &` on NixOS as well.

To pick a browser, or to reach one behind a wrapper:

```sh
HOMELINTER_BROWSER=firefox ./homelinter-0.2.0-x86_64.AppImage
HOMELINTER_BROWSER="flatpak run org.chromium.Chromium" ./homelinter-0.2.0-x86_64.AppImage
```

`HOMELINTER_BROWSER` takes a command line, so both a bare program name (looked up on the `PATH`) and an absolute path or multi-argument command work.

## 4. FUSE

A normal AppImage mounts itself through FUSE 2.
Where that is unavailable, extract and run instead:

```sh
APPIMAGE_EXTRACT_AND_RUN=1 ./homelinter-0.2.0-x86_64.AppImage
```

## 5. NixOS

With AppImage support enabled in the system configuration, the file runs directly:

```nix
programs.appimage = {
  enable = true;
  binfmt = true;
};
```

`binfmt` registers the AppImage runtime with the kernel, so the executable bit and a plain `./homelinter-*.AppImage` are enough.
Without that option:

```sh
nix-shell -p appimage-run --run "appimage-run ./homelinter-0.2.0-x86_64.AppImage"
```

The browser still comes from the user profile, so install `chromium` or `firefox` there and put it on the `PATH`; in a systemd service or a bare `nix shell` the `PATH` may not include it, and `HOMELINTER_BROWSER=/run/current-system/sw/bin/chromium` removes the guesswork.

The AppImage bundles no GTK or WebKit, so the usual AppImage failure on NixOS (missing host GIO modules or WebKitGTK) does not apply here.

## 6. Building the AppImage locally

The build script needs an already built SPA and launcher:

```sh
cd webui && pnpm install --frozen-lockfile && pnpm build && cd ..
cargo build --release -p homelinter-app
APPIMAGETOOL=appimagetool ./packaging/appimage/build.sh
```

`build.sh` reads the version from the root `Cargo.toml` and writes `target/appimage/homelinter-<version>-$(uname -m).AppImage`.
It honours these environment variables: `BIN`, `DIST`, `OUT`, `APPIMAGETOOL`, `VERSION`, and `ARCH`.
By default it sets `APPIMAGE_EXTRACT_AND_RUN=1` for `appimagetool`, so a downloaded `appimagetool` works without FUSE.

The AppDir layout is fixed and the launcher depends on it:

```
AppRun
homelinter.png
app.homelinter.desktop
usr/bin/homelinter-app
usr/share/homelinter/webui/           the built SPA
usr/share/applications/app.homelinter.desktop
usr/share/icons/hicolor/512x512/apps/homelinter.png
usr/share/metainfo/app.homelinter.desktop.appdata.xml
```

## 7. Requirements and limits

- A Chromium-based browser is preferred. Firefox works but opens a kiosk window, which has no tab strip and no address bar but is harder to leave with the keyboard.
- Under Wayland the compositor may name the window after the URL (for example `chrome-127.0.0.1__-Default`), because Chromium derives the Wayland app id itself; `--class` only sets X11's `WM_CLASS`. A window rule is the way to rename or decorate it.
- Starting the window is one launch of the browser; the launcher waits for it to exit before stopping the service.
- The AppImage is x86_64 only. An aarch64 build needs an arm runner plus an aarch64 `appimagetool`; it is not wired up yet.
- System integration is left to the desktop. The bundle carries a desktop entry, a 512x512 icon, and AppStream metadata for tools such as `appimaged` or AppImageLauncher, but it does not install anything by itself.
- A native Nix package is not implemented yet; the AppImage is the supported Linux artifact.
