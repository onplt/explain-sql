//! The statements that cost a database the most, as pg_stat_statements
//! counts them, and which of them explainsql can plan.

use serde::Serialize;

/// One statement from `pg_stat_statements`: its normalized text, with
/// constants replaced by `$1`, `$2`, …, and what its executions cost.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    /// The query identifier; none when the statement is another user's and
    /// the role may not see it.
    pub queryid: Option<i64>,
    pub query: String,
    pub calls: i64,
    /// Execution time over all calls, in milliseconds.
    pub total_ms: f64,
    /// Its part of the execution time of all the statements counted in the
    /// database, from 0 to 1.
    pub share: f64,
    pub mean_ms: f64,
    /// Rows returned or affected over all calls.
    pub rows: i64,
    /// Pages found in shared buffers, and read from disk or the OS cache.
    pub shared_hit: i64,
    pub shared_read: i64,
    /// Pages written to temporary files: sorts and hashes that spilled.
    pub temp_written: i64,
    /// Why it cannot be planned, when it cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unplannable: Option<String>,
}

impl Entry {
    /// Pages the statement touched, from cache or not.
    pub fn pages(&self) -> i64 {
        self.shared_hit.saturating_add(self.shared_read)
    }

    /// The text on one line, with runs of blanks as one space.
    pub fn one_line(&self) -> String {
        self.query.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// The text pg_stat_statements shows for another user's statement to a
/// role without `pg_read_all_stats`.
pub const HIDDEN: &str = "<insufficient privilege>";

/// The text of a statement whose text pg_stat_statements could not find.
pub const LOST: &str = "<text not found>";

/// Statement kinds explainsql plans.
const PLANNABLE: [&str; 8] = [
    "SELECT", "WITH", "VALUES", "TABLE", "INSERT", "UPDATE", "DELETE", "MERGE",
];

/// Why a statement cannot be planned, when it cannot: another user's that
/// the role may not read, a text lost, a utility command such as `VACUUM`
/// or `SET`, or a text cut off at `track_activity_query_size` bytes
/// (`query_size`).
pub fn unplannable(query: &str, query_size: Option<usize>) -> Option<String> {
    if query == HIDDEN {
        return Some(
            "another user's statement: its text needs the pg_read_all_stats role".to_owned(),
        );
    }
    if query == LOST {
        return Some("pg_stat_statements could not find its text".to_owned());
    }
    let keyword: String = skip_comments(query)
        .trim_start_matches('(')
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    if !PLANNABLE.contains(&keyword.as_str()) {
        return Some(if keyword.is_empty() {
            "not a statement explainsql plans".to_owned()
        } else {
            format!("{keyword} is a command without a plan")
        });
    }
    // The text is cut at one byte short of the setting.
    if let Some(size) = query_size {
        if size > 1 && query.len() >= size - 1 {
            return Some(format!(
                "its text was cut at track_activity_query_size ({size} bytes)"
            ));
        }
    }
    None
}

fn skip_comments(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_start();
        if let Some(rest) = sql.strip_prefix("--") {
            sql = rest.split_once('\n').map_or("", |(_, rest)| rest);
        } else if let Some(rest) = sql.strip_prefix("/*") {
            sql = rest.split_once("*/").map_or("", |(_, rest)| rest);
        } else {
            return sql;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_what_cannot_be_planned() {
        assert_eq!(
            unplannable("SELECT * FROM orders WHERE id = $1", Some(1024)),
            None
        );
        assert_eq!(
            unplannable("/* app */ WITH x AS (SELECT 1) SELECT * FROM x", None),
            None
        );
        assert!(
            unplannable("VACUUM orders", None)
                .unwrap()
                .contains("VACUUM")
        );
        assert!(
            unplannable("SET work_mem = $1", None)
                .unwrap()
                .contains("SET")
        );
        assert!(
            unplannable(HIDDEN, None)
                .unwrap()
                .contains("pg_read_all_stats")
        );
        assert!(unplannable(LOST, None).unwrap().contains("its text"));
        let long = format!("SELECT {}", "x, ".repeat(400));
        assert!(
            unplannable(&long[..1023], Some(1024))
                .unwrap()
                .contains("track_activity_query_size")
        );
    }
}
