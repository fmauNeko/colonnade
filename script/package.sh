#!/usr/bin/env bash
# Packages the release build of Colonnade for the host platform into dist/:
# a portable archive plus its SHA-256 checksum.
#
#   macOS    colonnade-<version>-<target>.zip     Colonnade.app (ad-hoc signed)
#   Linux    colonnade-<version>-<target>.tar.gz  binary + desktop entry
#   Windows  colonnade-<version>-<target>.zip     colonnade.exe
#
# Run `mise run build:release` first. Environment:
#   COLONNADE_VERSION  version label of the archive (default: dev-<short commit>)
#   EXPECTED_TARGET    fail unless the host target triple matches it (CI guard)
set -euo pipefail

cd "$(dirname "$0")/.."

target="$(rustc -vV | sed -n 's/^host: //p')"
if [[ -n "${EXPECTED_TARGET:-}" && "$target" != "$EXPECTED_TARGET" ]]; then
  echo "error: host target $target does not match the expected $EXPECTED_TARGET" >&2
  exit 1
fi

version="${COLONNADE_VERSION:-dev-$(git rev-parse --short HEAD)}"
# Info.plist needs a numeric version: use the crate version release-please maintains.
bundle_version="$(cargo pkgid | sed 's/.*[#@]//')"
name="colonnade-${version}-${target}"
stage="target/package/${name}"

rm -rf dist "$stage"
mkdir -p dist "$stage"

case "$target" in
  *-apple-darwin)
    app="$stage/Colonnade.app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp target/release/colonnade "$app/Contents/MacOS/colonnade"
    sed "s/@VERSION@/${bundle_version}/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
    codesign --force --sign - "$app"
    archive="dist/${name}.zip"
    ditto -c -k --keepParent "$app" "$archive"
    ;;
  *-linux-*)
    cp target/release/colonnade packaging/linux/ovh.dissidence.colonnade.desktop \
      LICENSE README.md "$stage/"
    archive="dist/${name}.tar.gz"
    tar -czf "$archive" -C "$(dirname "$stage")" "$name"
    ;;
  *-windows-*)
    cp target/release/colonnade.exe LICENSE README.md "$stage/"
    archive="dist/${name}.zip"
    # Windows' bsdtar writes standard zip archives, unlike Git Bash's GNU tar.
    "$(cygpath -u "$SYSTEMROOT")/System32/tar.exe" -a -cf "$archive" -C "$(dirname "$stage")" "$name"
    ;;
  *)
    echo "error: no packaging recipe for $target" >&2
    exit 1
    ;;
esac

if command -v sha256sum >/dev/null; then
  (cd dist && sha256sum "$(basename "$archive")" > "$(basename "$archive").sha256")
else
  (cd dist && shasum -a 256 "$(basename "$archive")" > "$(basename "$archive").sha256")
fi

echo "$archive"
