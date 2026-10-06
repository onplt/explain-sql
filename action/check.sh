#!/usr/bin/env bash
# The action's second step: runs explainsql check and keeps its Markdown and
# SARIF reports. The step itself passes, so that the comment can be written;
# the exit code is in the step outputs, and the action's last step fails
# with it.
set -uo pipefail

# A directory of this run's own: a job can use the action more than once,
# and each run's outputs must keep naming its own reports.
mkdir -p "${RUNNER_TEMP:-/tmp}/explainsql-action"
out="$(mktemp -d "${RUNNER_TEMP:-/tmp}/explainsql-action/check.XXXXXX")"
report="$out/report.md"
sarif="$out/report.sarif"

# Split the paths and the extra arguments on white space, with no globbing.
set -f
read -r -d '' -a paths <<< "$INPUT_PATHS" || true
read -r -d '' -a extra <<< "${INPUT_ARGS:-}" || true
set +f

args=(check "${paths[@]}" --lock "$INPUT_LOCK" --format md --sarif "$sarif" --color never)
[ -n "${INPUT_DATABASE_URL:-}" ] && args+=(-d "$INPUT_DATABASE_URL")
[ -n "${INPUT_FAIL_ON:-}" ] && args+=(--fail-on "$INPUT_FAIL_ON")
[ "${INPUT_STRICT:-false}" = "true" ] && args+=(--strict)
# ${a[@]+...}: an empty array under set -u, in the bash 3.2 of macOS too.
args+=(${extra[@]+"${extra[@]}"})

"$EXPLAINSQL" "${args[@]}" > "$report"
code=$?
case "$code" in
    0) result=passed ;;
    1) result=failed ;;
    *) result=error ;;
esac

if [ -s "$report" ]; then
    cat "$report" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
    echo "report=$report" >> "$GITHUB_OUTPUT"
fi
[ -s "$sarif" ] && echo "sarif=$sarif" >> "$GITHUB_OUTPUT"
{
    echo "exit-code=$code"
    echo "result=$result"
} >> "$GITHUB_OUTPUT"
echo "explainsql check: $result (exit code $code)"
exit 0
