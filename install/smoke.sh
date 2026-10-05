#!/bin/sh
# Smoke test of an installed explainsql: smoke.sh BINARY
#
# Run by the release workflow on every target, from the repository root.

set -eu

bin="${1:?usage: smoke.sh BINARY}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

check() {
    printf '  %s ... ' "$1"
}

check "--version"
"$bin" --version | grep -q '^explainsql [0-9]' && echo ok

check "--demo --print"
"$bin" --demo --print > "$work/demo.txt"
grep -q "ES001 Selective sequential scan" "$work/demo.txt"
grep -q "ES005 Expensive nested-loop inner side" "$work/demo.txt"
grep -q "CREATE INDEX CONCURRENTLY ON public.order_items (order_id);" "$work/demo.txt"
echo ok

check "a plan file, as JSON"
"$bin" --format json fixtures/pg/16/seq_scan_selective.json > "$work/report.json"
grep -q '"verdict"' "$work/report.json" && echo ok

check "a psql table on standard input, as Markdown"
"$bin" --format md < fixtures/inputs/psql-aligned.txt | grep -q '| Share | Time | Node |' && echo ok

check "--pager passes other output through"
printf ' id | name\n----+------\n  1 | x\n(1 row)\n' > "$work/table.txt"
"$bin" --pager < "$work/table.txt" > "$work/paged.txt"
cmp -s "$work/table.txt" "$work/paged.txt" && echo ok

check "input that is not a plan fails"
if printf 'hello\n' | "$bin" > /dev/null 2>&1; then
    echo "FAILED: exit code 0"
    exit 1
fi
echo ok
