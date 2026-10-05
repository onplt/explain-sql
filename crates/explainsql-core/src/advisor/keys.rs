//! Index keys from a scan's conditions, ordered by the ESR rule: columns
//! compared for equality first, then the sort order, then at most one range.

use super::{Index, IndexColumn, Method};
use crate::expr::{self, Access};

/// How a condition can use an index on its column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Use {
    /// `=`, `IN`, `= ANY`, `IS NULL`, or ORs of these on one column.
    Equality,
    /// `<`, `<=`, `>`, `>=`.
    Range,
    /// `LIKE 'abc%'`: a b-tree range, with `text_pattern_ops` unless the
    /// column uses the C collation.
    Prefix,
    /// `LIKE '%abc%'`, `ILIKE`: a trigram GIN index.
    Substring,
    /// `@>`, `&&`, `@@` and the like: a GIN index.
    Containment,
}

fn usage(operator: &str, value: &str) -> Option<Use> {
    match operator {
        "=" | "IS" | "OR" => Some(Use::Equality),
        "<" | "<=" | ">" | ">=" => Some(Use::Range),
        "~~" if value.starts_with("'%") || value.starts_with("'_") => Some(Use::Substring),
        "~~" if value.starts_with('\'') => Some(Use::Prefix),
        "~~*" => Some(Use::Substring),
        "@>" | "<@" | "?" | "?|" | "?&" | "&&" | "@@" => Some(Use::Containment),
        _ => None,
    }
}

/// What a scan's conditions say about an index on its table.
#[derive(Debug, Default)]
pub struct Keys {
    equality: Vec<String>,
    sort: Vec<(String, bool)>,
    range: Option<(String, Option<&'static str>)>,
    gin: Option<(String, Option<&'static str>)>,
    /// A cast or a function of a column: `(column, wrapper)`.
    pub wrapped: Option<(String, String)>,
    /// A condition ORs comparisons of different columns.
    pub or_across_columns: bool,
    /// The keys need an operator class that depends on the collation.
    pub needs_pattern_ops: bool,
    /// The index is a trigram index, which needs the pg_trgm extension.
    pub needs_trigram: bool,
    /// A column was compared with a value known only at run time.
    pub runtime_value: bool,
}

impl Keys {
    /// Adds a condition of the scan whose columns `own` qualifies. With
    /// `joins`, comparisons with another table's columns count as equality
    /// (a join key, or a correlated subquery's reference).
    pub fn add(&mut self, condition: &str, own: Option<&str>, joins: bool) {
        for conjunct in expr::conjuncts(condition) {
            match expr::access(conjunct) {
                Access::Column {
                    column,
                    operator,
                    value,
                } => {
                    if !belongs(column, own) {
                        continue;
                    }
                    let name = expr::split_column(column).1.to_owned();
                    if value.starts_with('$') || value.contains("InitPlan") {
                        self.runtime_value = true;
                    }
                    match usage(operator, value) {
                        Some(Use::Equality) => push(&mut self.equality, name),
                        Some(Use::Range) => {
                            self.range.get_or_insert((name, None));
                        }
                        Some(Use::Prefix) => {
                            self.needs_pattern_ops = true;
                            self.range.get_or_insert((name, Some("text_pattern_ops")));
                        }
                        Some(Use::Substring) => {
                            self.needs_trigram = true;
                            self.gin.get_or_insert((name, Some("gin_trgm_ops")));
                        }
                        Some(Use::Containment) => {
                            self.gin.get_or_insert((name, None));
                        }
                        None => {}
                    }
                }
                Access::Columns(a, b) if joins => {
                    // The side of the comparison that belongs to this scan.
                    let (mine, theirs) = (belongs(a, own), belongs(b, own));
                    if mine != theirs {
                        let column = if mine { a } else { b };
                        push(&mut self.equality, expr::split_column(column).1.to_owned());
                    }
                }
                Access::Wrapped { column, wrapper } if belongs(column, own) => {
                    self.wrapped.get_or_insert((
                        expr::split_column(column).1.to_owned(),
                        wrapper.to_owned(),
                    ));
                }
                Access::OrAcrossColumns => self.or_across_columns = true,
                _ => {}
            }
        }
    }

    /// Adds a column the rows are wanted in order of.
    pub fn sort(&mut self, column: String, descending: bool) {
        if !self.sort.iter().any(|(name, _)| *name == column) {
            self.sort.push((column, descending));
        }
    }

    /// The index these keys call for, if any: a b-tree on the equality, sort
    /// and range columns, or else a GIN index for containment and substring
    /// searches.
    pub fn index(&self, schema: Option<String>, table: String) -> Option<Index> {
        let mut columns: Vec<IndexColumn> = Vec::new();
        let mut add = |name: &str, descending: bool, opclass: Option<&'static str>| {
            if !columns.iter().any(|column| column.name == name) {
                columns.push(IndexColumn {
                    name: name.to_owned(),
                    descending,
                    opclass,
                });
            }
        };
        for name in &self.equality {
            add(name, false, None);
        }
        // Directions only matter when they differ: a b-tree reads backwards.
        let mixed = self.sort.iter().any(|(_, d)| *d) && self.sort.iter().any(|(_, d)| !*d);
        for (name, descending) in &self.sort {
            add(name, mixed && *descending, None);
        }
        if let Some((name, opclass)) = &self.range {
            add(name, false, *opclass);
        }
        if !columns.is_empty() {
            return Some(Index {
                schema,
                table,
                method: Method::Btree,
                columns,
                partitioned: false,
            });
        }
        let (name, opclass) = self.gin.clone()?;
        Some(Index {
            schema,
            table,
            method: Method::Gin,
            columns: vec![IndexColumn {
                name,
                descending: false,
                opclass,
            }],
            partitioned: false,
        })
    }
}

fn push(columns: &mut Vec<String>, name: String) {
    if !columns.contains(&name) {
        columns.push(name);
    }
}

/// Whether a column belongs to the scan its qualifier names. Unqualified
/// columns belong to the only table there is.
fn belongs(column: &str, own: Option<&str>) -> bool {
    match (expr::split_column(column).0, own) {
        (Some(qualifier), Some(own)) => qualifier == own,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(conditions: &[(&str, bool)], own: &str) -> Option<String> {
        let mut keys = Keys::default();
        for (condition, joins) in conditions {
            keys.add(condition, Some(own), *joins);
        }
        keys.index(None, "t".to_owned()).map(|index| index.target())
    }

    #[test]
    fn orders_keys_by_equality_then_range() {
        assert_eq!(
            index(
                &[
                    ("((t.created_at >= '2024-06-01'::date) AND (t.created_at < '2024-06-15'::date))", false),
                    ("(t.status = 'refunded'::text)", false)
                ],
                "t"
            )
            .as_deref(),
            Some("t (status, created_at)")
        );
        assert_eq!(
            index(&[("(t.a = ANY ('{1,2}'::integer[]))", false)], "t").as_deref(),
            Some("t (a)")
        );
    }

    #[test]
    fn picks_the_operator_class() {
        assert_eq!(
            index(&[("(t.note ~~ 'ab%'::text)", false)], "t").as_deref(),
            Some("t (note text_pattern_ops)")
        );
        assert_eq!(
            index(&[("(t.note ~~ '%ab%'::text)", false)], "t").as_deref(),
            Some("t USING gin (note gin_trgm_ops)")
        );
        assert_eq!(
            index(&[("(t.payload @> '{\"n\": 7}'::jsonb)", false)], "t").as_deref(),
            Some("t USING gin (payload)")
        );
    }

    #[test]
    fn takes_this_side_of_a_join() {
        assert_eq!(
            index(&[("(o.id = oi.order_id)", true)], "oi").as_deref(),
            Some("t (order_id)")
        );
        assert_eq!(index(&[("(o.id = oi.order_id)", false)], "oi"), None);
    }

    #[test]
    fn notices_what_no_index_serves() {
        let mut keys = Keys::default();
        keys.add("((t.a = 1) OR (t.b = 2))", Some("t"), false);
        assert!(keys.or_across_columns);
        assert_eq!(keys.index(None, "t".to_owned()), None);
        let mut keys = Keys::default();
        keys.add("((t.a)::text = '1'::text)", Some("t"), false);
        assert_eq!(keys.wrapped, Some(("a".to_owned(), "a cast".to_owned())));
    }

    #[test]
    fn sorts_after_equality() {
        let mut keys = Keys::default();
        keys.add("(t.customer_id = c.id)", Some("t"), true);
        keys.sort("created_at".to_owned(), true);
        assert_eq!(
            keys.index(None, "t".to_owned()).unwrap().target(),
            "t (customer_id, created_at)"
        );
    }
}
