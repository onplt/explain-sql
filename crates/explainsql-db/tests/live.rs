//! Against a real database holding `fixtures/schema.sql`, named by
//! `EXPLAINSQL_TEST_DATABASE_URL`; skipped without it. The CI's `db` job
//! runs them.
//!
//! The central promise: whatever explainsql runs is rolled back. Data-
//! modifying statements run only with `--allow-dml`, and leave the data as
//! it was.

use std::time::Duration;

use explainsql_core::ir::Plan;
use explainsql_core::scenario::Setting;
use explainsql_core::top;
use explainsql_db::{Cache, Database, Error, Mode, Safety, Settings, Writes};

fn database() -> Option<Database> {
    let Ok(url) = std::env::var("EXPLAINSQL_TEST_DATABASE_URL") else {
        eprintln!("EXPLAINSQL_TEST_DATABASE_URL is not set; skipping");
        return None;
    };
    let settings = Settings::resolve(Some(&url)).unwrap();
    Some(Database::connect(&settings).unwrap())
}

fn allow_dml() -> Safety {
    Safety {
        allow_dml: true,
        ..Safety::default()
    }
}

/// A count read the way explainsql reads plans: the actual rows of an
/// aggregate's plan, so the test needs no other connection.
fn count(db: &Database, table: &str) -> f64 {
    let json = db
        .explain(
            &format!("SELECT count(*) FROM {table}"),
            Mode::Analyze,
            Safety::default(),
        )
        .unwrap();
    let plan = explainsql_core::parse(&json).unwrap();
    let scans: f64 = plan
        .nodes
        .iter()
        .filter(|node| node.relation_name.as_deref() == Some(table))
        .map(|node| {
            let actuals = node.actuals.unwrap();
            actuals.rows * actuals.loops as f64
        })
        .sum();
    assert!(scans > 0.0 || table == "audit_log");
    scans
}

#[test]
fn explains_queries_estimated_and_measured() {
    let Some(db) = database() else { return };
    assert!(db.server_version() >= 120_000);
    let sql = "SELECT * FROM orders WHERE customer_id = 4242;";
    let estimated =
        explainsql_core::parse(&db.explain(sql, Mode::Estimate, Safety::default()).unwrap())
            .unwrap();
    assert!(estimated.root().actuals.is_none());
    let measured =
        explainsql_core::parse(&db.explain(sql, Mode::Analyze, Safety::default()).unwrap())
            .unwrap();
    assert!(measured.root().actuals.is_some());
    assert!(measured.summary.execution_time.is_some());
    assert!(measured.root().buffers.is_some());
}

#[test]
fn data_modifying_statements_are_never_committed() {
    let Some(db) = database() else { return };
    // Counted from another session, which sees only committed data.
    let observer = database().unwrap();
    let before = (count(&observer, "orders"), count(&observer, "order_items"));
    for sql in [
        "DELETE FROM order_items WHERE order_id < 1000",
        "UPDATE orders SET amount = 0 WHERE id < 1000",
        "INSERT INTO orders SELECT id + 1000000, customer_id, status, created_at, amount, note FROM orders WHERE id < 1000",
        "WITH gone AS (DELETE FROM order_items WHERE order_id < 2000 RETURNING 1) SELECT count(*) FROM gone",
    ] {
        // Refused without --allow-dml, before anything runs.
        assert!(
            matches!(
                db.explain(sql, Mode::Analyze, Safety::default()),
                Err(Error::NeedsAllowDml(_))
            ),
            "{sql}"
        );
        assert_eq!(
            db.writes(sql, Safety::default()).unwrap(),
            Writes::ModifiesData
        );
        // Run with it, and rolled back.
        let plan =
            explainsql_core::parse(&db.explain(sql, Mode::Analyze, allow_dml()).unwrap()).unwrap();
        assert!(
            plan.nodes
                .iter()
                .any(|node| node.node_type == "ModifyTable"),
            "{sql}"
        );
        assert_eq!(
            (count(&observer, "orders"), count(&observer, "order_items")),
            before,
            "{sql}"
        );
    }
    // The update really ran inside its transaction, and left no trace.
    let json = db
        .explain(
            "SELECT * FROM orders WHERE id < 1000 AND amount = 0",
            Mode::Analyze,
            Safety::default(),
        )
        .unwrap();
    assert_eq!(
        explainsql_core::parse(&json)
            .unwrap()
            .root()
            .actuals
            .unwrap()
            .rows,
        0.0
    );
}

#[test]
fn reads_run_read_only() {
    let Some(db) = database() else { return };
    // A function that writes fails in the read-only transaction.
    let error = db
        .explain("SELECT lo_create(0)", Mode::Analyze, Safety::default())
        .unwrap_err();
    assert!(
        matches!(&error, Error::Server(message) if message.contains("read-only")),
        "{error}"
    );
    // Row locks need --allow-dml.
    let sql = "SELECT * FROM orders WHERE id = 1 FOR UPDATE";
    assert_eq!(
        db.writes(sql, Safety::default()).unwrap(),
        Writes::LocksRows
    );
    assert!(matches!(
        db.explain(sql, Mode::Analyze, Safety::default()),
        Err(Error::NeedsAllowDml(_))
    ));
    assert!(db.explain(sql, Mode::Analyze, allow_dml()).is_ok());
}

#[test]
fn refuses_what_it_should_not_run() {
    let Some(db) = database() else { return };
    let before = count(&db, "order_items");
    // Several statements: the extended protocol refuses them.
    let error = db
        .explain(
            "SELECT 1; DELETE FROM order_items",
            Mode::Analyze,
            allow_dml(),
        )
        .unwrap_err();
    assert!(matches!(error, Error::Server(_)), "{error}");
    assert!(matches!(
        db.explain("DROP TABLE order_items", Mode::Analyze, allow_dml()),
        Err(Error::Refused(_))
    ));
    assert_eq!(count(&db, "order_items"), before);
}

#[test]
fn stops_at_the_timeout_and_on_cancel() {
    let Some(db) = database() else { return };
    let short = Safety {
        timeout: Duration::from_millis(300),
        ..Safety::default()
    };
    let error = db
        .explain("SELECT pg_sleep(5)", Mode::Analyze, short)
        .unwrap_err();
    assert_eq!(error, Error::Timeout(Duration::from_millis(300)));
    // The connection is still usable: the transaction was rolled back.
    assert!(
        db.explain("SELECT 1", Mode::Analyze, Safety::default())
            .is_ok()
    );

    let canceller = db.canceller();
    let cancelling = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel();
    });
    let error = db
        .explain("SELECT pg_sleep(10)", Mode::Analyze, Safety::default())
        .unwrap_err();
    cancelling.join().unwrap();
    assert_eq!(error, Error::Cancelled);
    assert!(
        db.explain("SELECT 1", Mode::Analyze, Safety::default())
            .is_ok()
    );
}

#[test]
fn reads_the_catalog() {
    let Some(db) = database() else { return };
    let catalog = db
        .catalog(
            &[
                (None, "orders".to_owned()),
                (Some("public".to_owned()), "events".to_owned()),
                (None, "no_such_table".to_owned()),
            ],
            &["order_items_order_id_fkey".to_owned()],
        )
        .unwrap();
    let orders = catalog.table(Some("public"), "orders").unwrap();
    assert!(orders.pages > 1000.0);
    assert!(
        orders
            .indexes
            .iter()
            .any(|index| index.columns == ["created_at"] && index.method == "btree" && index.valid),
        "{:?}",
        orders.indexes
    );
    assert!(orders.column("customer_id").is_some());
    assert!(catalog.table(None, "events").unwrap().partitioned);
    assert!(catalog.table(None, "no_such_table").is_none());
    assert_eq!(catalog.foreign_keys.len(), 1);
    assert_eq!(catalog.foreign_keys[0].table, "order_items");
    assert_eq!(catalog.foreign_keys[0].columns, ["order_id"]);
}

fn index_names(db: &Database, table: &str) -> Vec<String> {
    let catalog = db.catalog(&[(None, table.to_owned())], &[]).unwrap();
    let mut names: Vec<String> = catalog.tables[0]
        .indexes
        .iter()
        .map(|index| index.name.clone())
        .collect();
    names.sort();
    names
}

const SELECTIVE: &str = "SELECT * FROM orders WHERE customer_id = 4242";
const DDL: &str = "CREATE INDEX CONCURRENTLY ON public.orders (customer_id);";

#[test]
fn proves_an_index_with_hypopg() {
    let Some(db) = database() else { return };
    let catalog = db.catalog(&[], &[]).unwrap();
    if !catalog.extensions.iter().any(|name| name == "hypopg") {
        eprintln!("HypoPG is not installed; skipping");
        return;
    }
    let proof = db
        .prove(SELECTIVE, DDL, false, 1, Safety::default())
        .unwrap();
    assert!(!proof.measured);
    assert_eq!((proof.before.len(), proof.after.len()), (1, 1));
    let before = explainsql_core::parse(&proof.before[0]).unwrap();
    let after = explainsql_core::parse(&proof.after[0]).unwrap();
    let comparison = explainsql_core::compare::compare(&before, &after);
    assert!(comparison.improved(), "{}", comparison.summary());
    assert!(
        comparison
            .new_indexes
            .iter()
            .any(|name| name.contains("orders_customer_id")),
        "{:?}",
        comparison.new_indexes
    );
    // Nothing is left behind, in the session or the catalog.
    let again = db
        .prove(SELECTIVE, DDL, false, 1, Safety::default())
        .unwrap();
    assert_eq!(
        explainsql_core::parse(&again.before[0])
            .unwrap()
            .root()
            .node_type,
        before.root().node_type
    );
}

#[test]
fn proves_an_index_built_and_rolled_back() {
    let Some(db) = database() else { return };
    let indexes = index_names(&db, "orders");
    assert!(matches!(
        db.prove(SELECTIVE, DDL, true, 1, Safety::default()),
        Err(Error::Refused(message)) if message.contains("--allow-ddl")
    ));
    let safety = Safety {
        allow_ddl: true,
        ..Safety::default()
    };
    let proof = db.prove(SELECTIVE, DDL, true, 2, safety).unwrap();
    assert!(proof.measured);
    // Two measured runs on each side, after a run that warms the cache.
    assert_eq!((proof.before.len(), proof.after.len()), (2, 2));
    let parse = |plans: &[String]| -> Vec<explainsql_core::ir::Plan> {
        plans
            .iter()
            .map(|plan| explainsql_core::parse(plan).unwrap())
            .collect()
    };
    let comparison =
        explainsql_core::compare::compare_runs(&parse(&proof.before), &parse(&proof.after));
    assert!(comparison.after.execution_time.is_some());
    assert_eq!(comparison.after.runs, 2);
    assert!(comparison.improved(), "{}", comparison.summary());
    assert_eq!(
        comparison.basis,
        Some(explainsql_core::compare::Basis::Pages),
        "{}",
        comparison.summary()
    );
    // The index was rolled back with its transaction.
    assert_eq!(index_names(&db, "orders"), indexes);
    // Only CREATE INDEX statements are built.
    assert!(matches!(
        db.prove(
            SELECTIVE,
            "DROP INDEX orders_created_at_idx",
            true,
            1,
            safety
        ),
        Err(Error::Refused(_))
    ));
    assert_eq!(index_names(&db, "orders"), indexes);
}

#[test]
fn plans_under_settings_only_inside_the_transaction() {
    let Some(db) = database() else { return };
    let parse = |json: String| explainsql_core::parse(&json).unwrap();
    // audit_log has no index, so with sequential scans off the scan stays,
    // disabled: 10¹⁰ more expensive before PostgreSQL 18, marked from 18.
    let sql = "SELECT * FROM audit_log WHERE action = 'login'";
    let off = [Setting::new("enable_seqscan", "off")];
    let forced = parse(
        db.explain_with(sql, Mode::Estimate, &off, Safety::default())
            .unwrap(),
    );
    let root = forced.root();
    assert_eq!(root.node_type, "Seq Scan");
    assert!(
        root.disabled
            || root.estimates.unwrap().startup_cost >= explainsql_core::compare::DISABLE_COST,
        "{root:?}"
    );
    // The setting ended with its transaction.
    let normal = parse(db.explain(sql, Mode::Estimate, Safety::default()).unwrap());
    assert!(!normal.root().disabled);
    assert!(normal.root().estimates.unwrap().startup_cost < 1.0);

    // Measured under a setting, after a run that warms the cache: the sort
    // that spills with the default work_mem stays in memory.
    let sort = "SELECT * FROM orders ORDER BY note";
    let spilled = parse(db.explain(sort, Mode::Analyze, Safety::default()).unwrap());
    assert_eq!(spilled.root().extra_str("Sort Space Type"), Some("Disk"));
    let runs = db
        .measure(
            sort,
            &[Setting::new("work_mem", "64MB")],
            2,
            Safety::default(),
        )
        .unwrap();
    assert_eq!(runs.len(), 2);
    for run in runs {
        assert_eq!(
            parse(run).root().extra_str("Sort Space Type"),
            Some("Memory")
        );
    }

    // Anything but a planner setting, or a value of the wrong type, is
    // refused before anything runs.
    for (name, value) in [
        ("statement_timeout", "0"),
        ("default_transaction_read_only", "off"),
        ("role", "postgres"),
        ("enable_seqscan", "off'; DROP TABLE orders; --"),
        ("work_mem", "64"),
    ] {
        assert!(
            matches!(
                db.explain_with(
                    sql,
                    Mode::Estimate,
                    &[Setting::new(name, value)],
                    Safety::default()
                ),
                Err(Error::Refused(_))
            ),
            "{name} = {value}"
        );
    }
    // Measuring a statement that writes still needs --allow-dml.
    assert!(matches!(
        db.measure("DELETE FROM audit_log", &[], 1, Safety::default()),
        Err(Error::NeedsAllowDml(_))
    ));
    assert_eq!(count(&db, "audit_log"), 5000.0);
}

/// Every condition of a plan, in one string.
fn conditions(plan: &Plan) -> String {
    plan.nodes
        .iter()
        .flat_map(|node| {
            node.predicates
                .iter()
                .map(|predicate| predicate.text.clone())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn plans_prepared_statements_as_applications_run_them() {
    let Some(db) = database() else { return };
    let parse = |json: String| explainsql_core::parse(&json).unwrap();
    let safety = Safety::default();
    let sql = "SELECT * FROM orders WHERE customer_id = $1 ORDER BY created_at DESC LIMIT $2";
    assert_eq!(
        db.parameter_types(sql, safety).unwrap(),
        ["integer", "bigint"]
    );
    let values = [Some("4242".to_owned()), Some("10".to_owned())];
    // The generic plan keeps the parameters, a custom plan has the values.
    let generic = parse(
        db.explain_prepared(sql, Cache::Generic, &values, &[], Mode::Estimate, safety)
            .unwrap(),
    );
    assert!(
        conditions(&generic).contains("$1"),
        "{}",
        conditions(&generic)
    );
    let custom = parse(
        db.explain_prepared(sql, Cache::Custom, &values, &[], Mode::Analyze, safety)
            .unwrap(),
    );
    assert!(
        conditions(&custom).contains("4242"),
        "{}",
        conditions(&custom)
    );
    assert!(custom.root().actuals.is_some());
    let runs = db
        .measure_prepared(sql, Cache::Generic, &values, 2, safety)
        .unwrap();
    assert_eq!(runs.len(), 2);
    assert!(parse(runs[1].clone()).root().actuals.is_some());

    // Values travel as literals in dollar quotes, whatever they hold.
    let tricky = "it's $v$ $$ \\ ;DROP TABLE orders";
    let plan = parse(
        db.explain_prepared(
            "SELECT * FROM orders WHERE note = $1",
            Cache::Custom,
            &[Some(tricky.to_owned())],
            &[],
            Mode::Analyze,
            safety,
        )
        .unwrap(),
    );
    assert!(
        conditions(&plan).contains("'it''s $v$ $$ \\ ;DROP TABLE orders'"),
        "{}",
        conditions(&plan)
    );
    // One statement only.
    assert!(
        db.explain_prepared(
            "SELECT $1::int; DROP TABLE orders",
            Cache::Custom,
            &[Some("1".to_owned())],
            &[],
            Mode::Estimate,
            safety,
        )
        .is_err()
    );
    // Writes need --allow-dml, and are rolled back.
    let delete = "DELETE FROM audit_log WHERE id > $1";
    assert!(matches!(
        db.explain_prepared(
            delete,
            Cache::Generic,
            &[Some("0".to_owned())],
            &[],
            Mode::Analyze,
            safety
        ),
        Err(Error::NeedsAllowDml(_))
    ));
    db.explain_prepared(
        delete,
        Cache::Generic,
        &[Some("0".to_owned())],
        &[],
        Mode::Analyze,
        allow_dml(),
    )
    .unwrap();
    assert_eq!(count(&db, "audit_log"), 5000.0);
    // Nothing stays prepared, whatever happened.
    let left = parse(
        db.explain(
            "SELECT * FROM pg_prepared_statements WHERE name LIKE 'explainsql%'",
            Mode::Analyze,
            safety,
        )
        .unwrap(),
    );
    assert_eq!(left.root().actuals.unwrap().rows, 0.0);

    // Under planner settings: without partition pruning, the generic plan
    // shows every partition, whatever the values.
    let events = "SELECT * FROM events WHERE created_at >= $1 AND created_at < $2";
    let march = [Some("2025-03-01".to_owned()), Some("2025-03-02".to_owned())];
    let scans = |plan: &Plan| {
        plan.nodes
            .iter()
            .filter(|node| node.node_type == "Bitmap Heap Scan")
            .count()
    };
    let pruned = parse(
        db.explain_prepared(events, Cache::Generic, &march, &[], Mode::Estimate, safety)
            .unwrap(),
    );
    assert_eq!(scans(&pruned), 1);
    let all = parse(
        db.explain_prepared(
            events,
            Cache::Generic,
            &march,
            &[Setting::new("enable_partition_pruning", "off")],
            Mode::Estimate,
            safety,
        )
        .unwrap(),
    );
    assert_eq!(scans(&all), 12);
}

#[test]
fn reads_column_statistics() {
    let Some(db) = database() else { return };
    let status = db
        .column_stats(Some("public"), "orders", "status")
        .unwrap()
        .unwrap();
    assert_eq!(status.common_values[0], "delivered");
    assert!((status.common_freqs[0] - 0.7).abs() < 0.05);
    let created = db
        .column_stats(None, "orders", "created_at")
        .unwrap()
        .unwrap();
    assert_eq!(created.histogram.len(), 101);
    assert!(created.histogram[0].starts_with("2024-01-01"));
    assert_eq!(
        db.column_stats(None, "orders", "no_such_column").unwrap(),
        None
    );
}

/// The costliest statements from pg_stat_statements, and the generic plan
/// of one with parameters, which runs nothing.
#[test]
fn reads_pg_stat_statements_and_plans_generically() {
    let Some(db) = database() else { return };
    let catalog = db.catalog(&[], &[]).unwrap();
    if !catalog
        .extensions
        .iter()
        .any(|name| name == "pg_stat_statements")
    {
        eprintln!("pg_stat_statements is not installed; skipping");
        return;
    }
    // Something to count: a statement with a constant, which
    // pg_stat_statements records with $1.
    for _ in 0..3 {
        db.explain(
            "SELECT id, amount FROM orders WHERE customer_id = 4242",
            Mode::Analyze,
            Safety::default(),
        )
        .unwrap();
    }
    // All of them: other tests' statements may take more time.
    let entries = db.statements(1000, Safety::default()).unwrap();
    assert!(!entries.is_empty());
    assert!(
        entries
            .windows(2)
            .all(|pair| pair[0].total_ms >= pair[1].total_ms)
    );
    let entry = entries
        .iter()
        .find(|entry| {
            entry.query.contains("FROM orders WHERE customer_id = $1")
                && entry.query.starts_with("EXPLAIN")
        })
        .or_else(|| {
            entries
                .iter()
                .find(|entry| entry.query.contains("customer_id = $1"))
        })
        .unwrap_or_else(|| panic!("{entries:#?}"));
    assert!(entry.calls >= 3, "{entry:?}");
    assert!(entries.iter().all(|entry| entry.queryid.is_some()));
    let shares: f64 = entries.iter().map(|entry| entry.share).sum();
    assert!(shares > 0.0 && shares <= 1.0 + 1e-9, "{shares}");

    let sql = "SELECT id, amount FROM orders WHERE customer_id = $1";
    if db.has_generic_plan() {
        let json = db.generic_plan(sql, Safety::default()).unwrap();
        let plan = explainsql_core::parse(&json).unwrap();
        assert!(plan.root().actuals.is_none(), "estimated: nothing ran");
        assert!(
            plan.nodes
                .iter()
                .any(|node| node.relation_name.as_deref() == Some("orders"))
        );
        // Utility commands have no plan.
        assert!(matches!(
            db.generic_plan("VACUUM orders", Safety::default()),
            Err(Error::Refused(_))
        ));
        // One statement only: the second is never run.
        let refused = db.generic_plan(
            "SELECT 1 FROM orders WHERE id = $1; UPDATE orders SET note = 'x'",
            Safety::default(),
        );
        assert!(
            matches!(&refused, Err(Error::Server(message)) if message.contains("multiple commands")),
            "{refused:?}"
        );
        // A statement that writes is planned, not run.
        let before = count(&db, "orders");
        let json = db
            .generic_plan(
                "UPDATE orders SET note = $1 WHERE customer_id = $2",
                Safety::default(),
            )
            .unwrap();
        let plan = explainsql_core::parse(&json).unwrap();
        assert_eq!(plan.root().node_type, "ModifyTable");
        assert_eq!(count(&db, "orders"), before);
    } else {
        assert!(matches!(
            db.generic_plan(sql, Safety::default()),
            Err(Error::Refused(message)) if message.contains("PostgreSQL 16")
        ));
    }
}

/// What reading pg_stat_statements needs. Without the extension in the
/// database (the same server's `postgres` database), it says what to run.
/// A role without `pg_read_all_stats`, `EXPLAINSQL_TEST_READER_URL`'s, sees
/// its own statements, and other roles' as statements it cannot plan; that
/// part is skipped without it.
#[test]
fn says_what_reading_statements_needs() {
    let Some(db) = database() else { return };
    let url = std::env::var("EXPLAINSQL_TEST_DATABASE_URL").unwrap();
    if let Some(other) = postgres_database(&url) {
        let postgres = Database::connect(&Settings::resolve(Some(&other)).unwrap()).unwrap();
        match postgres.statements(10, Safety::default()) {
            Err(Error::Refused(message)) => assert!(
                message.contains("CREATE EXTENSION pg_stat_statements"),
                "{message}"
            ),
            Ok(_) => eprintln!("pg_stat_statements is in the postgres database too"),
            Err(error) => panic!("{error}"),
        }
    }

    let Ok(url) = std::env::var("EXPLAINSQL_TEST_READER_URL") else {
        eprintln!("EXPLAINSQL_TEST_READER_URL is not set; skipping");
        return;
    };
    let reader = Database::connect(&Settings::resolve(Some(&url)).unwrap()).unwrap();
    // Statements of each role: the reader's first read is one of its own.
    db.explain(
        "SELECT count(*) FROM customers",
        Mode::Estimate,
        Safety::default(),
    )
    .unwrap();
    match reader.statements(1000, Safety::default()) {
        Err(Error::Refused(message)) if message.contains("not installed") => {
            eprintln!("pg_stat_statements is not installed; skipping");
            return;
        }
        read => read.unwrap(),
    };
    let entries = reader.statements(1000, Safety::default()).unwrap();
    let hidden: Vec<_> = entries
        .iter()
        .filter(|entry| entry.query == top::HIDDEN)
        .collect();
    assert!(!hidden.is_empty(), "{entries:#?}");
    for entry in hidden {
        assert!(entry.queryid.is_none(), "{entry:?}");
        let reason = entry.unplannable.as_deref().unwrap();
        assert!(reason.contains("pg_read_all_stats"), "{reason}");
    }
    assert!(
        entries
            .iter()
            .any(|entry| entry.query.contains("pg_stat_statements") && entry.queryid.is_some()),
        "{entries:#?}"
    );
}

/// The same server's `postgres` database, for a URL.
fn postgres_database(url: &str) -> Option<String> {
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let (server, _) = base.rsplit_once('/')?;
    server
        .contains("://")
        .then(|| format!("{server}/postgres?{query}"))
}

/// Another session: after `delay`, it runs `sql` in a transaction, keeps
/// the transaction open for `hold` and rolls it back. Returns its PID, and
/// a receiver told once `sql` has run.
fn other_session(
    delay: Duration,
    sql: &'static str,
    hold: Duration,
) -> (
    i32,
    std::sync::mpsc::Receiver<()>,
    std::thread::JoinHandle<()>,
) {
    let url = std::env::var("EXPLAINSQL_TEST_DATABASE_URL").unwrap();
    let (pid_sender, pid) = std::sync::mpsc::channel();
    let (ran_sender, ran) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .unwrap();
            tokio::spawn(connection);
            let pid: i32 = client
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get(0);
            pid_sender.send(pid).unwrap();
            tokio::time::sleep(delay).await;
            client.batch_execute("BEGIN").await.unwrap();
            client.batch_execute(sql).await.unwrap();
            let _ = ran_sender.send(());
            tokio::time::sleep(hold).await;
            client.batch_execute("ROLLBACK").await.unwrap();
        });
    });
    (pid.recv().unwrap(), ran, thread)
}

#[test]
fn reads_the_locks_a_statement_takes() {
    use explainsql_core::locks::{self, Stage};
    let Some(db) = database() else { return };
    // now() is not a constant to the planner: it plans every partition,
    // with its index, and run-time pruning keeps one or none.
    let sql = "SELECT * FROM events WHERE created_at > now() - interval '1 day'";
    for (mode, stage) in [
        (Mode::Estimate, Stage::Planned),
        (Mode::Analyze, Stage::Ran),
    ] {
        let (json, capture) = db.explain_locks(sql, mode, Safety::default()).unwrap();
        let capture = capture.unwrap();
        assert_eq!(capture.stage, stage);
        let plan = explainsql_core::parse(&json).unwrap();
        let footprint = locks::footprint(&capture, &plan);
        // The table, 12 partitions and 13 indexes, 10 past the 16 slots.
        assert_eq!(footprint.relation_locks, 26, "{footprint:#?}");
        assert_eq!(footprint.outside_fast_path, 10, "{footprint:#?}");
        assert_eq!(footprint.tables.len(), 1);
        assert_eq!(footprint.tables[0].table, "events");
        assert_eq!(footprint.tables[0].partitions, 12);
        assert_eq!(footprint.tables[0].indexes, 13);
        // Only the statement's: nothing of the transaction or of the
        // queries that read them.
        assert!(footprint.other.is_empty(), "{:?}", footprint.other);
        assert!(
            capture
                .held
                .iter()
                .all(|lock| lock
                    .relation
                    .is_none_or(|oid| capture.relations.iter().any(|relation| relation.oid
                        == oid
                        && !relation.schema.starts_with("pg_"))))
        );
    }

    // A write: its table's lock, and its transaction ID.
    let (json, capture) = db
        .explain_locks(
            "UPDATE orders SET note = note WHERE id = 1",
            Mode::Analyze,
            allow_dml(),
        )
        .unwrap();
    let footprint = locks::footprint(&capture.unwrap(), &explainsql_core::parse(&json).unwrap());
    assert_eq!(footprint.tables[0].mode.name(), "RowExclusiveLock");
    assert!(
        footprint
            .other
            .iter()
            .any(|other| other.contains("transaction ID")),
        "{:?}",
        footprint.other
    );
    assert!(
        footprint.blocked_by[0].contains("CREATE INDEX without CONCURRENTLY"),
        "{:?}",
        footprint.blocked_by
    );
}

#[test]
fn reads_the_locks_of_generic_and_custom_plans() {
    use explainsql_core::locks::{self, Stage};
    let Some(db) = database() else { return };
    let sql = "SELECT * FROM events WHERE created_at > $1";
    let values = [Some("2025-12-20".to_owned())];
    let mut footprints = Vec::new();
    for (cache, mode) in [
        (Cache::Generic, Mode::Analyze),
        (Cache::Custom, Mode::Estimate),
    ] {
        let (json, capture) = db
            .prepared_locks(sql, cache, &values, mode, Safety::default())
            .unwrap();
        footprints.push(locks::footprint(
            &capture,
            &explainsql_core::parse(&json).unwrap(),
        ));
    }
    let (generic, custom) = (&footprints[0], &footprints[1]);
    assert_eq!(generic.stage, Stage::Generic);
    // Each execution of the generic plan locks every partition, and the
    // index of the one it reads; a custom plan, the partition it reads
    // and its indexes.
    assert_eq!(generic.tables[0].partitions, 12, "{generic:#?}");
    assert_eq!(generic.tables[0].partitions_in_plan, Some(1));
    assert_eq!(custom.stage, Stage::Custom);
    assert_eq!(custom.tables[0].partitions, 1, "{custom:#?}");
    assert!(custom.relation_locks < generic.relation_locks);
    locks::compare_executions(&mut footprints);
    assert!(
        footprints[0].notes.iter().any(|note| note
            .summary
            .contains("a custom plan for the same values takes")),
        "{:#?}",
        footprints[0].notes
    );
}

#[test]
fn watches_what_a_run_waits_for() {
    let Some(db) = database() else { return };
    db.watch_waits();
    let update = "UPDATE orders SET note = note WHERE id = 1";
    // Another session holds the row for a while: the run waits for it.
    let (pid, ran, other) = other_session(Duration::ZERO, update, Duration::from_millis(400));
    ran.recv().unwrap();
    let (_, capture) = db
        .explain_locks(update, Mode::Analyze, allow_dml())
        .unwrap();
    other.join().unwrap();
    let waits = capture.unwrap().waits.unwrap();
    assert!(waits.lock_samples() > 0, "{waits:#?}");
    assert!(
        waits.blockers.iter().any(|blocker| blocker.pid == pid),
        "{waits:#?}"
    );
    assert!(db.take_notes().is_empty());

    // Another session asks for a lock that conflicts with the statement's
    // while it runs, and waits behind it.
    let (pid, _, other) = other_session(
        Duration::from_millis(150),
        "SET LOCAL lock_timeout = '5s'; LOCK TABLE orders IN ACCESS EXCLUSIVE MODE",
        Duration::ZERO,
    );
    let (json, capture) = db
        .explain_locks(
            "SELECT pg_sleep(0.6), id FROM orders WHERE id = 1",
            Mode::Analyze,
            Safety::default(),
        )
        .unwrap();
    other.join().unwrap();
    let footprint = explainsql_core::locks::footprint(
        &capture.unwrap(),
        &explainsql_core::parse(&json).unwrap(),
    );
    assert!(
        footprint
            .conflicts
            .iter()
            .any(|other| other.pid == Some(pid) && !other.granted),
        "{footprint:#?}"
    );
}
