"""Compares ExplainSQL's exclusive times with another engine's, node by node.

    python3 compare.py <label> <command…>

runs `<command…> <plan file>` for each reference plan; the command prints one
line per node (see depesz.pl and pev2.mjs). Nodes are matched by their
actual time, rows and loops. A node agrees when the two exclusive times are
within 5% of each other, or 0.01 ms.
"""
import json, subprocess, sys, os
from collections import defaultdict

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
BIN = os.environ.get("EXPLAINSQL", f"{REPO}/target/debug/explainsql")
REFERENCE = """seq_scan_selective nested_loop_inner_seq_scan lateral_join_top_n hash_join_batches
hash_aggregate_spill sort_external_merge parallel_hash_join parallel_aggregate parallel_gather_merge
parallel_seq_scan cte_materialized cte_multiple_scans cte_recursive initplan subplan_correlated
subplan_hashed nested_loop_memoize merge_join partition_append_all window_function delete_fk_trigger
bitmap_or anti_join semi_join""".split()

def key(time, rows, loops):
    return (round(float(time), 3), round(float(rows), 2), int(float(loops)))

def ours(path):
    report = json.loads(subprocess.run([BIN, "--format", "json", path], capture_output=True, text=True, check=True).stdout)
    out = []
    for node, metrics in zip(report["plan"]["nodes"], report["metrics"]["nodes"]):
        actuals = node.get("actuals")
        if not actuals or "total_time" not in actuals:
            continue
        out.append((key(actuals["total_time"], actuals["rows"], actuals["loops"]), metrics.get("exclusive_time", 0.0), node["node_type"]))
    return out

def run_other(engine, path):
    lines = subprocess.run(engine + [path], capture_output=True, text=True, check=True).stdout.splitlines()
    out = defaultdict(list)
    for line in lines:
        time, rows, loops, excl, incl, kind = line.split("\t")
        if time == "" or float(excl) < 0:
            continue
        out[key(time, rows, loops)].append((float(excl), kind))
    return out

def main(engine, label, major):
    total = agree = 0
    worst = []
    for scenario in REFERENCE:
        path = f"{REPO}/fixtures/pg/{major}/{scenario}.txt"
        if not os.path.exists(path):
            continue
        theirs = run_other(engine, path)
        for k, excl, kind in ours(path):
            candidates = theirs.get(k)
            if not candidates:
                print(f"  {scenario}: no match for {kind} {k}")
                continue
            other = candidates.pop(0)[0]
            total += 1
            diff = abs(excl - other)
            ok = diff <= max(0.05 * max(excl, other), 0.01)
            agree += ok
            if not ok:
                worst.append((diff, scenario, kind, excl, other))
    print(f"{label}, PostgreSQL {major}: {agree}/{total} nodes within ±5% (or 0.01 ms)")
    for diff, scenario, kind, excl, other in sorted(worst, reverse=True)[:15]:
        print(f"  {scenario:28} {kind:22} ours {excl:10.3f} ms   {label} {other:10.3f} ms")

if __name__ == "__main__":
    label, engine = sys.argv[1], sys.argv[2:]
    for major in (13, 16, 18):
        main(engine, label, major)
