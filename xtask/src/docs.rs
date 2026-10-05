//! The documentation: rule pages with examples from the corpus, and a check
//! of the links between pages.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use explainsql_core::rules::{RULES, Severity};

use crate::fixtures::{DEFAULT_VERSIONS, workspace_root};
use crate::scenario::{self, Scenario};

const START: &str = "<!-- example:start -->";
const END: &str = "<!-- example:end -->";
/// Longer plans are cut in the examples.
const MAX_PLAN_LINES: usize = 40;

/// `cargo xtask rule-docs [--check]`: writes the example of each rule page,
/// or with `--check` fails if one is out of date.
pub fn rule_docs(check: bool) -> Result<(), String> {
    let root = workspace_root();
    let scenarios = scenario::load_all(&root.join("fixtures/scenarios"))?;
    let mut stale = Vec::new();
    for rule in RULES {
        let path = root.join("docs/rules").join(format!("{}.md", rule.id));
        let page = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (before, rest) = page
            .split_once(START)
            .ok_or_else(|| format!("{}: no {START} marker", path.display()))?;
        let (_, after) = rest
            .split_once(END)
            .ok_or_else(|| format!("{}: no {END} marker", path.display()))?;
        let example = example(&root, &scenarios, rule.id)?;
        let fresh = format!("{before}{START}\n{example}{END}{after}");
        if fresh != page {
            if check {
                stale.push(path.display().to_string());
            } else {
                fs::write(&path, fresh).map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
    }
    if !stale.is_empty() {
        return Err(format!(
            "rule pages out of date (run cargo xtask rule-docs):\n  {}",
            stale.join("\n  ")
        ));
    }
    println!(
        "{} rule pages {}",
        RULES.len(),
        if check { "are up to date" } else { "written" }
    );
    Ok(())
}

/// The example section of a rule page: the scenario that shows the rule
/// best (it requires the rule and triggers the fewest others), its plan,
/// and the finding.
fn example(root: &Path, scenarios: &[Scenario], id: &str) -> Result<String, String> {
    let mut candidates: Vec<&Scenario> = scenarios
        .iter()
        .filter(|scenario| scenario.rules.iter().any(|rule| rule == id))
        .collect();
    candidates.sort_by_key(|scenario| (scenario.rules.len(), scenario.name.clone()));
    // The newest captured version first, preferring 16 when it has it.
    let mut versions: Vec<u32> = vec![16];
    versions.extend(DEFAULT_VERSIONS.iter().rev().filter(|&&major| major != 16));
    for scenario in candidates {
        for &major in &versions {
            let path = root
                .join("fixtures/pg")
                .join(major.to_string())
                .join(format!("{}.txt", scenario.name));
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let plan =
                explainsql_core::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            let analysis = explainsql_core::analyze(&plan);
            let Some(finding) = analysis
                .findings
                .iter()
                .find(|finding| finding.rule.id == id)
            else {
                continue;
            };
            let mut out = String::new();
            out.push_str(&format!(
                "\n## Example\n\nThe `{}` scenario of the test corpus, captured on PostgreSQL {major}: {}\n\n",
                scenario.name,
                lowercase_first(&scenario.description)
            ));
            out.push_str(&format!("```sql\n{};\n```\n\n", scenario.statement));
            let lines: Vec<&str> = text.trim_end().lines().collect();
            out.push_str("```text\n");
            for line in lines.iter().take(MAX_PLAN_LINES) {
                out.push_str(line.trim_end());
                out.push('\n');
            }
            if lines.len() > MAX_PLAN_LINES {
                out.push_str(&format!(
                    "… ({} more lines)\n",
                    lines.len() - MAX_PLAN_LINES
                ));
            }
            out.push_str("```\n\nexplainsql reports:\n\n```text\n");
            let severity = match finding.severity {
                Severity::High => "HIGH",
                Severity::Medium => "MEDIUM",
                Severity::Low => "LOW",
            };
            out.push_str(&format!(
                "{severity}  {} {}\n",
                finding.rule.id, finding.rule.name
            ));
            out.push_str(&format!("{}.\n", finding.summary));
            for evidence in &finding.evidence {
                out.push_str(&format!("{}: {}\n", evidence.label, evidence.value));
            }
            out.push_str(&format!("→ {}\n```\n\n", finding.action));
            return Ok(out);
        }
    }
    Err(format!("no scenario in the corpus triggers {id}"))
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        // Keep acronyms and names: only lowercase a capital followed by a
        // lowercase letter.
        Some(first) if chars.clone().next().is_some_and(char::is_lowercase) => {
            first.to_lowercase().chain(chars).collect()
        }
        _ => text.to_owned(),
    }
}

/// `cargo xtask check-links`: every relative link in the README and the
/// documentation points to a file that exists, and every `#anchor` to a
/// heading in it. Pages of the documentation site may not link outside
/// `docs/`, which the site does not contain.
pub fn check_links() -> Result<(), String> {
    let root = workspace_root();
    let mut files = vec![root.join("README.md"), root.join("CHANGELOG.md")];
    collect_markdown(&root.join("docs"), &mut files);
    let docs = root.join("docs");
    let mut problems = Vec::new();
    let mut checked = 0;
    for file in &files {
        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        for target in links(&text) {
            if target.contains("://") || target.starts_with("mailto:") {
                continue;
            }
            checked += 1;
            let (path, anchor) = match target.split_once('#') {
                Some((path, anchor)) => (path, Some(anchor)),
                None => (target.as_str(), None),
            };
            let resolved = if path.is_empty() {
                file.clone()
            } else {
                normalize(&file.parent().expect("files have a parent").join(path))
            };
            let label = format!("{}: {target}", relative(&root, file));
            if !resolved.exists() {
                problems.push(format!("{label}: no such file"));
                continue;
            }
            if file.starts_with(&docs) && !resolved.starts_with(&docs) {
                problems.push(format!(
                    "{label}: leaves docs/, which the documentation site does not contain; link to GitHub instead"
                ));
            }
            if let Some(anchor) = anchor {
                let target_text = fs::read_to_string(&resolved).unwrap_or_default();
                if !anchors(&target_text).contains(anchor) {
                    problems.push(format!("{label}: no heading for #{anchor}"));
                }
            }
        }
    }
    if !problems.is_empty() {
        return Err(format!(
            "{} broken links:\n  {}",
            problems.len(),
            problems.join("\n  ")
        ));
    }
    println!("{checked} links in {} files are fine", files.len());
    Ok(())
}

fn collect_markdown(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "book") {
                collect_markdown(&path, files);
            }
        } else if path.extension().is_some_and(|extension| extension == "md") {
            files.push(path);
        }
    }
}

/// The targets of Markdown links, `[text](target)`, outside code.
fn links(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        // Inline code spans hold no links.
        let mut plain = String::new();
        for (index, part) in line.split('`').enumerate() {
            if index % 2 == 0 {
                plain.push_str(part);
            }
        }
        let mut rest = plain.as_str();
        while let Some(at) = rest.find("](") {
            let after = &rest[at + 2..];
            let Some(end) = after.find(')') else { break };
            targets.push(after[..end].trim().to_owned());
            rest = &after[end + 1..];
        }
    }
    targets
}

/// GitHub-style anchors of the headings in a Markdown text.
fn anchors(text: &str) -> BTreeSet<String> {
    let mut anchors = BTreeSet::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || !line.starts_with('#') {
            continue;
        }
        let heading = line.trim_start_matches('#').trim();
        let slug: String = heading
            .to_lowercase()
            .chars()
            .filter_map(|c| match c {
                ' ' => Some('-'),
                c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
                _ => None,
            })
            .collect();
        anchors.insert(slug);
    }
    anchors
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_links_and_anchors() {
        let text = "# Index advisor (without requiring HypoPG)\n\nSee [a](b.md#c) and `[not](a link)`.\n```\n[x](y)\n```\n";
        assert_eq!(links(text), ["b.md#c"]);
        assert!(anchors(text).contains("index-advisor-without-requiring-hypopg"));
    }
}
