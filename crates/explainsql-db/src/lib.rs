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
mod prove;
mod tls;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use explainsql_core::catalog::Catalog;
use tokio::runtime::Runtime;
use tokio_postgres::{CancelToken, Client};
use tokio_postgres_rustls::MakeRustlsConnect;

pub use conn::Settings;
pub use exec::{Mode, Writes};
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
        let config = settings.config().map_err(Error::Connect)?;
        let tls = tls::connector(settings).map_err(Error::Connect)?;
        let (client, connection) =
            runtime
                .block_on(config.connect(tls.clone()))
                .map_err(|error| {
                    Error::Connect(format!("{}: {}", settings.describe(), describe(&error)))
                })?;
        runtime.spawn(async move {
            let _ = connection.await;
        });
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
        })
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
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let result = self.runtime.block_on(exec::explain(
            &self.client,
            sql,
            mode,
            safety,
            self.server_version,
        ));
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
    /// in a transaction that is rolled back and run with EXPLAIN ANALYZE.
    pub fn prove(
        &self,
        sql: &str,
        ddl: &str,
        measured: bool,
        safety: Safety,
    ) -> Result<Proof, Error> {
        self.canceller.cancelled.store(false, Ordering::SeqCst);
        let result = if measured {
            self.runtime.block_on(prove::measured(
                &self.client,
                sql,
                ddl,
                safety,
                self.server_version,
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
