#!/bin/sh
# Installs explainsql from a GitHub release.
#
#   curl -fsSL https://github.com/onplt/explain-sql/releases/latest/download/install.sh | sh
#   sh install.sh [--version 0.1.0] [--to DIR] [--target TRIPLE] [--from DIR|URL]
#
# The archive's SHA-256 checksum is checked before anything is installed.
# --from installs from a directory or URL holding the release files, as the
# release workflow's smoke tests do.

set -eu

repo="onplt/explain-sql"
version=""
to="${EXPLAINSQL_INSTALL_DIR:-$HOME/.local/bin}"
target=""
from=""

fail() {
    echo "explainsql install: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) version="${2:?--version needs a value}"; shift 2 ;;
        --to) to="${2:?--to needs a directory}"; shift 2 ;;
        --target) target="${2:?--target needs a target triple}"; shift 2 ;;
        --from) from="${2:?--from needs a directory or URL}"; shift 2 ;;
        -h | --help)
            sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) fail "unknown option $1 (see --help)" ;;
    esac
done

if [ -z "$target" ]; then
    case "$(uname -s)" in
        Linux) os="unknown-linux-musl" ;;
        Darwin) os="apple-darwin" ;;
        *) fail "unsupported system $(uname -s); on Windows, use install.ps1" ;;
    esac
    case "$(uname -m)" in
        x86_64 | amd64) arch="x86_64" ;;
        arm64 | aarch64) arch="aarch64" ;;
        *) fail "unsupported processor $(uname -m)" ;;
    esac
    target="$arch-$os"
fi
archive="explainsql-$target.tar.gz"

if [ -n "$from" ]; then
    base="$from"
elif [ -n "$version" ]; then
    base="https://github.com/$repo/releases/download/v${version#v}"
else
    base="https://github.com/$repo/releases/latest/download"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

fetch() {
    # fetch NAME: copies or downloads $base/NAME into $work.
    if [ -d "$base" ]; then
        [ -f "$base/$1" ] || fail "$base/$1 not found"
        cp "$base/$1" "$work/$1"
    elif command -v curl > /dev/null 2>&1; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$work/$1" "$base/$1" || fail "cannot download $base/$1"
    elif command -v wget > /dev/null 2>&1; then
        wget -q -O "$work/$1" "$base/$1" || fail "cannot download $base/$1"
    else
        fail "neither curl nor wget is installed"
    fi
}

fetch "$archive"
fetch "$archive.sha256"

expected="$(cut -d ' ' -f 1 < "$work/$archive.sha256")"
if command -v sha256sum > /dev/null 2>&1; then
    actual="$(sha256sum "$work/$archive" | cut -d ' ' -f 1)"
elif command -v shasum > /dev/null 2>&1; then
    actual="$(shasum -a 256 "$work/$archive" | cut -d ' ' -f 1)"
else
    fail "cannot check the download: neither sha256sum nor shasum is installed"
fi
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    fail "checksum mismatch for $archive: expected $expected, got $actual"
fi

tar -xzf "$work/$archive" -C "$work"
mkdir -p "$to"
binary="$work/explainsql-$target/explainsql"
[ -f "$binary" ] || fail "the archive holds no explainsql binary"
cp "$binary" "$to/explainsql.tmp"
chmod 755 "$to/explainsql.tmp"
mv "$to/explainsql.tmp" "$to/explainsql"

echo "Installed $("$to/explainsql" --version) to $to/explainsql"
case ":$PATH:" in
    *":$to:"*) echo "Try it: explainsql --demo" ;;
    *) echo "$to is not on your PATH. Add it, for example: export PATH=\"$to:\$PATH\"" ;;
esac
