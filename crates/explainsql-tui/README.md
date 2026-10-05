# explainsql-tui

The interactive viewer of [ExplainSQL](https://github.com/onplt/explain-sql), a terminal tool that finds out why a PostgreSQL query is slow, suggests a fix and proves it works.

Built on Ratatui, it shows:

- the verdict;
- a virtualized plan tree with heat bars;
- the details of each node;
- the findings and the index advice.

It also offers search, folding of similar siblings and view modes, and stays usable at 80×24.

To use ExplainSQL itself, install the `explainsql` crate: `cargo install explainsql --locked`.

Licensed under either of Apache License, Version 2.0 or MIT license, at your option.
