//! `EXPLAIN (FORMAT JSON)` into the raw tree.

use std::collections::VecDeque;

use serde_json::{Map, Value};

use super::raw::RawPlan;
use crate::ir::Warning;

/// JSON nesting deeper than this is rejected before parsing, because
/// serde_json parses recursively. A plan level takes two JSON levels, so this
/// allows 255 plan levels. Parsing a plan at the limit takes under 192 KiB of
/// stack in a release build and about 1 MiB in a debug build (measured on
/// Linux x86-64), half of a test thread's default stack.
const MAX_DEPTH: usize = 512;

pub(crate) fn parse(text: &str, warnings: &mut Vec<Warning>) -> Result<RawPlan, String> {
    let value = match read(text) {
        Ok((value, trailing)) => {
            if trailing {
                warnings.push(Warning {
                    line: None,
                    message: "ignored text after the JSON plan".to_owned(),
                });
            }
            value
        }
        Err(error) => {
            let repaired = repair_truncated(text).ok_or(error)?;
            warnings.push(Warning {
                line: None,
                message: "the JSON plan is incomplete; showing the part that could be read"
                    .to_owned(),
            });
            repaired
        }
    };

    let mut top = match value {
        Value::Array(items) => {
            if items.len() > 1 {
                warnings.push(Warning {
                    line: None,
                    message: format!(
                        "the input contains {} plans; showing the first",
                        items.len()
                    ),
                });
            }
            match items.into_iter().next() {
                Some(Value::Object(top)) => top,
                _ => return Err("expected an array holding a plan object".to_owned()),
            }
        }
        Value::Object(top) => top,
        _ => return Err("expected a plan object or an array of them".to_owned()),
    };
    let Some(Value::Object(root)) = top.remove("Plan") else {
        return Err("no \"Plan\" object".to_owned());
    };

    let mut plan = RawPlan::new();
    plan.top = top;
    // Depth-first with an explicit stack; children are pushed in reverse so
    // that they are numbered in plan order.
    let mut stack: Vec<(Map<String, Value>, Option<usize>)> = vec![(root, None)];
    while let Some((mut props, parent)) = stack.pop() {
        let children = match props.remove("Plans") {
            Some(Value::Array(children)) => children,
            Some(other) => {
                props.insert("Plans".to_owned(), other);
                Vec::new()
            }
            None => Vec::new(),
        };
        let index = plan.add_node(parent, None);
        plan.nodes[index].props = props;
        for child in children.into_iter().rev() {
            match child {
                Value::Object(child) => stack.push((child, Some(index))),
                other => warnings.push(Warning {
                    line: None,
                    message: format!("ignored a child plan that is not an object: {other}"),
                }),
            }
        }
    }
    Ok(plan)
}

/// Parses the first JSON value in `text` without serde_json's fixed nesting
/// limit of 128, which deep join trees exceed, while still bounding the
/// recursion depth. Also tells whether text follows the value.
fn read(text: &str) -> Result<(Value, bool), String> {
    let depth = nesting_depth(text);
    if depth > MAX_DEPTH {
        return Err(format!("the JSON is nested {depth} levels deep"));
    }
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    let mut values = deserializer.into_iter::<Value>();
    let value = match values.next() {
        Some(value) => value.map_err(|e| e.to_string())?,
        None => return Err("no JSON value".to_owned()),
    };
    let trailing = !text[values.byte_offset()..].trim().is_empty();
    Ok((value, trailing))
}

/// The deepest bracket nesting outside of strings.
fn nesting_depth(text: &str) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    let (mut in_string, mut escaped) = (false, false);
    for byte in text.bytes() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                max = max.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// Recovers a plan whose JSON was cut off (a truncated copy or log line) by
/// closing the open brackets after one of the last complete values.
fn repair_truncated(text: &str) -> Option<Value> {
    const CANDIDATES: usize = 32;
    if nesting_depth(text) > MAX_DEPTH {
        return None;
    }
    // Positions after which the text could be closed: after a closing
    // bracket or before a comma, inside an open structure.
    let mut cuts: VecDeque<usize> = VecDeque::with_capacity(CANDIDATES);
    let mut depth = 0usize;
    let (mut in_string, mut escaped) = (false, false);
    for (position, byte) in text.bytes().enumerate() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        let cut = match byte {
            b'"' => {
                in_string = true;
                None
            }
            b'{' | b'[' => {
                depth += 1;
                None
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                Some(position + 1)
            }
            b',' => Some(position),
            _ => None,
        };
        if let Some(cut) = cut.filter(|_| depth > 0) {
            if cuts.len() == CANDIDATES {
                cuts.pop_front();
            }
            cuts.push_back(cut);
        }
    }
    if depth == 0 && !in_string {
        // Complete but invalid: not a truncation.
        return None;
    }
    cuts.iter().rev().find_map(|&cut| {
        let mut candidate = text[..cut].to_owned();
        candidate.push_str(&closers(&candidate));
        read(&candidate).ok().map(|(value, _)| value)
    })
}

/// The brackets that close what is open at the end of `text`.
fn closers(text: &str) -> String {
    let mut open = Vec::new();
    let (mut in_string, mut escaped) = (false, false);
    for byte in text.bytes() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => open.push('}'),
            b'[' => open.push(']'),
            b'}' | b']' => {
                open.pop();
            }
            _ => {}
        }
    }
    open.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_types(plan: &RawPlan) -> Vec<&str> {
        plan.nodes
            .iter()
            .map(|node| node.props["Node Type"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn numbers_nodes_in_plan_order() {
        let text = r#"[{"Plan": {"Node Type": "Hash Join", "Plans": [
            {"Node Type": "Seq Scan"},
            {"Node Type": "Hash", "Plans": [{"Node Type": "Index Scan"}]}
        ]}, "Execution Time": 1.5}]"#;
        let plan = parse(text, &mut Vec::new()).unwrap();
        assert_eq!(
            node_types(&plan),
            ["Hash Join", "Seq Scan", "Hash", "Index Scan"]
        );
        assert_eq!(plan.nodes[0].children, [1, 2]);
        assert_eq!(plan.nodes[3].parent, Some(2));
        assert_eq!(plan.top["Execution Time"], 1.5);
    }

    #[test]
    fn accepts_an_auto_explain_object() {
        let text = r#"{"Query Text": "SELECT 1", "Plan": {"Node Type": "Result"}}"#;
        let plan = parse(text, &mut Vec::new()).unwrap();
        assert_eq!(plan.top["Query Text"], "SELECT 1");
    }

    #[test]
    fn repairs_a_truncated_plan() {
        let text = r#"[{"Plan": {"Node Type": "Limit", "Plans": [{"Node Type": "Seq Scan", "Relation Name": "or"#;
        let mut warnings = Vec::new();
        let plan = parse(text, &mut warnings).unwrap();
        assert_eq!(node_types(&plan), ["Limit", "Seq Scan"]);
        assert!(!plan.nodes[1].props.contains_key("Relation Name"));
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn ignores_text_after_the_plan() {
        let mut warnings = Vec::new();
        let plan = parse(
            "[{\"Plan\": {\"Node Type\": \"Result\"}}]\n\nThis took 5 s.",
            &mut warnings,
        )
        .unwrap();
        assert_eq!(node_types(&plan), ["Result"]);
        assert_eq!(warnings[0].message, "ignored text after the JSON plan");
    }

    /// Plans nested `levels` deep below the root, in `levels * 2 + 2` levels
    /// of JSON.
    fn nested(levels: usize) -> String {
        let mut text = String::from(r#"{"Plan": "#);
        for _ in 0..levels {
            text.push_str(r#"{"Node Type": "Nested Loop", "Plans": ["#);
        }
        text.push_str(r#"{"Node Type": "Result"}"#);
        for _ in 0..levels {
            text.push_str("]}");
        }
        text.push('}');
        text
    }

    #[test]
    fn reads_plans_up_to_the_depth_limit() {
        // Deeper than serde_json's own limit of 128 allows.
        let deepest = (MAX_DEPTH - 2) / 2;
        // About 1 MiB is needed in a debug build on Linux; leave room for
        // platforms with larger stack frames.
        let parsed = std::thread::Builder::new()
            .stack_size(4 << 20)
            .spawn(move || parse(&nested(deepest), &mut Vec::new()).map(|plan| plan.nodes.len()))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(parsed, Ok(deepest + 1));
        assert!(parse(&nested(deepest + 1), &mut Vec::new()).is_err());
    }

    #[test]
    fn rejects_what_is_not_a_plan() {
        assert!(parse("[1, 2]", &mut Vec::new()).is_err());
        assert!(parse(r#"{"Planz": {}}"#, &mut Vec::new()).is_err());
        assert!(parse(r#"{"Plan": "x"}"#, &mut Vec::new()).is_err());
        assert!(parse(r#"{"Plan": }"#, &mut Vec::new()).is_err());
        let deep = "[".repeat(MAX_DEPTH + 1);
        assert!(parse(&deep, &mut Vec::new()).is_err());
    }
}
