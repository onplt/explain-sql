# Test plans for the action

`.github/workflows/action.yml` runs the action on these. In `unchanged/` the
plan is the one locked in its `explainsql.lock`, and the check passes; in
`regressed/` the index scan that was locked became a sequential scan, and
the check fails.
