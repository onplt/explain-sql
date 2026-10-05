#!/usr/bin/env bash
# Packages a release build: package.sh TARGET [DIST]
#
# Puts the binary, the README, the changelog and the licenses in
# explainsql-TARGET/, archives it (.zip on Windows, .tar.gz elsewhere) in
# DIST (default dist/) and writes its SHA-256 checksum next to it.

set -euo pipefail

target="${1:?usage: package.sh TARGET [DIST]}"
dist="${2:-dist}"
name="explainsql-$target"
exe=""
[[ "$target" == *windows* ]] && exe=".exe"

binary="target/$target/release/explainsql$exe"
[[ -f "$binary" ]] || { echo "no binary at $binary" >&2; exit 1; }

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name" "$dist"
cp "$binary" README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE "$stage/$name/"

if [[ -n "$exe" ]]; then
    archive="$name.zip"
    (cd "$stage" && 7z a -tzip -bd "$archive" "$name" > /dev/null)
else
    archive="$name.tar.gz"
    tar -czf "$stage/$archive" -C "$stage" "$name"
fi
mv "$stage/$archive" "$dist/"

cd "$dist"
if command -v sha256sum > /dev/null; then
    sha256sum "$archive" > "$archive.sha256"
else
    shasum -a 256 "$archive" > "$archive.sha256"
fi
cat "$archive.sha256"
