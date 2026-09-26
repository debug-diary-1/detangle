//! Evaluates `[[forbidden]]` rules against the graph.

use std::collections::HashSet;

use rustc_hash::FxHashMap as HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};

use crate::config::{Rule, Severity};
use crate::graph::{Graph, ModuleKind};

#[derive(Debug, Clone)]
pub struct Violation {
    pub rule: String,
    pub severity: Severity,
    pub comment: Option<String>,
    pub from: usize,
    pub to: Option<usize>,
    /// For circular violations: the cycle, as module indices.
    pub cycle: Vec<usize>,
}

/// A `to.path`-style pattern that may reference `from.path` capture groups.
enum Pattern {
    Static(Regex),
    Template(String),
}

impl Pattern {
    fn new(src: &str, rule: &str) -> Result<Self> {
        let is_template = src.as_bytes().windows(2).any(|w| w[0] == b'$' && w[1].is_ascii_digit());
        if is_template {
            Regex::new(&substitute(src, None))
                .with_context(|| format!("rule '{rule}': invalid regex {src:?}"))?;
            Ok(Pattern::Template(src.to_string()))
        } else {
            Ok(Pattern::Static(
                Regex::new(src).with_context(|| format!("rule '{rule}': invalid regex {src:?}"))?,
            ))
        }
    }

    fn is_match(&self, s: &str, caps: Option<&Captures>, cache: &mut HashMap<String, Regex>) -> bool {
        match self {
            Pattern::Static(r) => r.is_match(s),
            Pattern::Template(t) => {
                let src = substitute(t, caps);
                let re = cache
                    .entry(src)
                    .or_insert_with_key(|k| Regex::new(k).expect("validated at compile time"));
                re.is_match(s)
            }
        }
    }
}

/// Replaces `$1`..`$9` with the (regex-escaped) capture group values.
fn substitute(template: &str, caps: Option<&Captures>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$'
            && let Some(d) = chars.peek().and_then(|d| d.to_digit(10)) {
                chars.next();
                let v = caps.and_then(|c| c.get(d as usize)).map_or("", |m| m.as_str());
                out.push_str(&regex::escape(v));
                continue;
            }
        out.push(c);
    }
    out
}

struct Compiled<'r> {
    rule: &'r Rule,
    from_path: Option<Regex>,
    from_not: Option<Regex>,
    to_path: Option<Pattern>,
    to_not: Option<Pattern>,
}

fn compile(rule: &Rule) -> Result<Compiled<'_>> {
    let re = |s: &Option<String>| -> Result<Option<Regex>> {
        s.as_deref()
            .map(|p| Regex::new(p).with_context(|| format!("rule '{}': invalid regex {p:?}", rule.name)))
            .transpose()
    };
    let pat = |s: &Option<String>| s.as_deref().map(|p| Pattern::new(p, &rule.name)).transpose();
    if rule.to.reachable.is_some() && rule.from.path.is_none() {
        bail!("rule '{}': `to.reachable` needs `from.path` to name the entry points", rule.name);
    }
    Ok(Compiled {
        rule,
        from_path: re(&rule.from.path)?,
        from_not: re(&rule.from.path_not)?,
        to_path: pat(&rule.to.path)?,
        to_not: pat(&rule.to.path_not)?,
    })
}

pub fn validate(rules: &[Rule]) -> Result<()> {
    rules.iter().try_for_each(|r| compile(r).map(drop))
}

pub fn evaluate(g: &Graph, rules: &[Rule]) -> Result<Vec<Violation>> {
    let mut out = vec![];
    let mut cache = HashMap::default();
    for rule in rules.iter().filter(|r| r.severity != Severity::Off) {
        let c = compile(rule)?;
        let violation = |from, to, cycle| Violation {
            rule: rule.name.clone(),
            severity: rule.severity,
            comment: rule.comment.clone(),
            from,
            to,
            cycle,
        };

        if rule.from.orphan == Some(true) {
            for m in 0..g.modules.len() {
                if g.is_orphan(m) && c.source_matches(&g.modules[m].id).is_some() {
                    out.push(violation(m, None, vec![]));
                }
            }
        } else if let Some(reachable) = rule.to.reachable {
            let entries: Vec<usize> = (0..g.modules.len())
                .filter(|&m| g.modules[m].kind == ModuleKind::Local)
                .filter(|&m| c.source_matches(&g.modules[m].id).is_some())
                .collect();
            if entries.is_empty() {
                continue;
            }
            let reach = g.closure(&entries, true);
            for m in 0..g.modules.len() {
                let module = &g.modules[m];
                if module.kind != ModuleKind::Local || !module.scanned || entries.contains(&m) {
                    continue;
                }
                if reach.contains(&m) == reachable && c.to_path_matches(&module.id, None, &mut cache) {
                    out.push(violation(m, None, vec![]));
                }
            }
        } else {
            // Match `from` once per module, then test its outgoing edges.
            for m in 0..g.modules.len() {
                if g.out[m].is_empty() {
                    continue;
                }
                let Some(caps) = c.source_matches(&g.modules[m].id) else { continue };
                for &i in &g.out[m] {
                    let e = &g.edges[i];
                    if c.edge_matches(g, i, caps.as_ref(), &mut cache) {
                        let cycle = if e.circular { g.cycle_path(i) } else { vec![] };
                        out.push(violation(e.from, Some(e.to), cycle));
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.rule.cmp(&b.rule))
            .then_with(|| g.modules[a.from].id.cmp(&g.modules[b.from].id))
    });
    Ok(out)
}

impl Compiled<'_> {
    /// `None` = no match; `Some(caps)` = match, with `from.path` captures if any.
    fn source_matches<'s>(&self, id: &'s str) -> Option<Option<Captures<'s>>> {
        if self.from_not.as_ref().is_some_and(|r| r.is_match(id)) {
            return None;
        }
        match &self.from_path {
            Some(r) => r.captures(id).map(Some),
            None => Some(None),
        }
    }

    fn to_path_matches(&self, id: &str, caps: Option<&Captures>, cache: &mut HashMap<String, Regex>) -> bool {
        self.to_path.as_ref().is_none_or(|p| p.is_match(id, caps, cache))
            && !self.to_not.as_ref().is_some_and(|p| p.is_match(id, caps, cache))
    }

    fn edge_matches(&self, g: &Graph, edge: usize, caps: Option<&Captures>, cache: &mut HashMap<String, Regex>) -> bool {
        let e = &g.edges[edge];
        let to = &g.modules[e.to];
        let t = &self.rule.to;
        let is = |want: Option<bool>, actual: bool| want.is_none_or(|w| w == actual);
        is(t.circular, e.circular)
            && is(t.could_not_resolve, to.kind == ModuleKind::Unresolved)
            && is(t.type_only, e.flags.type_only)
            && is(t.dynamic, e.flags.dynamic)
            && t.dependency_types.as_ref().is_none_or(|l| e.types.iter().any(|x| l.iter().any(|y| y == x)))
            && !t.dependency_types_not.as_ref().is_some_and(|l| e.types.iter().any(|x| l.iter().any(|y| y == x)))
            && is(
                t.more_unstable,
                to.kind == ModuleKind::Local && g.instability(e.to) > g.instability(e.from),
            )
            && self.to_path_matches(&to.id, caps, cache)
    }
}

/// Known violations recorded with `--write-baseline`; matching ones are
/// suppressed so a legacy codebase can adopt rules incrementally.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Hash, Clone)]
pub struct BaselineEntry {
    pub rule: String,
    pub from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

fn baseline_key(g: &Graph, v: &Violation) -> BaselineEntry {
    BaselineEntry {
        rule: v.rule.clone(),
        from: g.modules[v.from].id.clone(),
        to: v.to.map(|t| g.modules[t].id.clone()),
    }
}

pub fn write_baseline(g: &Graph, vs: &[Violation], path: &Path) -> Result<()> {
    let entries: Vec<_> = vs.iter().map(|v| baseline_key(g, v)).collect();
    std::fs::write(path, serde_json::to_string_pretty(&entries)? + "\n")?;
    Ok(())
}

/// Removes baselined violations; returns how many were suppressed.
pub fn apply_baseline(g: &Graph, vs: &mut Vec<Violation>, path: &Path) -> Result<usize> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let known: HashSet<BaselineEntry> = serde_json::from_str::<Vec<_>>(&text)
        .with_context(|| format!("parsing {}", path.display()))?
        .into_iter()
        .collect();
    let before = vs.len();
    vs.retain(|v| !known.contains(&baseline_key(g, v)));
    Ok(before - vs.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_escaped_captures() {
        let re = Regex::new("^src/features/([^/]+)/").unwrap();
        let caps = re.captures("src/features/a.b/x.ts").unwrap();
        assert_eq!(substitute("^src/features/$1/", Some(&caps)), r"^src/features/a\.b/");
        assert_eq!(substitute("end$", Some(&caps)), "end$");
    }
}
