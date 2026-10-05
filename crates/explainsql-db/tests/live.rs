//! Against a real database holding `fixtures/schema.sql`, named by
//! `EXPLAINSQL_TEST_DATABASE_URL`; skipped without it. The CI's `db` job
//! runs them.
//!
//! The central promise: whatever explainsql runs is rolled back. Data-
//! modifying statements run only with `--allow-dml`, and leave the data as
//! it was.

use std::time::Duration;

use explainsql_core::scenario::Setting;
use explainsql_db::{Database, Error, Mode, Safety, Settings, Writes};

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
