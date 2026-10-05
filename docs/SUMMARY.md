# Summary

[Introduction](index.md)

# User guide

- [Install and use](guide.md)

# Rules

- [Rule catalog](rules.md)
  - [ES001: Selective sequential scan](rules/ES001.md)
  - [ES002: Row misestimate](rules/ES002.md)
  - [ES003: Sort spilled to disk](rules/ES003.md)
  - [ES004: Hash or aggregate spilled to disk](rules/ES004.md)
  - [ES005: Expensive nested-loop inner side](rules/ES005.md)
  - [ES006: Index scan that filters most rows](rules/ES006.md)
  - [ES007: Index-only scan with many heap fetches](rules/ES007.md)
  - [ES008: Lossy bitmap or heavy recheck](rules/ES008.md)
  - [ES009: Slow foreign-key trigger](rules/ES009.md)
  - [ES010: Cartesian product](rules/ES010.md)
  - [ES011: Fewer parallel workers than planned](rules/ES011.md)
  - [ES012: JIT overhead dominates](rules/ES012.md)

# Design

- [Vision](VISION.md)
- [Architecture](ARCHITECTURE.md)
- [Roadmap](ROADMAP.md)
