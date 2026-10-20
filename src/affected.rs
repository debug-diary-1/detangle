//! Evidence for the affected command, following incoming edges once for all inputs.

use std::collections::VecDeque;

use detangle::graph::Graph;
use rustc_hash::FxHashMap;

pub struct Reason {
    pub seed: usize,
    /// The edge from this module toward the seed; roots have no edge.
    pub edge: Option<usize>,
}

pub struct Evidence {
    pub reached: FxHashMap<usize, Reason>,
}

impl Evidence {
    pub fn build(g: &Graph, seeds: &[usize]) -> Self {
        let mut seeds = seeds.to_vec();
        seeds.sort_unstable_by_key(|&m| &g.modules[m].id);
        seeds.dedup();
        let mut reached: FxHashMap<_, _> = seeds.iter().map(|&m| (m, Reason { seed: m, edge: None })).collect();
        let mut queue: VecDeque<_> = seeds.into();
        while let Some(m) = queue.pop_front() {
            let seed = reached[&m].seed;
            let mut incoming = g.inc[m].clone();
            incoming.sort_unstable_by_key(|&e| &g.modules[g.edges[e].from].id);
            for e in incoming {
                let next = g.edges[e].from;
                if let std::collections::hash_map::Entry::Vacant(entry) = reached.entry(next) {
                    entry.insert(Reason { seed, edge: Some(e) });
                    queue.push_back(next);
                }
            }
        }
        Self { reached }
    }

    pub fn print_path(&self, g: &Graph, mut m: usize) {
        print!("{}", display_path(&g.modules[m].id));
        while let Some(e) = self.reached[&m].edge {
            m = g.edges[e].to;
            print!(" → {}", display_path(&g.modules[m].id));
        }
        println!(" (changed)");
    }
}

/// Quote separators and escape controls so every explanation occupies one line.
pub fn display_path(path: &str) -> String {
    if path.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\\' | '→')) {
        format!("{path:?}")
    } else {
        path.to_owned()
    }
}

#[derive(serde::Serialize)]
pub struct Limitation {
    pub code: &'static str,
    pub message: &'static str,
    pub paths: Vec<String>,
    pub count: Option<usize>,
}

pub fn limitations(
    g: &Graph,
    options: &detangle::config::Options,
    git_comparison: bool,
    deleted: &[String],
    configuration: &[String],
    unmatched: &[String],
) -> Vec<Limitation> {
    use detangle::graph::ModuleKind;
    let mut result = vec![];
    let mut add = |code, message, paths: Vec<String>, count| {
        let mut paths = paths;
        paths.sort();
        paths.dedup();
        result.push(Limitation { code, message, paths, count });
    };
    if git_comparison {
        add("historical-edges-not-analyzed", "Only the current graph is analyzed; removed historical edges are unavailable.", vec![], None);
    }
    if !deleted.is_empty() {
        add("deleted-inputs-not-followed", "Deleted inputs cannot seed historical traversal.", deleted.to_vec(), None);
    }
    if !configuration.is_empty() {
        add("configuration-impact-not-expanded", "Configuration inputs do not broaden selection.", configuration.to_vec(), None);
    }
    if !unmatched.is_empty() {
        add("unmatched-inputs", "Inputs did not match graph modules; their impact is unknown.", unmatched.to_vec(), None);
    }
    if graph_restrictions(options).next().is_some() {
        add("graph-restrictions", "Active graph restrictions can hide dependency chains.", vec![], None);
    }
    let unresolved: Vec<_> = g.edges.iter().filter(|e| g.modules[e.to].kind == ModuleKind::Unresolved).collect();
    if !unresolved.is_empty() {
        add("unresolved-imports", "Imports could not be resolved in the current graph.",
            unresolved.iter().map(|e| g.modules[e.from].id.clone()).collect(), Some(unresolved.len()));
    }
    let errors: usize = g.modules.iter().map(|m| m.parse_errors).sum();
    if errors > 0 {
        add("parse-errors", "Parse errors can leave dependencies unavailable.",
            g.modules.iter().filter(|m| m.parse_errors > 0).map(|m| m.id.clone()).collect(), Some(errors));
    }
    add("dynamic-relationships-not-guaranteed", "Represented literal dynamic imports participate; runtime-computed relationships are not guaranteed.", vec![], None);
    result
}

pub fn print_scope(g: &Graph, options: &detangle::config::Options, filter: Option<&str>, limitations: &[Limitation]) {
    eprintln!("scope: current-graph static dependency reachability; root={}", display_path(&g.root.to_string_lossy()));
    if let Some(filter) = filter {
        eprintln!("output filter={filter:?}");
    }
    for (name, value) in graph_restrictions(options) {
        match value {
            Restriction::Pattern(pattern) => eprintln!("graph restriction: {name}={pattern:?}"),
            Restriction::Enabled(enabled) => eprintln!("graph restriction: {name}={enabled}"),
        }
    }
    for limit in limitations {
        eprint!("limitation [{}]: {}", limit.code, limit.message);
        if let Some(count) = limit.count {
            eprint!(" count={count}");
        }
        for path in &limit.paths {
            eprint!(" {}", display_path(path));
        }
        eprintln!();
    }
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum Restriction<'a> {
    Pattern(&'a str),
    Enabled(bool),
}

pub fn graph_restrictions(options: &detangle::config::Options) -> impl Iterator<Item = (&'static str, Restriction<'_>)> {
    [("exclude_path", &options.exclude_path), ("include_only", &options.include_only), ("do_not_follow", &options.do_not_follow)]
        .into_iter()
        .filter_map(|(name, value)| value.as_ref().filter(|p| !p.0.is_empty()).map(|p| (name, Restriction::Pattern(p.0.as_str()))))
        .chain(options.ignore_type_only.then_some(("ignore_type_only", Restriction::Enabled(true))))
        .chain(options.exclude_dynamic.then_some(("exclude_dynamic", Restriction::Enabled(true))))
}

#[derive(Default, serde::Serialize)]
pub struct Origins {
    pub explicit: bool,
    pub git: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Classification {
    Module,
    Deleted,
    Configuration,
    Unmatched,
}

#[derive(serde::Serialize)]
pub struct Input {
    pub origins: Origins,
    pub classification: Classification,
    pub module: Option<String>,
    pub deleted: bool,
}

impl Default for Input {
    fn default() -> Self {
        Self { origins: Origins::default(), classification: Classification::Unmatched, module: None, deleted: false }
    }
}

#[derive(serde::Serialize)]
pub struct ReportedInput<'a> {
    pub path: &'a str,
    #[serde(flatten)]
    pub input: &'a Input,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Comparison<'a> {
    pub mode: &'static str,
    pub requested_reference: Option<&'a str>,
    pub resolved_base: Option<&'a str>,
    pub head: Option<&'a str>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Scope<'a> {
    pub root: &'a std::path::Path,
    pub basis: &'static str,
    pub graph_restrictions: std::collections::BTreeMap<&'static str, Restriction<'a>>,
    pub output_filter: Option<&'a str>,
}

#[derive(serde::Serialize)]
pub struct Counts {
    pub inputs: usize,
    pub seeds: usize,
    pub affected: usize,
    pub displayed: usize,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report<'a> {
    pub schema_version: usize,
    pub comparison: Comparison<'a>,
    pub scope: Scope<'a>,
    pub inputs: Vec<ReportedInput<'a>>,
    pub counts: Counts,
    pub limitations: &'a [Limitation],
}

/// Serialize paths directly from predecessors. Even a long chain needs no
/// duplicated path buffers; only the graph, traversal and result indices live.
pub fn write_json(g: &Graph, evidence: &Evidence, hit: &[usize], report: Report<'_>) -> anyhow::Result<()> {
    use std::io::Write;
    #[derive(serde::Serialize)]
    struct Document<'a> {
        #[serde(flatten)]
        report: Report<'a>,
        affected: Results<'a>,
    }
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    serde_json::to_writer(&mut out, &Document { report, affected: Results { g, evidence, hit } })?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

struct Results<'a> {
    g: &'a Graph,
    evidence: &'a Evidence,
    hit: &'a [usize],
}

impl serde::Serialize for Results<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        #[derive(serde::Serialize)]
        struct Record<'a> {
            module: &'a str,
            reason: &'static str,
            seed: &'a str,
            path: Chain<'a>,
            edges: Chain<'a>,
        }
        let mut seq = serializer.serialize_seq(Some(self.hit.len()))?;
        for &m in self.hit {
            let reason = &self.evidence.reached[&m];
            seq.serialize_element(&Record {
                module: &self.g.modules[m].id,
                reason: if reason.edge.is_none() { "changed" } else { "dependent" },
                seed: &self.g.modules[reason.seed].id,
                path: Chain { g: self.g, evidence: self.evidence, start: m, edges: false },
                edges: Chain { g: self.g, evidence: self.evidence, start: m, edges: true },
            })?;
        }
        seq.end()
    }
}

struct Chain<'a> {
    g: &'a Graph,
    evidence: &'a Evidence,
    start: usize,
    edges: bool,
}

impl serde::Serialize for Chain<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Edge<'a> {
            from: &'a str,
            to: &'a str,
            specifiers: Vec<&'a str>,
            dependency_types: Vec<&'a str>,
        }
        let mut seq = serializer.serialize_seq(None)?;
        let mut m = self.start;
        loop {
            if !self.edges { seq.serialize_element(&self.g.modules[m].id)?; }
            let Some(e) = self.evidence.reached[&m].edge else { break; };
            let e = &self.g.edges[e];
            if self.edges {
                let mut specifiers: Vec<_> = e.imports().into_iter().map(|(s, _)| s).collect();
                specifiers.sort_unstable();
                specifiers.dedup();
                let mut dependency_types = e.types.to_vec();
                dependency_types.sort_unstable();
                dependency_types.dedup();
                seq.serialize_element(&Edge { from: &self.g.modules[m].id, to: &self.g.modules[e.to].id, specifiers, dependency_types })?;
            }
            m = e.to;
        }
        seq.end()
    }
}
