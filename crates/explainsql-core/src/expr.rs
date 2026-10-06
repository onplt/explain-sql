//! Conditions as PostgreSQL prints them in plans (`((status)::text =
//! 'open'::text)`): enough of the deparsed expressions to tell which columns
//! a condition compares with what, and whether it wraps them in a cast or a
//! function. Shared by the rules and the index advisor.

/// The parts of a condition joined by `AND` at the top level, without their
/// outer parentheses.
pub fn conjuncts(condition: &str) -> Vec<&str> {
    split(strip_parens(condition), " AND ")
}

/// The parts joined by `OR`.
pub fn disjuncts(condition: &str) -> Vec<&str> {
    split(strip_parens(condition), " OR ")
}

/// Splits at a separator outside parentheses, brackets and quotes.
pub(crate) fn split<'a>(text: &'a str, separator: &str) -> Vec<&'a str> {
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let (mut start, mut depth, mut quote) = (0, 0i32, None);
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None => match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                _ if depth == 0 && bytes[at..].starts_with(separator.as_bytes()) => {
                    parts.push(strip_parens(&text[start..at]));
                    at += separator.len();
                    start = at;
                    continue;
                }
                _ => {}
            },
        }
        at += 1;
    }
    parts.push(strip_parens(&text[start..]));
    parts
}

/// The position of the parenthesis closing the one that opens `text`.
fn closing(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let (mut depth, mut quote) = (0i32, None);
    for (at, &byte) in bytes.iter().enumerate() {
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None => match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(at);
                    }
                }
                _ => {}
            },
        }
    }
    None
}

/// Removes parentheses around the whole text.
pub fn strip_parens(text: &str) -> &str {
    let mut text = text.trim();
    while closing(text) == Some(text.len().saturating_sub(1)) && text.len() >= 2 {
        text = text[1..text.len() - 1].trim();
    }
    text
}

/// `left operator right`, from the first operator outside parentheses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Comparison<'a> {
    pub left: &'a str,
    pub operator: &'a str,
    pub right: &'a str,
}

const OPERATOR: &[u8] = b"=<>!~*@&|#%^+-/?";

pub fn comparison(condition: &str) -> Option<Comparison<'_>> {
    let text = strip_parens(condition);
    let bytes = text.as_bytes();
    let (mut depth, mut quote) = (0i32, None);
    for (at, &byte) in bytes.iter().enumerate() {
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None => match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                b' ' if depth == 0 => {
                    let length = bytes[at + 1..]
                        .iter()
                        .take_while(|byte| OPERATOR.contains(byte))
                        .count();
                    let end = at + 1 + length;
                    if length > 0 && bytes.get(end) == Some(&b' ') {
                        return Some(Comparison {
                            left: text[..at].trim(),
                            operator: &text[at + 1..end],
                            right: text[end + 1..].trim(),
                        });
                    }
                    if bytes[at + 1..].starts_with(b"IS ") {
                        return Some(Comparison {
                            left: text[..at].trim(),
                            operator: "IS",
                            right: text[at + 4..].trim(),
                        });
                    }
                }
                _ => {}
            },
        }
    }
    None
}

/// What one side of a comparison is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand<'a> {
    /// A column, possibly qualified: `orders.customer_id`.
    Column(&'a str),
    /// A cast of a column: `(orders.customer_id)::text`.
    Cast(&'a str),
    /// A function of a column: `date_trunc('day'::text, created_at)`.
    Function {
        name: &'a str,
        column: &'a str,
    },
    /// A constant or a parameter: `4242`, `'x'::text`, `$1`,
    /// `ANY ('{1,2}'::integer[])`.
    Value,
    Other,
}

pub fn operand(text: &str) -> Operand<'_> {
    let text = strip_parens(text);
    if is_value(text) {
        return Operand::Value;
    }
    if is_column(text) {
        return Operand::Column(text);
    }
    if let Some(close) = closing(text) {
        // (column)::type
        if text[close + 1..].starts_with("::") {
            return match operand(&text[1..close]) {
                Operand::Column(column) | Operand::Cast(column) => Operand::Cast(column),
                Operand::Value => Operand::Value,
                other => other,
            };
        }
    }
    if let Some((before, _)) = text.split_once("::") {
        if is_column(before) {
            return Operand::Cast(before);
        }
        if is_value(before) {
            return Operand::Value;
        }
    }
    if let Some(open) = text.find('(') {
        let name = &text[..open];
        if is_column(name)
            && closing(&text[open..]).map(|close| open + close) == Some(text.len() - 1)
        {
            let arguments = &text[open + 1..text.len() - 1];
            for argument in split(arguments, ", ") {
                if let Operand::Column(column) | Operand::Cast(column) = operand(argument) {
                    return Operand::Function { name, column };
                }
            }
        }
    }
    Operand::Other
}

fn is_value(text: &str) -> bool {
    let first = text.as_bytes().first().copied().unwrap_or(b' ');
    first == b'\''
        || first == b'$'
        || first.is_ascii_digit()
        || (first == b'-' && text.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
        || ["NULL", "NOT NULL", "true", "false", "TRUE", "FALSE"].contains(&text)
        || ["ANY (", "ALL (", "ARRAY[", "(InitPlan ", "InitPlan "]
            .iter()
            .any(|prefix| text.starts_with(prefix))
}

/// An identifier chain such as `orders.customer_id` or `"Order"."Id"`.
fn is_column(text: &str) -> bool {
    !text.is_empty()
        && split(text, ".").into_iter().all(|part| {
            if let Some(quoted) = part.strip_prefix('"') {
                quoted.ends_with('"') && quoted.len() > 1
            } else {
                part.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                    && part
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
            }
        })
}

/// A qualified column's relation alias and name: `o.customer_id` →
/// (`Some("o")`, `customer_id`).
pub fn split_column(column: &str) -> (Option<&str>, &str) {
    let parts = split(column, ".");
    match parts.last() {
        Some(name) if parts.len() > 1 => {
            // The parts are slices of `column`; the last follows a dot.
            let start = name.as_ptr() as usize - column.as_ptr() as usize;
            (Some(&column[..start - 1]), name)
        }
        _ => (None, column),
    }
}

/// What one conjunct of a filter means for index use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access<'a> {
    /// Compares a column with a value, or ORs such comparisons on one column.
    Column {
        column: &'a str,
        operator: &'a str,
        value: &'a str,
    },
    /// Compares a cast or a function of a column with a value: an index on
    /// the column cannot serve it.
    Wrapped {
        column: &'a str,
        wrapper: &'a str,
    },
    /// ORs conditions on different columns.
    OrAcrossColumns,
    /// Compares two columns, as a join condition does.
    Columns(&'a str, &'a str),
    Unknown,
}

pub fn access(conjunct: &str) -> Access<'_> {
    let parts = disjuncts(conjunct);
    if parts.len() > 1 {
        let accesses: Vec<Access> = parts.iter().map(|part| access(part)).collect();
        let first = match accesses.first() {
            Some(Access::Column { column, .. }) => *column,
            _ => return Access::OrAcrossColumns,
        };
        let same = accesses
            .iter()
            .all(|access| matches!(access, Access::Column { column, .. } if *column == first));
        return if same {
            Access::Column {
                column: first,
                operator: "OR",
                value: "",
            }
        } else {
            Access::OrAcrossColumns
        };
    }
    let Some(comparison) = comparison(conjunct) else {
        return Access::Unknown;
    };
    let (left, right) = (operand(comparison.left), operand(comparison.right));
    let (side, value) = match (left, right) {
        (Operand::Column(a), Operand::Column(b)) => return Access::Columns(a, b),
        (side, Operand::Value) => (side, comparison.right),
        (Operand::Value, side) => (side, comparison.left),
        _ => return Access::Unknown,
    };
    match side {
        Operand::Column(column) => Access::Column {
            column,
            operator: comparison.operator,
            value,
        },
        Operand::Cast(column) => Access::Wrapped {
            column,
            wrapper: "a cast",
        },
        Operand::Function { name, column } => Access::Wrapped {
            column,
            wrapper: name,
        },
        Operand::Value | Operand::Other => Access::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_conditions() {
        assert_eq!(
            conjuncts("((addresses.city = 42) AND (addresses.country = 0))"),
            ["addresses.city = 42", "addresses.country = 0"]
        );
        assert_eq!(
            conjuncts("(note = 'a AND b'::text)"),
            ["note = 'a AND b'::text"]
        );
        assert_eq!(
            disjuncts("((customer_id = 4242) OR (status = 'refunded'::text))"),
            ["customer_id = 4242", "status = 'refunded'::text"]
        );
        assert_eq!(strip_parens("(a) = (b)"), "(a) = (b)");
        assert_eq!(strip_parens("((x))"), "x");
    }

    #[test]
    fn finds_comparisons() {
        let comparison = comparison("((orders.customer_id)::text = '4242'::text)").unwrap();
        assert_eq!(
            (comparison.left, comparison.operator, comparison.right),
            ("(orders.customer_id)::text", "=", "'4242'::text")
        );
        let like = super::comparison("(orders.note ~~ '%abcd%'::text)").unwrap();
        assert_eq!(like.operator, "~~");
        let null = super::comparison("(shipped_at IS NULL)").unwrap();
        assert_eq!((null.operator, null.right), ("IS", "NULL"));
    }

    #[test]
    fn classifies_access() {
        assert_eq!(
            access("(orders.customer_id = 4242)"),
            Access::Column {
                column: "orders.customer_id",
                operator: "=",
                value: "4242"
            }
        );
        assert_eq!(
            access("((orders.customer_id)::text = '4242'::text)"),
            Access::Wrapped {
                column: "orders.customer_id",
                wrapper: "a cast"
            }
        );
        assert_eq!(
            access(
                "(date_trunc('day'::text, orders.created_at) = '2024-06-01 00:00:00+00'::timestamp with time zone)"
            ),
            Access::Wrapped {
                column: "orders.created_at",
                wrapper: "date_trunc"
            }
        );
        assert_eq!(
            access("((orders.customer_id = 4242) OR (orders.status = 'refunded'::text))"),
            Access::OrAcrossColumns
        );
        assert!(matches!(
            access("((status = 'a'::text) OR (status = 'b'::text))"),
            Access::Column {
                column: "status",
                ..
            }
        ));
        assert_eq!(
            access("(o.id = oi.order_id)"),
            Access::Columns("o.id", "oi.order_id")
        );
        assert!(matches!(
            access("(payload @> '{\"n\": 7}'::jsonb)"),
            Access::Column { operator: "@>", .. }
        ));
    }

    #[test]
    fn splits_qualified_columns() {
        assert_eq!(split_column("o.customer_id"), (Some("o"), "customer_id"));
        assert_eq!(split_column("public.t.a"), (Some("public.t"), "a"));
        assert_eq!(
            split_column("\"My T\".\"Id\""),
            (Some("\"My T\""), "\"Id\"")
        );
        assert_eq!(split_column("id"), (None, "id"));
    }
}
