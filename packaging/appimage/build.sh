#!/usr/bin/env bash
# Assemble the homelinter AppImage.
#
# Inputs (override with environment variables):
#   BIN           release launcher binary   (target/release/homelinter-app)
#   DIST          built SPA                 (webui/dist)
#   OUT           output directory          (target/appimage)
#   APPIMAGETOOL  appimagetool executable   ($PATH)
#   VERSION       bundle version            ([workspace.package] version)
#   ARCH          AppImage architecture     (uname -m)
#
# The AppImage carries no GUI toolkit: the launcher starts the service
# in-process and opens the SPA in an app-mode window of the browser the
# user already has. That keeps the bundle small and avoids bundling
# WebKitGTK, GStreamer, and GIO modules, which is what usually breaks
# AppImages on NixOS and other non-FHS systems.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
bin="${BIN:-$root/target/release/homelinter-app}"
dist="${DIST:-$root/webui/dist}"
out_dir="${OUT:-$root/target/appimage}"
appimagetool="${APPIMAGETOOL:-appimagetool}"
arch="${ARCH:-$(uname -m)}"
version="${VERSION:-$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)}"

if [ -z "$version" ]; then
  echo "cannot read the workspace version from $root/Cargo.toml" >&2
  exit 1
fi
# Build the launcher unless an explicit binary was handed in, so a bundle can
# never carry a stale `target/release/homelinter-app` left over from an earlier
# build (that silently shipped an old service in an otherwise fresh bundle).
if [ -z "${BIN:-}" ]; then
  ( cd "$root" && cargo build --release -p homelinter-app )
fi
if [ ! -x "$bin" ]; then
  echo "missing release binary: $bin" >&2
  echo "build it first: cargo build --release -p homelinter-app" >&2
  exit 1
fi
if [ ! -f "$dist/index.html" ]; then
  echo "missing built SPA: $dist/index.html" >&2
  echo "build it first: (cd webui && pnpm install --frozen-lockfile && pnpm build)" >&2
  exit 1
fi

appdir="$out_dir/AppDir"
rm -rf "$appdir"
install -d "$appdir/usr/bin" \
  "$appdir/usr/share/homelinter" \
  "$appdir/usr/share/applications" \
  "$appdir/usr/share/icons/hicolor/512x512/apps" \
  "$appdir/usr/share/metainfo"

install -m 755 "$bin" "$appdir/usr/bin/homelinter-app"
cp -r "$dist" "$appdir/usr/share/homelinter/webui"

icon="$root/crates/desktop/src-tauri/icons/icon.png"
install -m 644 "$icon" "$appdir/homelinter.png"
install -m 644 "$icon" "$appdir/usr/share/icons/hicolor/512x512/apps/homelinter.png"
install -m 755 "$root/packaging/appimage/AppRun" "$appdir/AppRun"
install -m 644 "$root/packaging/appimage/app.homelinter.desktop" \
  "$appdir/app.homelinter.desktop"
install -m 644 "$root/packaging/appimage/app.homelinter.desktop" \
  "$appdir/usr/share/applications/app.homelinter.desktop"
install -m 644 "$root/packaging/appimage/app.homelinter.desktop.appdata.xml" \
  "$appdir/usr/share/metainfo/app.homelinter.desktop.appdata.xml"

out="$out_dir/homelinter-$version-$arch.AppImage"
# appimagetool is itself an AppImage; extract-and-run keeps the build
# working on machines (and CI runners) without FUSE.
export APPIMAGE_EXTRACT_AND_RUN="${APPIMAGE_EXTRACT_AND_RUN:-1}"
ARCH="$arch" "$appimagetool" --comp zstd --no-appstream "$appdir" "$out"
chmod +x "$out"
echo "built $out"
