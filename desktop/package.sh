#!/bin/sh
# Builds a self-contained "ESSM.app" into desktop/dist/.
#
# The bundle carries the release desktop binary plus a full OTP release
# of the Findex backend (its own ERTS and the native library), so the
# app runs on machines without Elixir, mix, or a source checkout. The
# desktop binary prefers Contents/Resources/backend when it exists.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
root="$here/.."

echo "==> backend OTP release"
(cd "$root/rust_client/backend" && MIX_ENV=prod mix release backend --overwrite)

echo "==> desktop release binary"
(cd "$here" && cargo build --release)

app="$here/dist/ESSM.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$here/target/release/essm" "$app/Contents/MacOS/essm"
cp -R "$root/rust_client/backend/_build/prod/rel/backend" \
      "$app/Contents/Resources/backend"
cp "$here/packaging/Info.plist" "$app/Contents/Info.plist"
cp "$here/packaging/ESSM.icns" "$app/Contents/Resources/ESSM.icns"

echo "==> ad-hoc code signature"
codesign --force --deep -s - "$app" 2>/dev/null

echo "packaged: $app"
du -sh "$app"
