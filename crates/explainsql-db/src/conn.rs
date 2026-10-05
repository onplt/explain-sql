//! Connection settings the way libpq reads them, so that whatever `psql`
//! connects to, `explainsql` connects to as well: a URL
//! (`postgresql://user@host/db`), a `key=value` string or a bare database
//! name, then the service file, then the `PG*` environment variables, then
//! the defaults, and the password from the password file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::{env, fs};

/// The settings explainsql understands, by their libpq names.
const KEYS: [&str; 10] = [
    "host",
    "hostaddr",
    "port",
    "dbname",
    "user",
    "password",
    "sslmode",
    "sslrootcert",
    "application_name",
    "connect_timeout",
];

/// The environment variable for each setting.
const ENVIRONMENT: [(&str, &str); 10] = [
    ("host", "PGHOST"),
    ("hostaddr", "PGHOSTADDR"),
    ("port", "PGPORT"),
    ("dbname", "PGDATABASE"),
    ("user", "PGUSER"),
    ("password", "PGPASSWORD"),
    ("sslmode", "PGSSLMODE"),
    ("sslrootcert", "PGSSLROOTCERT"),
    ("application_name", "PGAPPNAME"),
    ("connect_timeout", "PGCONNECT_TIMEOUT"),
];

/// Resolved connection settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    values: BTreeMap<String, String>,
}

/// What `sslmode` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    /// Encrypt when the server supports it, without checking its certificate.
    Prefer,
    /// Encrypt, without checking the certificate.
    Require,
    /// Encrypt, and check that a trusted authority signed the certificate.
    VerifyCa,
    /// Also check that the certificate names the host.
    VerifyFull,
}

/// Where the settings come from, so that tests can replace the
/// environment and the home directory.
pub struct Sources<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub home: Option<PathBuf>,
}

impl Sources<'_> {
    pub fn system() -> Sources<'static> {
        Sources {
            env: &|name| env::var(name).ok().filter(|value| !value.is_empty()),
            home: home_dir(),
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os(if cfg!(windows) { "APPDATA" } else { "HOME" }).map(PathBuf::from)
}

impl Settings {
    /// Resolves the settings from what the user gave (`None` for nothing)
    /// and the system.
    pub fn resolve(given: Option<&str>) -> Result<Self, String> {
        Self::resolve_with(given, &Sources::system())
    }

    pub fn resolve_with(given: Option<&str>, sources: &Sources) -> Result<Self, String> {
        let explicit = match given.map(str::trim).filter(|given| !given.is_empty()) {
            None => BTreeMap::new(),
            Some(given)
                if given.starts_with("postgresql://") || given.starts_with("postgres://") =>
            {
                parse_url(given)?
            }
            Some(given) if given.contains('=') => parse_conninfo(given)?,
            Some(name) => BTreeMap::from([("dbname".to_owned(), name.to_owned())]),
        };
        let mut values = BTreeMap::new();
        // Lowest first: the environment, then the service file, then what
        // the user gave.
        for (key, variable) in ENVIRONMENT {
            if let Some(value) = (sources.env)(variable) {
                values.insert(key.to_owned(), value);
            }
        }
        let service = explicit
            .get("service")
            .cloned()
            .or_else(|| (sources.env)("PGSERVICE"));
        if let Some(service) = service {
            values.extend(read_service(&service, sources)?);
        }
        values.extend(explicit);
        values.remove("service");

        if !values.contains_key("user") {
            let user = (sources.env)("USER")
                .or_else(|| (sources.env)("USERNAME"))
                .unwrap_or_else(|| "postgres".to_owned());
            values.insert("user".to_owned(), user);
        }
        if !values.contains_key("dbname") {
            values.insert("dbname".to_owned(), values["user"].clone());
        }
        if !values.contains_key("host") && !values.contains_key("hostaddr") {
            values.insert("host".to_owned(), default_host());
        }
        values
            .entry("port".to_owned())
            .or_insert_with(|| "5432".to_owned());
        values
            .entry("application_name".to_owned())
            .or_insert_with(|| "explainsql".to_owned());
        if !values.contains_key("password") {
            if let Some(password) = read_password(&values, sources) {
                values.insert("password".to_owned(), password);
            }
        }
        let settings = Settings { values };
        settings.ssl_mode()?;
        settings.port()?;
        Ok(settings)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn host(&self) -> &str {
        self.get("host")
            .or(self.get("hostaddr"))
            .unwrap_or("localhost")
    }

    pub fn port(&self) -> Result<u16, String> {
        let port = self.get("port").unwrap_or("5432");
        port.parse()
            .map_err(|_| format!("invalid port `{port}`: expected a number"))
    }

    pub fn ssl_mode(&self) -> Result<SslMode, String> {
        match self.get("sslmode").unwrap_or("prefer") {
            "disable" | "allow" => Ok(SslMode::Disable),
            "prefer" => Ok(SslMode::Prefer),
            "require" => Ok(SslMode::Require),
            "verify-ca" => Ok(SslMode::VerifyCa),
            "verify-full" => Ok(SslMode::VerifyFull),
            other => Err(format!(
                "invalid sslmode `{other}`: expected disable, prefer, require, verify-ca or verify-full"
            )),
        }
    }

    /// `user@host:port/dbname`, for messages. Never the password.
    pub fn describe(&self) -> String {
        format!(
            "{}@{}:{}/{}",
            self.get("user").unwrap_or(""),
            self.host(),
            self.get("port").unwrap_or("5432"),
            self.get("dbname").unwrap_or("")
        )
    }

    /// The settings for tokio-postgres.
    pub fn config(&self) -> Result<tokio_postgres::Config, String> {
        let mut config = tokio_postgres::Config::new();
        for host in self.host().split(',') {
            #[cfg(unix)]
            if host.starts_with('/') {
                config.host_path(host);
                continue;
            }
            config.host(host);
        }
        config.port(self.port()?);
        if let Some(user) = self.get("user") {
            config.user(user);
        }
        if let Some(dbname) = self.get("dbname") {
            config.dbname(dbname);
        }
        if let Some(password) = self.get("password") {
            config.password(password);
        }
        if let Some(name) = self.get("application_name") {
            config.application_name(name);
        }
        if let Some(timeout) = self.get("connect_timeout") {
            let seconds: u64 = timeout
                .parse()
                .map_err(|_| format!("invalid connect_timeout `{timeout}`"))?;
            if seconds > 0 {
                config.connect_timeout(std::time::Duration::from_secs(seconds));
            }
        }
        config.ssl_mode(match self.ssl_mode()? {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => {
                tokio_postgres::config::SslMode::Require
            }
        });
        Ok(config)
    }
}

/// libpq's default: the Unix socket directory, or localhost.
fn default_host() -> String {
    if cfg!(unix) {
        for directory in ["/var/run/postgresql", "/tmp"] {
            if Path::new(directory).join(".s.PGSQL.5432").exists() {
                return directory.to_owned();
            }
        }
    }
    "localhost".to_owned()
}

/// `postgresql://[user[:password]@][host][:port][/dbname][?key=value&…]`.
fn parse_url(url: &str) -> Result<BTreeMap<String, String>, String> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let (rest, query) = match rest.split_once('?') {
        Some((rest, query)) => (rest, Some(query)),
        None => (rest, None),
    };
    let (authority, dbname) = match rest.split_once('/') {
        Some((authority, dbname)) => (authority, Some(dbname)),
        None => (rest, None),
    };
    let mut values = BTreeMap::new();
    let hosts = match authority.rsplit_once('@') {
        Some((credentials, hosts)) => {
            let (user, password) = match credentials.split_once(':') {
                Some((user, password)) => (user, Some(password)),
                None => (credentials, None),
            };
            if !user.is_empty() {
                values.insert("user".to_owned(), decode(user)?);
            }
            if let Some(password) = password {
                values.insert("password".to_owned(), decode(password)?);
            }
            hosts
        }
        None => authority,
    };
    let (mut hosts_list, mut ports) = (Vec::new(), Vec::new());
    for host in hosts.split(',').filter(|host| !host.is_empty()) {
        // An IPv6 address in brackets may contain colons.
        let (host, port) = if let Some(inner) = host.strip_prefix('[') {
            let (address, after) = inner
                .split_once(']')
                .ok_or_else(|| format!("invalid host `{host}` in the URL"))?;
            (address.to_owned(), after.strip_prefix(':'))
        } else {
            match host.split_once(':') {
                Some((host, port)) => (decode(host)?, Some(port)),
                None => (decode(host)?, None),
            }
        };
        hosts_list.push(host);
        if let Some(port) = port {
            ports.push(port.to_owned());
        }
    }
    if !hosts_list.is_empty() {
        values.insert("host".to_owned(), hosts_list.join(","));
    }
    if let Some(port) = ports.first() {
        values.insert("port".to_owned(), port.clone());
    }
    if let Some(dbname) = dbname.filter(|dbname| !dbname.is_empty()) {
        values.insert("dbname".to_owned(), decode(dbname)?);
    }
    for pair in query.into_iter().flat_map(|query| query.split('&')) {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode(key)?;
        let key = if key == "ssl" && value == "true" {
            values.insert("sslmode".to_owned(), "require".to_owned());
            continue;
        } else {
            key
        };
        known(&key)?;
        values.insert(key, decode(value)?);
    }
    Ok(values)
}

fn decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = text
                .get(at + 1..at + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .ok_or_else(|| format!("invalid percent-encoding in `{text}`"))?;
            out.push(hex);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("invalid UTF-8 in `{text}`"))
}

/// `host=db port=5433 dbname='my db'`.
fn parse_conninfo(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let key: String = chars
            .by_ref()
            .take_while(|&c| c != '=')
            .collect::<String>()
            .trim()
            .to_owned();
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek() == Some(&'\'') {
            chars.next();
            loop {
                match chars.next() {
                    Some('\\') => value.extend(chars.next()),
                    Some('\'') => break,
                    Some(c) => value.push(c),
                    None => {
                        return Err("unterminated quoted value in the connection string".to_owned());
                    }
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                chars.next();
                if c == '\\' {
                    value.extend(chars.next());
                } else {
                    value.push(c);
                }
            }
        }
        if key.is_empty() {
            return Err(format!("invalid connection string `{text}`"));
        }
        known(&key)?;
        values.insert(key, value);
    }
    Ok(values)
}

fn known(key: &str) -> Result<(), String> {
    if KEYS.contains(&key) || key == "service" {
        Ok(())
    } else {
        Err(format!("unsupported connection setting `{key}`"))
    }
}

/// The settings of a section of the service file: `~/.pg_service.conf`
/// (or `PGSERVICEFILE`), then the system-wide file.
fn read_service(name: &str, sources: &Sources) -> Result<BTreeMap<String, String>, String> {
    let mut files = Vec::new();
    if let Some(file) = (sources.env)("PGSERVICEFILE") {
        files.push(PathBuf::from(file));
    } else if let Some(home) = &sources.home {
        files.push(home.join(if cfg!(windows) {
            "postgresql/.pg_service.conf"
        } else {
            ".pg_service.conf"
        }));
    }
    if let Some(directory) = (sources.env)("PGSYSCONFDIR") {
        files.push(PathBuf::from(directory).join("pg_service.conf"));
    } else if cfg!(unix) {
        files.push(PathBuf::from("/etc/postgresql-common/pg_service.conf"));
        files.push(PathBuf::from("/etc/pg_service.conf"));
    }
    for file in files {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        let mut section = None;
        let mut values = BTreeMap::new();
        let mut found = false;
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(header) = line
                .strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
            {
                section = Some(header.trim().to_owned());
                found |= header.trim() == name;
                continue;
            }
            if section.as_deref() == Some(name) {
                if let Some((key, value)) = line.split_once('=') {
                    let key = key.trim();
                    known(key).map_err(|error| format!("{}: {error}", file.display()))?;
                    values.insert(key.to_owned(), value.trim().to_owned());
                }
            }
        }
        if found {
            return Ok(values);
        }
    }
    Err(format!("service `{name}` not found in any service file"))
}

/// The password for these settings from the password file: `~/.pgpass`
/// (`%APPDATA%\postgresql\pgpass.conf` on Windows) or `PGPASSFILE`. Like
/// libpq, ignores a file others can read.
fn read_password(values: &BTreeMap<String, String>, sources: &Sources) -> Option<String> {
    let file = match (sources.env)("PGPASSFILE") {
        Some(file) => PathBuf::from(file),
        None => sources.home.as_ref()?.join(if cfg!(windows) {
            "postgresql/pgpass.conf"
        } else {
            ".pgpass"
        }),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&file).ok()?.permissions().mode();
        if mode & 0o077 != 0 {
            eprintln!(
                "explainsql: ignoring {}: it must not be readable by others (chmod 0600)",
                file.display()
            );
            return None;
        }
    }
    let text = fs::read_to_string(&file).ok()?;
    let host = values
        .get("host")
        .or(values.get("hostaddr"))
        .map(String::as_str)
        .unwrap_or("localhost");
    // A socket directory matches `localhost`.
    let host = if host.starts_with('/') {
        "localhost"
    } else {
        host
    };
    let wanted = [
        host,
        values.get("port").map_or("5432", String::as_str),
        values.get("dbname").map_or("", String::as_str),
        values.get("user").map_or("", String::as_str),
    ];
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields = split_pgpass(line);
        if fields.len() != 5 {
            continue;
        }
        let matches = fields[..4]
            .iter()
            .zip(wanted)
            .all(|(field, wanted)| field == "*" || field == wanted);
        if matches {
            return Some(fields[4].clone());
        }
    }
    None
}

/// The fields of a password file line: colons and backslashes are escaped
/// with a backslash.
fn split_pgpass(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => fields.last_mut().expect("not empty").extend(chars.next()),
            ':' if fields.len() < 5 => fields.push(String::new()),
            c => fields.last_mut().expect("not empty").push(c),
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(given: Option<&str>, env: &[(&str, &str)], home: Option<&Path>) -> Settings {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let lookup = move |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        Settings::resolve_with(
            given,
            &Sources {
                env: &lookup,
                home: home.map(Path::to_path_buf),
            },
        )
        .unwrap()
    }

    #[test]
    fn reads_urls() {
        let settings = resolve(
            Some(
                "postgresql://alice:s%3Acret@db.example.com:6543/shop?sslmode=require&application_name=x",
            ),
            &[],
            None,
        );
        assert_eq!(settings.get("user"), Some("alice"));
        assert_eq!(settings.get("password"), Some("s:cret"));
        assert_eq!(settings.host(), "db.example.com");
        assert_eq!(settings.port(), Ok(6543));
        assert_eq!(settings.get("dbname"), Some("shop"));
        assert_eq!(settings.ssl_mode(), Ok(SslMode::Require));
        assert_eq!(settings.describe(), "alice@db.example.com:6543/shop");
        let ipv6 = resolve(Some("postgres://[::1]:5433/x"), &[("USER", "bob")], None);
        assert_eq!((ipv6.host(), ipv6.port()), ("::1", Ok(5433)));
        assert_eq!(ipv6.get("user"), Some("bob"));
    }

    #[test]
    fn reads_key_value_strings_and_names() {
        let settings = resolve(
            Some("host=localhost port=5433 dbname='my db' user=o\\'brien"),
            &[],
            None,
        );
        assert_eq!(settings.get("dbname"), Some("my db"));
        assert_eq!(settings.get("user"), Some("o'brien"));
        assert_eq!(settings.port(), Ok(5433));
        let named = resolve(Some("shop"), &[("PGUSER", "carol")], None);
        assert_eq!(named.get("dbname"), Some("shop"));
        assert_eq!(named.get("user"), Some("carol"));
    }

    #[test]
    fn falls_back_on_the_environment_and_defaults() {
        let settings = resolve(
            None,
            &[
                ("PGHOST", "pg"),
                ("PGUSER", "dave"),
                ("PGSSLMODE", "disable"),
            ],
            None,
        );
        assert_eq!(settings.host(), "pg");
        assert_eq!(settings.get("dbname"), Some("dave"));
        assert_eq!(settings.port(), Ok(5432));
        assert_eq!(settings.ssl_mode(), Ok(SslMode::Disable));
        assert_eq!(settings.get("application_name"), Some("explainsql"));
        // What the user gives wins.
        let settings = resolve(Some("host=other"), &[("PGHOST", "pg")], None);
        assert_eq!(settings.host(), "other");
    }

    #[test]
    fn rejects_what_it_does_not_understand() {
        let lookup = |_: &str| None;
        let sources = Sources {
            env: &lookup,
            home: None,
        };
        assert!(Settings::resolve_with(Some("host=x gssencmode=disable"), &sources).is_err());
        assert!(Settings::resolve_with(Some("host=x sslmode=sometimes"), &sources).is_err());
        assert!(Settings::resolve_with(Some("host=x port=abc"), &sources).is_err());
        assert!(Settings::resolve_with(Some("service=missing"), &sources).is_err());
    }

    #[test]
    fn reads_the_service_and_password_files() {
        let home = std::env::temp_dir().join(format!("explainsql-conn-{}", std::process::id()));
        let config = if cfg!(windows) {
            home.join("postgresql")
        } else {
            home.clone()
        };
        fs::create_dir_all(&config).unwrap();
        let service = config.join(".pg_service.conf");
        fs::write(
            &service,
            "# services\n[shop]\nhost=db.internal\nport=6000\ndbname=shop\nuser=erin\n",
        )
        .unwrap();
        let pgpass = config.join(if cfg!(windows) {
            "pgpass.conf"
        } else {
            ".pgpass"
        });
        fs::write(
            &pgpass,
            "other:*:*:*:wrong\ndb.internal:6000:shop:erin:pa\\:ss\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&pgpass, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let settings = resolve(Some("service=shop"), &[], Some(&home));
        assert_eq!(settings.host(), "db.internal");
        assert_eq!(settings.port(), Ok(6000));
        assert_eq!(settings.get("password"), Some("pa:ss"));
        // PGSERVICE works too, and explicit settings override the service.
        let settings = resolve(Some("port=6001"), &[("PGSERVICE", "shop")], Some(&home));
        assert_eq!(settings.port(), Ok(6001));
        assert_eq!(settings.get("password"), None);
        fs::remove_dir_all(&home).unwrap();
    }
}
