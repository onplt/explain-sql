-- Deterministic dataset for the EXPLAIN fixture corpus.
--
-- `cargo xtask gen-fixtures` loads this script once into a fresh PostgreSQL
-- container per major version. Every value is derived from generate_series,
-- so plan shapes and row counts are reproducible; only timings and buffer
-- counts change between runs. Autovacuum is off on the server, so statistics
-- and the visibility map stay exactly as this script leaves them.

-- customers: 20,000 rows, 10 countries.
CREATE TABLE customers (
    id         integer     PRIMARY KEY,
    name       text        NOT NULL,
    email      text        NOT NULL,
    country    text        NOT NULL,
    created_at timestamptz NOT NULL
);

INSERT INTO customers (id, name, email, country, created_at)
SELECT i,
       'Customer ' || i,
       'user' || i || '@example.com',
       (ARRAY['TR', 'DE', 'US', 'GB', 'FR', 'NL', 'ES', 'IT', 'PL', 'SE'])[1 + i % 10],
       timestamptz '2023-01-01 00:00:00+00' + (i % 730) * interval '1 day'
FROM generate_series(1, 20000) AS i;

-- products: 5,000 rows, 5 categories.
CREATE TABLE products (
    id       integer       PRIMARY KEY,
    name     text          NOT NULL,
    category text          NOT NULL,
    price    numeric(10,2) NOT NULL
);

INSERT INTO products (id, name, category, price)
SELECT i,
       'Product ' || i,
       (ARRAY['books', 'games', 'music', 'tools', 'toys'])[1 + i % 5],
       5 + (i * 37) % 500 + 0.99
FROM generate_series(1, 5000) AS i;

-- orders: 200,000 rows (about 2,300 pages), 10 orders per customer.
-- customer_id and status are deliberately not indexed. Status mix:
-- delivered 70%, shipped 20%, pending 5%, cancelled 4%, refunded 1%.
CREATE TABLE orders (
    id          integer       PRIMARY KEY,
    customer_id integer       NOT NULL REFERENCES customers (id),
    status      text          NOT NULL,
    created_at  timestamptz   NOT NULL,
    amount      numeric(10,2) NOT NULL,
    note        text          NOT NULL
);

INSERT INTO orders (id, customer_id, status, created_at, amount, note)
SELECT i,
       1 + (i * 7919) % 20000,
       CASE
           WHEN i % 100 < 70 THEN 'delivered'
           WHEN i % 100 < 90 THEN 'shipped'
           WHEN i % 100 < 95 THEN 'pending'
           WHEN i % 100 < 99 THEN 'cancelled'
           ELSE 'refunded'
       END,
       timestamptz '2024-01-01 00:00:00+00'
           + (i % 730) * interval '1 day'
           + (i % 1440) * interval '1 minute',
       (i::bigint * 104729) % 100000 / 100.0,
       md5(i::text)
FROM generate_series(1, 200000) AS i;

CREATE INDEX orders_created_at_idx ON orders (created_at);

-- order_items: 300,000 rows (about 1,900 pages) for orders 1 to 190,000.
-- Orders above 190,000 have no items, so they can be deleted without
-- violating the foreign key. order_id is deliberately not indexed.
CREATE TABLE order_items (
    id         integer       PRIMARY KEY,
    order_id   integer       NOT NULL REFERENCES orders (id),
    product_id integer       NOT NULL REFERENCES products (id),
    quantity   integer       NOT NULL,
    unit_price numeric(10,2) NOT NULL
);

INSERT INTO order_items (id, order_id, product_id, quantity, unit_price)
SELECT i,
       1 + (i - 1) % 190000,
       1 + (i * 13) % 5000,
       1 + i % 5,
       1 + (i * 7) % 5000 / 10.0
FROM generate_series(1, 300000) AS i;

-- events: range-partitioned by month, 12 partitions for 2025, 120,000 rows.
CREATE TABLE events (
    id         bigint      NOT NULL,
    created_at timestamptz NOT NULL,
    kind       text        NOT NULL,
    payload    jsonb       NOT NULL
) PARTITION BY RANGE (created_at);

CREATE TABLE events_2025_01 PARTITION OF events FOR VALUES FROM ('2025-01-01 00:00:00+00') TO ('2025-02-01 00:00:00+00');
CREATE TABLE events_2025_02 PARTITION OF events FOR VALUES FROM ('2025-02-01 00:00:00+00') TO ('2025-03-01 00:00:00+00');
CREATE TABLE events_2025_03 PARTITION OF events FOR VALUES FROM ('2025-03-01 00:00:00+00') TO ('2025-04-01 00:00:00+00');
CREATE TABLE events_2025_04 PARTITION OF events FOR VALUES FROM ('2025-04-01 00:00:00+00') TO ('2025-05-01 00:00:00+00');
CREATE TABLE events_2025_05 PARTITION OF events FOR VALUES FROM ('2025-05-01 00:00:00+00') TO ('2025-06-01 00:00:00+00');
CREATE TABLE events_2025_06 PARTITION OF events FOR VALUES FROM ('2025-06-01 00:00:00+00') TO ('2025-07-01 00:00:00+00');
CREATE TABLE events_2025_07 PARTITION OF events FOR VALUES FROM ('2025-07-01 00:00:00+00') TO ('2025-08-01 00:00:00+00');
CREATE TABLE events_2025_08 PARTITION OF events FOR VALUES FROM ('2025-08-01 00:00:00+00') TO ('2025-09-01 00:00:00+00');
CREATE TABLE events_2025_09 PARTITION OF events FOR VALUES FROM ('2025-09-01 00:00:00+00') TO ('2025-10-01 00:00:00+00');
CREATE TABLE events_2025_10 PARTITION OF events FOR VALUES FROM ('2025-10-01 00:00:00+00') TO ('2025-11-01 00:00:00+00');
CREATE TABLE events_2025_11 PARTITION OF events FOR VALUES FROM ('2025-11-01 00:00:00+00') TO ('2025-12-01 00:00:00+00');
CREATE TABLE events_2025_12 PARTITION OF events FOR VALUES FROM ('2025-12-01 00:00:00+00') TO ('2026-01-01 00:00:00+00');

INSERT INTO events (id, created_at, kind, payload)
SELECT i,
       timestamptz '2025-01-01 00:00:00+00'
           + (i % 365) * interval '1 day'
           + (i % 86400) * interval '1 second',
       (ARRAY['click', 'view', 'purchase', 'signup'])[1 + i % 4],
       jsonb_build_object('n', i % 1000, 'source', (ARRAY['web', 'ios', 'android'])[1 + i % 3])
FROM generate_series(1, 120000) AS i;

CREATE INDEX events_created_at_idx ON events (created_at);

-- addresses: 200,000 rows where city determines country (correlated columns).
CREATE TABLE addresses (
    id      integer PRIMARY KEY,
    city    integer NOT NULL,
    country integer NOT NULL,
    street  text    NOT NULL
);

INSERT INTO addresses (id, city, country, street)
SELECT i, i % 1000, (i % 1000) / 50, 'Street ' || i % 977
FROM generate_series(1, 200000) AS i;

-- page_views: updated after the final VACUUM (see the end of this script),
-- so most pages are not all-visible and index-only scans need heap fetches.
CREATE TABLE page_views (
    id        integer     PRIMARY KEY,
    page      integer     NOT NULL,
    viewed_at timestamptz NOT NULL
);

INSERT INTO page_views (id, page, viewed_at)
SELECT i, i % 500, timestamptz '2025-01-01 00:00:00+00' + i * interval '1 minute'
FROM generate_series(1, 100000) AS i;

CREATE INDEX page_views_page_idx ON page_views (page);

-- shipments: statistics go stale on purpose. ANALYZE sees only 'archived'
-- rows; 50,000 'active' rows arrive afterwards (see the end of this script).
CREATE TABLE shipments (
    id     integer PRIMARY KEY,
    state  text    NOT NULL,
    weight integer NOT NULL
);

INSERT INTO shipments (id, state, weight)
SELECT i, 'archived', i % 100
FROM generate_series(1, 50000) AS i;

-- audit_log: 5,000 rows and no index at all.
CREATE TABLE audit_log (
    id     integer     NOT NULL,
    action text        NOT NULL,
    at     timestamptz NOT NULL
);

INSERT INTO audit_log (id, action, at)
SELECT i,
       (ARRAY['login', 'logout', 'update', 'delete'])[1 + i % 4],
       timestamptz '2025-01-01 00:00:00+00' + i * interval '1 second'
FROM generate_series(1, 5000) AS i;

-- settings_kv: a 50-row lookup table.
CREATE TABLE settings_kv (
    key   text PRIMARY KEY,
    value text NOT NULL
);

INSERT INTO settings_kv (key, value)
SELECT 'key' || i, 'value' || i % 7
FROM generate_series(1, 50) AS i;

-- FREEZE makes the vacuum aggressive, so every page ends up all-visible. A
-- plain VACUUM right after the load leaves the last pages of orders behind
-- on some versions, which made index-only scans differ between versions.
VACUUM (FREEZE, ANALYZE);

-- Changes after the final VACUUM. They are never vacuumed or analyzed.
UPDATE page_views SET viewed_at = viewed_at + interval '1 second' WHERE id % 10 = 0;

INSERT INTO shipments (id, state, weight)
SELECT i, 'active', i % 100
FROM generate_series(50001, 100000) AS i;
