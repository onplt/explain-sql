#!/usr/bin/env bash
# The action's comment step: writes the report on the pull request as one
# comment, found again on later runs by a hidden marker and updated in
# place. A failed check posts or updates it; a check that passed only
# updates a comment already there, to say so. Pull requests from forks get
# no comment, since their token cannot write one: the report is in the job
# summary.
set -euo pipefail

marker="<!-- explainsql-action: ${KEY:-default} -->"

if [ -n "${HEAD_REPO:-}" ] && [ "$HEAD_REPO" != "$REPOSITORY" ]; then
    echo "::notice::A pull request from a fork: the explainsql report is in the job summary, not in a comment."
    exit 0
fi
case "${RESULT:-}" in
    passed | failed) ;;
    *) echo "explainsql check did not run to the end: no comment"; exit 0 ;;
esac

existing="$(gh api --paginate "repos/$REPOSITORY/issues/$PR_NUMBER/comments" \
    --jq ".[] | select(.body | contains(\"$marker\")) | .id" | head -n 1)" || {
    echo "::warning::Cannot read the pull request's comments; does the token have the pull-requests write permission?"
    exit 0
}

body="${RUNNER_TEMP:-/tmp}/explainsql-action/comment.md"
if [ "$RESULT" = "failed" ]; then
    { cat "$REPORT"; echo; echo "$marker"; } > "$body"
elif [ -n "$existing" ]; then
    {
        echo "<!-- explainsql check -->"
        echo "### explainsql check"
        echo
        echo "**No plan failed** as of ${HEAD_SHA:0:7}."
        echo
        echo "$marker"
    } > "$body"
else
    echo "No plan failed and no comment to update."
    exit 0
fi

if [ -n "$existing" ]; then
    gh api --method PATCH "repos/$REPOSITORY/issues/comments/$existing" -F "body=@$body" --silent \
        && echo "Updated the comment." \
        || echo "::warning::Cannot update the comment; does the token have the pull-requests write permission?"
else
    gh api --method POST "repos/$REPOSITORY/issues/$PR_NUMBER/comments" -F "body=@$body" --silent \
        && echo "Commented on the pull request." \
        || echo "::warning::Cannot comment; does the token have the pull-requests write permission?"
fi
