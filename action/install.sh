#!/usr/bin/env bash
# The action's first step: puts explainsql on the runner and names it in the
# step output `binary`. INPUT_BINARY, when set, is used as it is; otherwise
# the release INPUT_VERSION is installed, or the release the action was
# referenced by (ACTION_REF, such as v0.3.0), or the latest one.
set -euo pipefail

if [ -n "${INPUT_BINARY:-}" ]; then
    [ -x "$INPUT_BINARY" ] || { echo "::error::$INPUT_BINARY is not an executable"; exit 1; }
    binary="$(cd "$(dirname "$INPUT_BINARY")" && pwd)/$(basename "$INPUT_BINARY")"
else
    case "$(uname -s)" in
        Linux | Darwin) ;;
        *) echo "::error::the explainsql action runs on Linux and macOS runners"; exit 1 ;;
    esac
    version="${INPUT_VERSION:-}"
    if [ -z "$version" ] && [[ "${ACTION_REF:-}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        version="$ACTION_REF"
    fi
    dir="${RUNNER_TEMP:-/tmp}/explainsql-action/bin"
    if [ -n "$version" ]; then
        sh "$GITHUB_ACTION_PATH/install/install.sh" --version "${version#v}" --to "$dir"
    else
        sh "$GITHUB_ACTION_PATH/install/install.sh" --to "$dir"
    fi
    binary="$dir/explainsql"
fi

"$binary" --version
echo "binary=$binary" >> "$GITHUB_OUTPUT"
