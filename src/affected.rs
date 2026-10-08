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
        eprintln!("graph restriction: {name}={value:?}");
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

pub fn graph_restrictions(options: &detangle::config::Options) -> impl Iterator<Item = (&'static str, &str)> {
    [("exclude_path", &options.exclude_path), ("include_only", &options.include_only), ("do_not_follow", &options.do_not_follow)]
        .into_iter()
        .filter_map(|(name, value)| value.as_ref().filter(|p| !p.0.is_empty()).map(|p| (name, p.0.as_str())))
}
