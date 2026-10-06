# The viewer

In a terminal, ExplainSQL opens plans in an interactive viewer. It is built to answer "why is this slow?" on the first screen, and to let you dig in from there.

## The screen

- **The verdict** sits on the top line: the statement's time, where most of it went, and why. For example: *"20.0 ms. 99% of it in Seq Scan on orders o, which reads 200,000 rows to keep 10."*
- **The statement line** under it: planning and execution time, pages read and how many came from the cache, and when they matter, time in triggers, in JIT, outside the plan tree, and reading from disk.
- **The plan tree**: each node's share of the runtime, its own time, a bar, the node, actual rows, the estimate, and the pages it read itself. `▲` and `▼` mark rows that are 10× or more above or below the estimate. `!` in the margin marks a node with a finding.
- **The details** of the selected node: every figure the plan has for it, its conditions, its findings, and the answer when you ask the planner about it.
- **Findings or advice**, at the bottom, with the status line under them.

On a wide terminal (110 columns or more), the details sit next to the tree; on a narrower one, below it. Columns are dropped before node names get too short to read, so the viewer stays usable at 80×24.

## Keys

| Keys | What they do |
|---|---|
| `j` `k` `↓` `↑` | Move |
| `PgDn` `PgUp`, `g` `G` | Move a page, go to the first or last node |
| `h` `l` `←` `→`, `Enter` `Space` | Fold and unfold. `h` on a leaf goes to its parent |
| `/`, `n` `N` | Search node names and conditions, next and previous match |
| `1` … `9` | Go to the slowest nodes |
| `f`, `i` | Show the findings, or the advice |
| `Tab` | Move between the plan and the list below it |
| `Enter` | In the list: go to the node, opening any folds that hide it |
| `c` | Copy the suggested `CREATE INDEX` to the clipboard |
| `x` | Time in the node, or including its children |
| `w` | Wall-clock time, or CPU time summed over parallel workers |
| `b` | Rank by time, or by pages |
| `J` `K` | Scroll the details |
| `F` | Switch between the tree and the [icicle view](#the-icicle-view) |
| `r`, `e` | Connected: run the statement again, edit it in `$VISUAL` or `$EDITOR` |
| `t` | Connected: [test the suggested index](connected.md#test-a-suggested-index) |
| `y` | Connected: [ask the planner why](why-not.md) it chose the selected node |
| `L` | Connected: [the locks the statement takes](locks.md) |
| `W` | Connected: [what the statement's writes cost](writes.md), once the measured plan is in |
| `Esc` | Connected: cancel a running statement |
| `?` | Help |
| `q`, `Esc` | Quit |

## Times, shares and views

By default, each node shows its **own** time: the time spent in the node, minus the time spent in its children. That sounds like simple subtraction, but it is not. Times in a plan are averages per loop while buffers are totals; under a `Gather`, several processes run a node side by side; a CTE's work shows up inside whichever scan pulls its rows first; an InitPlan runs inside the node that needs its result, not the one it is listed under. ExplainSQL handles all of these, so the shares add up to the statement's time. The [architecture](../ARCHITECTURE.md#metrics-inclusive-and-exclusive-time) has the details.

Three keys change the view:

- `x` shows time **including children**, which is what `EXPLAIN` prints as "actual time".
- `w` shows **CPU time**, summed over the leader and its parallel workers, instead of wall-clock time.
- `b` ranks nodes by **pages** instead of time. Pages do not change with the state of the cache, which makes them the steadier measure of how much work a node does.

A plan without timing (captured with `TIMING OFF`, or not run at all) has no times to show, so the hotspots are ranked by pages instead. The viewer never fills in a time from an estimate.

## Folding and search

Any node can be folded with `h` or `←` and opened again with `l` or `→`. Runs of four or more similar leaves start folded into a single row that adds up their time, rows and pages, so a plan that scans 1,000 partitions shows one line, not a thousand. Leaves are similar when they have the same type and the same relation and conditions once numbers are blanked out.

`/` searches node names and conditions, and `n` and `N` move between matches. A match hidden inside a fold is opened for you.

## Findings and advice

`f` lists the findings, most severe first, and `i` lists the advice: suggested indexes, rewrites of conditions that wrap a column in a function or a cast, and the reasons a slow scan gets no index. Press `Tab` to move into the list, then `Enter` to jump to the node.

On a suggested index, `c` copies its `CREATE INDEX CONCURRENTLY` statement. It uses the OSC 52 escape sequence, so it reaches your local clipboard even over SSH and inside tmux, as long as your terminal supports it.

## The icicle view

`F` replaces the tree with an icicle: the root across the top, and each node in a box under its parent, as wide as the CPU time spent in it and below it, and colored by its own share. It is the quickest way to see where the time goes in a big plan.

- Workers of a parallel plan add up under the node that gathers them.
- InitPlans, SubPlans and CTEs sit under the node they belong to.
- Time in triggers, which happens outside the tree, is shown in the title.
- A plan without timing is drawn by estimated cost, and the title says so.

In the icicle, `k` and `j` go to the parent and to the widest child, and `h` and `l` move along the row. `Enter` zooms in on a box so that it fills the width; `Enter` on it again zooms back out, and `g` returns to the whole plan. Nodes too narrow for a single column are folded into their parent and shown as `…`, and zooming opens them. The details, search and the hotspot keys work as in the tree, and `F` takes you back to the tree on the same node.

## In connected mode

When ExplainSQL ran the statement itself ([connected mode](connected.md)), the viewer can do more:

- It shows the estimated plan at once, runs `EXPLAIN ANALYZE` in the background with a counter in the status line, and swaps in the measured plan when it is ready. `Esc` cancels the run.
- `r` runs the statement again. `e` opens it in `$VISUAL` or `$EDITOR`; when you save and close, the viewer shows the estimated plan of the edited statement and runs it. After either, the status line compares the new run with the previous one, pages first.
- `t`, `y`, `L` and `W` test an index, ask the planner, and show the locks and the writes, as described in their chapters.

## Colors and themes

The viewer follows your terminal: true color when `COLORTERM` says so, otherwise 256 or 16 colors. With `NO_COLOR` set, it falls back to bold, dim and reverse video. Use `--theme light` on a light background. Severities and misestimates always carry a word or a symbol as well as a color, so nothing depends on color alone.

The viewer reads keys from the terminal even when the plan came in on standard input, so `pbpaste | explainsql` is fully interactive.
