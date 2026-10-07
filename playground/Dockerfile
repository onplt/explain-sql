# PostgreSQL 16 with HypoPG, for trying ExplainSQL locally (see README.md).
# The official image already carries the PGDG apt repository.
FROM postgres:16
RUN apt-get update \
 && apt-get install -y --no-install-recommends postgresql-16-hypopg \
 && rm -rf /var/lib/apt/lists/*
