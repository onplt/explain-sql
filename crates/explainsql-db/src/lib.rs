//! Connected mode for ExplainSQL: running `EXPLAIN` safely, always inside a
//! transaction that is rolled back, and reading catalog and statistics data
//! for the relations in a plan.
//!
//! The API is blocking: a Tokio runtime runs the connection in the
//! background, so that the viewer and the command-line tool stay
//! synchronous. A [`Canceller`] stops a running statement from another
//! thread.

mod catalog;
pub mod conn;
mod exec;
mod locks;
mod prepared;
mod prove;
mod stats;
mod tls;

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use explainsql_core::catalog::Catalog;
use explainsql_core::locks::{Capture, Stage};
use explainsql_core::params::ColumnStats;
use explainsql_core::scenario::Setting;
use tokio::runtime::Runtime;
use tokio_postgres::{CancelToken, Client};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::exec::Observe;
use crate::locks::Watch;

pub use conn::Settings;
pub use exec::{Mode, Writes};
pub use prepared::Cache;
pub use prove::Proof;

/// What can go wrong, in words for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Could not connect.
    Connect(String),
    /// explainsql will not run this statement.
    Refused(String),
    /// The statement modifies data or locks rows; `--allow-dml` runs it
    /// anyway, rolled back.
    NeedsAllowDml(String),
    /// `statement_timeout` stopped the statement.
    Timeout(Duration),
    /// Cancelled by the user.
    Cancelled,
    /// An error from the server, with its SQLSTATE.
    Server(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Connect(message) => write!(f, "cannot connect: {message}"),
            Error::Refused(message) => f.write_str(message),
            Error::NeedsAllowDml(reason) => write!(
                f,
                "the statement {reason}. EXPLAIN ANALYZE would run it; with --allow-dml it runs inside a transaction that is rolled back"
            ),
            Error::Timeout(timeout) => write!(
                f,
                "the statement ran longer than the {} s timeout (--timeout) and was stopped",
                timeout.as_secs_f64()
            ),
            Error::Cancelled => f.write_str("cancelled: the statement was stopped and rolled back"),
            Error::Server(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

/// How explainsql may run a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Safety {
    /// Run statements that modify data or lock rows (always rolled back).
    pub allow_dml: bool,
    /// Build a suggested index to measure it (always rolled back).
    pub allow_ddl: bool,
    /// `statement_timeout` for each run.
    pub timeout: Duration,
}

impl Default for Safety {
    fn default() -> Self {
        Safety {
            allow_dml: false,
            allow_ddl: false,
            timeout: Duration::from_secs(30),
        }
    }
}

/// A connection to a database.
pub struct Database {
    runtime: Runtime,
    client: Arc<Client>,
    canceller: Canceller,
    server_version: u32,
    description: String,
    settings: Settings,
    watching: Mutex<Watching>,
    /// What the user should know about runs, such as a measured run that
    /// waited for a lock: see [`Database::take_notes`].
    notes: Mutex<Vec<String>>,
}

/// Whether a second connection watches what statements wait on.
enum Watching {
    Off,
    /// Asked for; the connection opens with the first run it watches.
    Wanted,
    On(Watch),
    /// The connection could not be opened.
    Failed,
}

/// Stops the statement a [`Database`] is running, from any thread.
#[derive(Clone)]
pub struct Canceller {
    handle: tokio::runtime::Handle,
    token: CancelToken,
    tls: MakeRustlsConnect,
    cancelled: Arc<AtomicBool>,
}

impl Canceller {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let (token, tls) = (self.token.clone(), self.tls.clone());
        self.handle.spawn(async move {
            let _ = token.cancel_query(tls).await;
        });
    }
}

impl Database {
    pub fn connect(settings: &Settings) -> Result<Self, Error> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("explainsql-db")
            .enable_all()
            .build()
            .map_err(|error| Error::Connect(error.to_string()))?;
        let (client, tls) = open(&runtime, settings)?;
        let version: String = runtime
            .block_on(client.query_one("SELECT current_setting('server_version_num')", &[]))
            .map_err(|error| Error::Connect(describe(&error)))?
            .get(0);
        let server_version = version.parse().unwrap_or(0);
        if server_version < 90_600 {
            return Err(Error::Connect(format!(
                "PostgreSQL {version} is too old: explainsql needs 9.6 or later"
            )));
        }
        let canceller = Canceller {
            handle: runtime.handle().clone(),
            token: client.cancel_token(),
            tls,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        Ok(Database {
            runtime,
            client: Arc::new(client),
            canceller,
            server_version,
            description: settings.describe(),
            settings: settings.clone(),
            watching: Mutex::new(Watching::Off),
            notes: Mutex::new(Vec::new()),
        })
    }

    /// Watches what statements wait on from a second connection, opened
    /// with the first run it watches: the runs [`Database::explain_locks`]
    /// and [`Database::prepared_locks`] read locks of, and measured runs,
    /// which run again when they waited for another session's lock.
    pub fn watch_waits(&self) {
        let mut watching = self
            .watching
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if matches!(*watching, Watching::Off) {
            *watching = Watching::Wanted;
        }
    }

    /// The watching connection, opened if it is wanted and not yet open.
    fn watch(&self) -> Option<Watch> {
        let mut watching = self
            .watching
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if matches!(*watching, Watching::Wanted) {
            *watching = match open(&self.runtime, &self.settings) {
                Ok((client, _)) => Watching::On(Watch {
                    client: Arc::new(client),
                    server_version: self.server_version,
                }),
                Err(error) => {
                    self.note(vec![format!(
                        "cannot watch what statements wait on from a second connection: {error}"
                    )]);
                    Watching::Failed
                }
            };
        }
        match &*watching {
            Watching::On(watch) => Some(watch.clone()),
            _ => None,
        }
    }

    fn note(&self, notes: Vec<String>) {
        self.notes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend(notes);
    }

    /// What the user should know about the runs since the last call: a
    /// measured run that waited for another session's lock and ran again,
    /// or a watching connection that could not be opened.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().unwrap_or_else(|error| error.into_inner()))
    }

    /// `server_version_num`: 160004 for 16.4.
    pub fn server_version(&self) -> u32 {
        self.server_version
    }

    /// `user@host:port/dbname`.
    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn canceller(&self) -> Canceller {
        self.canceller.clone()
    }

    /// The plan of a statement as JSON: estimated, or measured with
    /// `EXPLAIN ANALYZE` inside a transaction that is always rolled back.
    pub fn explain(&self, sql: &str, mode: Mode, safety: Safety) -> Result<String, Error> {
        self.explain_with(sql, mode, &[], safety)
    }

    /// The plan of a statement under planner settings, such as
    /// `enable_seqscan = off`. They hold only inside the transaction that is
    /// rolled back; settings that are not planner settings are refused.
    pub fn explain_with(
        &self,
        sql: &str,
        mode: Mode,
        settings: &[Setting],
        safety: Safety,
    ) -> Result<String, Error> {
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let result = self.runtime.block_on(exec::explain(
            &self.client,
            sql,
            mode,
            settings,
            safety,
            self.server_version,
        ));
        self.classify(result, safety)
    }

    /// The plan of a statement, as [`Database::explain`] makes it, and the
    /// locks its last run took, read before the rollback: those planning
    /// takes, or with `EXPLAIN ANALYZE` planning and running. `None` when
    /// they could not be read. Watched ([`Database::watch_waits`]), they
    /// come with what the run waited on.
    pub fn explain_locks(
        &self,
        sql: &str,
        mode: Mode,
        safety: Safety,
    ) -> Result<(String, Option<Capture>), Error> {
        let watch = self.watch();
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let observe = Observe {
            locks: Some(match mode {
                Mode::Estimate => Stage::Planned,
                Mode::Analyze => Stage::Ran,
            }),
            watch: watch.as_ref().filter(|_| mode == Mode::Analyze),
            server_version: self.server_version,
        };
        let result = self.runtime.block_on(exec::explain_observed(
            &self.client,
            sql,
            mode,
            &[],
            safety,
            observe,
        ));
        self.classify(result, safety)
            .map(|(plan, observed)| (plan, observed.locks))
    }

    /// `runs` measured plans of a statement under planner settings, after a
    /// run that only warms the cache. Each run is rolled back.
    pub fn measure(
        &self,
        sql: &str,
        settings: &[Setting],
        runs: usize,
        safety: Safety,
    ) -> Result<Vec<String>, Error> {
        let watch = self.watch();
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let mut notes = Vec::new();
        let result = self.runtime.block_on(exec::measure(
            &self.client,
            sql,
            settings,
            runs,
            safety,
            self.server_version,
            watch.as_ref(),
            &mut notes,
        ));
        self.note(notes);
        self.classify(result, safety)
    }

    /// Whether a statement modifies data or locks rows, from its estimated
    /// plan.
    pub fn writes(&self, sql: &str, safety: Safety) -> Result<Writes, Error> {
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let result =
            self.runtime
                .block_on(exec::writes(&self.client, sql, safety, self.server_version));
        self.classify(result, safety)
    }

    /// The statement's plan without and with a suggested index: estimated
    /// with a HypoPG hypothetical index, or with `measured`, built for real
    /// in a transaction that is rolled back and run with EXPLAIN ANALYZE
    /// `runs` times on each side, after a run that only warms the cache.
    pub fn prove(
        &self,
        sql: &str,
        ddl: &str,
        measured: bool,
        runs: usize,
        safety: Safety,
    ) -> Result<Proof, Error> {
        let watch = if measured { self.watch() } else { None };
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let mut notes = Vec::new();
        let result = if measured {
            self.runtime.block_on(prove::measured(
                &self.client,
                sql,
                ddl,
                runs,
                safety,
                self.server_version,
                watch.as_ref(),
                &mut notes,
            ))
        } else {
            self.runtime.block_on(prove::hypothetical(
                &self.client,
                sql,
                ddl,
                safety,
                self.server_version,
            ))
        };
        self.note(notes);
        self.classify(result, safety)
    }

    /// The plan of a statement with parameters (`$1`, `$2`, …), prepared
    /// and executed with `values` as an application runs it: under
    /// `plan_cache_mode`, the generic plan, for any value, or a custom plan,
    /// for these values, and under planner settings. Estimated, or measured
    /// with EXPLAIN ANALYZE inside a transaction that is always rolled back.
    /// Needs PostgreSQL 12.
    pub fn explain_prepared(
        &self,
        sql: &str,
        cache: Cache,
        values: &[Option<String>],
        settings: &[Setting],
        mode: Mode,
        safety: Safety,
    ) -> Result<String, Error> {
        self.plan_cache_mode()?;
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let result = self.runtime.block_on(prepared::explain(
            &self.client,
            sql,
            cache,
            values,
            settings,
            mode,
            safety,
            self.server_version,
        ));
        self.classify(result, safety)
    }

    /// `runs` measured plans of a statement with parameters, prepared and
    /// executed with `values` under `plan_cache_mode`, after a run that
    /// only warms the cache. Each run is rolled back.
    pub fn measure_prepared(
        &self,
        sql: &str,
        cache: Cache,
        values: &[Option<String>],
        runs: usize,
        safety: Safety,
    ) -> Result<Vec<String>, Error> {
        self.plan_cache_mode()?;
        let watch = self.watch();
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let mut notes = Vec::new();
        let result = self.runtime.block_on(prepared::measure(
            &self.client,
            sql,
            cache,
            values,
            runs,
            safety,
            self.server_version,
            watch.as_ref(),
            &mut notes,
        ));
        self.note(notes);
        self.classify(result, safety)
    }

    /// The plan of one execution of a statement with parameters, prepared
    /// and executed with `values` under `plan_cache_mode`, and the locks
    /// that execution took. The plan is made in a first transaction, and
    /// the locks read in a second: those of an execution of the cached
    /// generic plan, which PostgreSQL does not plan again, or of a custom
    /// plan, planned for each execution. Estimated, or with
    /// `EXPLAIN ANALYZE` run, and always rolled back.
    pub fn prepared_locks(
        &self,
        sql: &str,
        cache: Cache,
        values: &[Option<String>],
        mode: Mode,
        safety: Safety,
    ) -> Result<(String, Capture), Error> {
        self.plan_cache_mode()?;
        let watch = self.watch();
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let result = self.runtime.block_on(prepared::locks(
            &self.client,
            sql,
            cache,
            values,
            mode,
            safety,
            self.server_version,
            watch.as_ref().filter(|_| mode == Mode::Analyze),
        ));
        self.classify(result, safety)
    }

    /// The types PostgreSQL infers for a statement's parameters, as
    /// `format_type` names them: `integer`, `timestamp with time zone`.
    pub fn parameter_types(&self, sql: &str, safety: Safety) -> Result<Vec<String>, Error> {
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let result =
            self.runtime
                .block_on(prepared::parameter_types(&self.client, sql, safety.timeout));
        self.classify(result, safety)
    }

    /// What `pg_stats` says about a column: `None` when it has no
    /// statistics, or values that do not read as a list.
    pub fn column_stats(
        &self,
        schema: Option<&str>,
        table: &str,
        column: &str,
    ) -> Result<Option<ColumnStats>, Error> {
        let safety = Safety::default();
        let result = self.runtime.block_on(prepared::column_stats(
            &self.client,
            schema,
            table,
            column,
            safety.timeout,
        ));
        self.classify(result, safety)
    }

    /// `plan_cache_mode`, which chooses the plan of a prepared statement,
    /// came with PostgreSQL 12.
    fn plan_cache_mode(&self) -> Result<(), Error> {
        if self.server_version < 120_000 {
            return Err(Error::Refused(
                "trying a statement's parameters needs PostgreSQL 12 or later, for plan_cache_mode"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// The `limit` statements of this database with the most execution
    /// time, from pg_stat_statements.
    pub fn statements(
        &self,
        limit: usize,
        safety: Safety,
    ) -> Result<Vec<explainsql_core::top::Entry>, Error> {
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let result = self.runtime.block_on(stats::statements(
            &self.client,
            limit,
            safety,
            self.server_version,
        ));
        self.classify(result, safety)
    }

    /// Whether [`Database::generic_plan`] can plan statements with
    /// parameters: `EXPLAIN (GENERIC_PLAN)` came with PostgreSQL 16.
    pub fn has_generic_plan(&self) -> bool {
        self.server_version >= 160_000
    }

    /// The generic plan of a statement with parameters, as JSON, estimated
    /// and without values: nothing runs.
    pub fn generic_plan(&self, sql: &str, safety: Safety) -> Result<String, Error> {
        if !self.has_generic_plan() {
            return Err(Error::Refused(
                "planning a statement with parameters without their values needs PostgreSQL 16 or later, for EXPLAIN (GENERIC_PLAN)".to_owned(),
            ));
        }
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let result = self.runtime.block_on(exec::generic(
            &self.client,
            sql,
            safety,
            self.server_version,
        ));
        self.classify(result, safety)
    }

    /// What the catalog says about the tables and foreign keys a plan and
    /// its advice involve.
    pub fn catalog(
        &self,
        tables: &[(Option<String>, String)],
        constraints: &[String],
    ) -> Result<Catalog, Error> {
        let safety = Safety::default();
        let result =
            self.runtime
                .block_on(catalog::read(&self.client, tables, constraints, safety));
        self.classify(result, safety)
    }

    /// The foreign keys of a single column from or to `table.column`.
    pub fn references(
        &self,
        schema: Option<&str>,
        table: &str,
        column: &str,
    ) -> Result<Vec<explainsql_core::requests::Reference>, Error> {
        let safety = Safety::default();
        let result = self.runtime.block_on(catalog::references(
            &self.client,
            schema,
            table,
            column,
            safety,
        ));
        self.classify(result, safety)
    }

    /// The median time of a round trip to the server, `SELECT 1` sent and
    /// its result back, over `samples` runs after one that is left out.
    pub fn round_trip(&self, samples: usize) -> Result<Duration, Error> {
        let client = &self.client;
        let result = self.runtime.block_on(async {
            let mut times = Vec::with_capacity(samples);
            for warm_up in std::iter::once(true).chain(std::iter::repeat_n(false, samples.max(1))) {
                let start = std::time::Instant::now();
                client
                    .simple_query("SELECT 1")
                    .await
                    .map_err(|error| Error::Server(describe(&error)))?;
                if !warm_up {
                    times.push(start.elapsed());
                }
            }
            times.sort();
            Ok(times[times.len() / 2])
        });
        self.classify(result, Safety::default())
    }

    /// Turns a query cancellation into a timeout or a cancellation.
    fn classify<T>(&self, result: Result<T, Error>, safety: Safety) -> Result<T, Error> {
        match result {
            Err(Error::Server(message)) if message.contains("57014") => {
                if self.canceller.cancelled.swap(false, Ordering::SeqCst) {
                    Err(Error::Cancelled)
                } else {
                    Err(Error::Timeout(safety.timeout))
                }
            }
            other => other,
        }
    }
}

/// Opens a connection, run in the background by the runtime.
fn open(runtime: &Runtime, settings: &Settings) -> Result<(Client, MakeRustlsConnect), Error> {
    let config = settings.config().map_err(Error::Connect)?;
    let tls = tls::connector(settings).map_err(Error::Connect)?;
    let (client, connection) = runtime
        .block_on(config.connect(tls.clone()))
        .map_err(|error| {
            Error::Connect(format!("{}: {}", settings.describe(), describe(&error)))
        })?;
    runtime.spawn(async move {
        let _ = connection.await;
    });
    Ok((client, tls))
}

/// A database error with its SQLSTATE, detail and hint.
pub(crate) fn describe(error: &tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => {
            let mut text = format!("{} ({})", db.message(), db.code().code());
            if let Some(detail) = db.detail() {
                text.push_str(&format!("; {detail}"));
            }
            if let Some(hint) = db.hint() {
                text.push_str(&format!("; hint: {hint}"));
            }
            text
        }
        None => {
            let mut text = error.to_string();
            let mut source = std::error::Error::source(error);
            while let Some(cause) = source {
                text.push_str(&format!(": {cause}"));
                source = cause.source();
            }
            text
        }
    }
}
