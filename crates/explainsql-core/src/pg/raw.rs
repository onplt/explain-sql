//! The raw plan tree that both PostgreSQL parsers produce.
//!
//! Properties use PostgreSQL's JSON names (`"Node Type"`, `"Actual Rows"`,
//! `"Shared Hit Blocks"`, ...), so a text plan and a JSON plan of the same
//! query yield the same raw tree and share one lowering step. Nodes are kept
//! in a flat arena in pre-order so that no step needs recursion, however deep
//! the input is nested.

use serde_json::{Map, Number, Value};

pub(crate) struct RawPlan {
    /// Top-level properties (`"Planning Time"`, `"Triggers"`, ...), without `"Plan"`.
    pub top: Map<String, Value>,
    /// `nodes[0]` is the root; a parent always precedes its children.
    pub nodes: Vec<RawNode>,
}

pub(crate) struct RawNode {
    pub props: Map<String, Value>,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    /// 1-based line in the (unwrapped) text input, for warnings.
    pub line: Option<usize>,
}

impl RawPlan {
    pub fn new() -> Self {
        RawPlan {
            top: Map::new(),
            nodes: Vec::new(),
        }
    }

    pub fn add_node(&mut self, parent: Option<usize>, line: Option<usize>) -> usize {
        let index = self.nodes.len();
        self.nodes.push(RawNode {
            props: Map::new(),
            parent,
            children: Vec::new(),
            line,
        });
        if let Some(parent) = parent {
            self.nodes[parent].children.push(index);
        }
        index
    }
}

/// A JSON number for `value`: an integer when it is integral, so that
/// numbers read from text compare equal to the ones PostgreSQL prints in JSON.
pub(crate) fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        // The range check makes the conversion exact.
        #[allow(clippy::cast_possible_truncation)]
        let integer = value as i64;
        Value::Number(Number::from(integer))
    } else {
        Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}
