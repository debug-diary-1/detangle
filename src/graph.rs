//! The dependency graph: modules, deduplicated edges, cycles and metrics.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use petgraph::algo::tarjan_scc;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::Serialize;

use crate::config::Options;
use crate::scan::{ImportFlags, Scan, Target};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModuleKind {
    Local,
    Npm,
    Builtin,
    Unresolved,
}

impl ModuleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ModuleKind::Local => "local",
            ModuleKind::Npm => "npm",
            ModuleKind::Builtin => "core",
            ModuleKind::Unresolved => "unresolved",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Module {
    pub id: String,
    pub kind: ModuleKind,
    /// Parsed by tangle (as opposed to e.g. a .css/.json file that was only imported).
    pub scanned: bool,
    pub parse_errors: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub specifier: String,
    pub flags: ImportFlags,
    /// Dependency types, e.g. `["npm-dev", "type-only"]`.
    pub types: Vec<&'static str>,
    pub circular: bool,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct Timings {
    pub walk_ms: f64,
    pub parse_ms: f64,
    pub graph_ms: f64,
}

pub struct Graph {
    pub root: PathBuf,
    pub modules: Vec<Module>,
    pub edges: Vec<Edge>,
    /// Outgoing / incoming edge indices per module.
    pub out: Vec<Vec<usize>>,
    pub inc: Vec<Vec<usize>>,
    /// Strongly connected components that form cycles, largest first.
    pub cycles: Vec<Vec<usize>>,
    pub cycle_of: Vec<Option<usize>>,
    pub timings: Timings,
    index: HashMap<(ModuleKind, String), usize>,
}

impl Graph {
    pub fn build(root: &Path, scan: Scan, opts: &Options) -> Graph {
        let t = std::time::Instant::now();
        let mut g = Graph {
            root: root.to_path_buf(),
            modules: vec![],
            edges: vec![],
            out: vec![],
            inc: vec![],
            cycles: vec![],
            cycle_of: vec![],
            timings: Timings { walk_ms: scan.walk_ms, parse_ms: scan.parse_ms, graph_ms: 0.0 },
            index: HashMap::new(),
        };
        for f in &scan.files {
            let id = g.rel(&f.path);
            let m = g.intern(ModuleKind::Local, id);
            g.modules[m].scanned = true;
            g.modules[m].parse_errors = f.parse_errors;
        }

        let mut pkgs = PackageJsons::default();
        let mut seen: HashMap<(usize, usize), usize> = HashMap::new();
        let mut bases: Vec<&'static str> = vec![];
        for f in &scan.files {
            let from = g.index[&(ModuleKind::Local, g.rel(&f.path))];
            for imp in &f.imports {
                let (kind, id, base) = match &imp.target {
                    Target::Local(p) => (ModuleKind::Local, g.rel(p), "local"),
                    Target::Npm(pkg) => (ModuleKind::Npm, pkg.clone(), pkgs.classify(&f.path, pkg)),
                    Target::Builtin(n) => (ModuleKind::Builtin, n.clone(), "core"),
                    Target::Unresolved => {
                        (ModuleKind::Unresolved, imp.specifier.clone(), "unresolvable")
                    }
                };
                let to = g.intern(kind, id);
                match seen.get(&(from, to)) {
                    Some(&e) => g.edges[e].flags = g.edges[e].flags.merge(imp.flags),
                    None => {
                        seen.insert((from, to), g.edges.len());
                        bases.push(base);
                        g.edges.push(Edge {
                            from,
                            to,
                            specifier: imp.specifier.clone(),
                            flags: imp.flags,
                            types: vec![],
                            circular: false,
                        });
                    }
                }
            }
        }
        for (e, base) in g.edges.iter_mut().zip(bases) {
            e.types = edge_types(base, e.flags);
        }

        g.out = vec![vec![]; g.modules.len()];
        g.inc = vec![vec![]; g.modules.len()];
        for (i, e) in g.edges.iter().enumerate() {
            g.out[e.from].push(i);
            g.inc[e.to].push(i);
        }
        g.find_cycles(opts.cycles_ignore_type_only);
        g.timings.graph_ms = t.elapsed().as_secs_f64() * 1000.0;
        g
    }

    fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&self.root).unwrap_or(p).to_string_lossy().replace('\\', "/")
    }

    fn intern(&mut self, kind: ModuleKind, id: String) -> usize {
        if let Some(&i) = self.index.get(&(kind, id.clone())) {
            return i;
        }
        let i = self.modules.len();
        self.index.insert((kind, id.clone()), i);
        self.modules.push(Module { id, kind, scanned: false, parse_errors: 0 });
        i
    }

    fn counts_for_cycles(&self, e: &Edge, ignore_type_only: bool) -> bool {
        !(ignore_type_only && e.flags.type_only)
            && self.modules[e.to].kind == ModuleKind::Local
    }

    fn find_cycles(&mut self, ignore_type_only: bool) {
        let mut dg = DiGraph::<(), ()>::with_capacity(self.modules.len(), self.edges.len());
        for _ in &self.modules {
            dg.add_node(());
        }
        for e in &self.edges {
            if self.counts_for_cycles(e, ignore_type_only) {
                dg.add_edge(NodeIndex::new(e.from), NodeIndex::new(e.to), ());
            }
        }
        let mut cycles: Vec<Vec<usize>> = tarjan_scc(&dg)
            .into_iter()
            .map(|c| c.into_iter().map(|n| n.index()).collect::<Vec<_>>())
            .filter(|c| c.len() > 1 || dg.contains_edge(NodeIndex::new(c[0]), NodeIndex::new(c[0])))
            .collect();
        for c in &mut cycles {
            c.sort_by(|a, b| self.modules[*a].id.cmp(&self.modules[*b].id));
        }
        cycles.sort_by(|a, b| {
            b.len().cmp(&a.len()).then_with(|| self.modules[a[0]].id.cmp(&self.modules[b[0]].id))
        });
        self.cycle_of = vec![None; self.modules.len()];
        for (ci, c) in cycles.iter().enumerate() {
            for &m in c {
                self.cycle_of[m] = Some(ci);
            }
        }
        for i in 0..self.edges.len() {
            let e = &self.edges[i];
            let circular = self.counts_for_cycles(e, ignore_type_only)
                && self.cycle_of[e.from].is_some()
                && self.cycle_of[e.from] == self.cycle_of[e.to];
            self.edges[i].circular = circular;
        }
        self.cycles = cycles;
    }

    /// Shortest cycle through `edge` (a circular edge): `[from, to, ..., from]`.
    pub fn cycle_path(&self, edge: usize) -> Vec<usize> {
        let e = &self.edges[edge];
        let mut path = vec![e.from];
        path.extend(self.path_between(e.to, e.from, true).unwrap_or_default());
        path
    }

    /// A shortest representative cycle for a strongly connected component.
    pub fn representative_cycle(&self, cycle: usize) -> Vec<usize> {
        let start = self.cycles[cycle][0];
        self.out[start]
            .iter()
            .filter(|&&e| self.edges[e].circular)
            .map(|&e| self.cycle_path(e))
            .min_by_key(|p| p.len())
            .unwrap_or_else(|| vec![start])
    }

    /// BFS for the shortest module path `from → … → to` (inclusive).
    /// With `circular_only`, only edges marked circular are followed.
    pub fn path_between(&self, from: usize, to: usize, circular_only: bool) -> Option<Vec<usize>> {
        let mut prev: HashMap<usize, usize> = HashMap::from([(from, from)]);
        let mut q = VecDeque::from([from]);
        while let Some(m) = q.pop_front() {
            if m == to {
                let mut path = vec![to];
                let mut cur = to;
                while cur != from {
                    cur = prev[&cur];
                    path.push(cur);
                }
                path.reverse();
                return Some(path);
            }
            for &e in &self.out[m] {
                let edge = &self.edges[e];
                if circular_only && !edge.circular {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(v) = prev.entry(edge.to) {
                    v.insert(m);
                    q.push_back(edge.to);
                }
            }
        }
        None
    }

    /// Every module reachable from `starts` following dependencies (`forward`)
    /// or dependents (`!forward`), including the starts themselves.
    pub fn closure(&self, starts: &[usize], forward: bool) -> HashSet<usize> {
        let mut seen: HashSet<usize> = starts.iter().copied().collect();
        let mut q: VecDeque<usize> = starts.iter().copied().collect();
        while let Some(m) = q.pop_front() {
            let adj = if forward { &self.out[m] } else { &self.inc[m] };
            for &e in adj {
                let next = if forward { self.edges[e].to } else { self.edges[e].from };
                if seen.insert(next) {
                    q.push_back(next);
                }
            }
        }
        seen
    }

    pub fn fan_in(&self, m: usize) -> usize {
        self.inc[m].len()
    }

    pub fn fan_out(&self, m: usize) -> usize {
        self.out[m].len()
    }

    /// Martin's instability: Ce / (Ca + Ce). 0 = stable, 1 = unstable.
    pub fn instability(&self, m: usize) -> f64 {
        let (i, o) = (self.fan_in(m), self.fan_out(m));
        if i + o == 0 { 0.0 } else { o as f64 / (i + o) as f64 }
    }

    pub fn is_orphan(&self, m: usize) -> bool {
        self.modules[m].kind == ModuleKind::Local && self.out[m].is_empty() && self.inc[m].is_empty()
    }

    pub fn find(&self, kind: ModuleKind, id: &str) -> Option<usize> {
        self.index.get(&(kind, id.to_string())).copied()
    }

    /// Resolve a user-typed module reference: exact id, root-relative path,
    /// then unique suffix/substring match.
    pub fn lookup(&self, query: &str) -> Result<usize, String> {
        let q = query.trim_start_matches("./");
        if let Some(i) = self.modules.iter().position(|m| m.id == q) {
            return Ok(i);
        }
        if let Ok(abs) = std::fs::canonicalize(query)
            && let Some(i) = self.find(ModuleKind::Local, &self.rel(&abs)) {
                return Ok(i);
            }
        let hits: Vec<usize> = (0..self.modules.len())
            .filter(|&i| self.modules[i].id.ends_with(q))
            .collect();
        let hits = if hits.is_empty() {
            (0..self.modules.len()).filter(|&i| self.modules[i].id.contains(q)).collect()
        } else {
            hits
        };
        match hits.as_slice() {
            [one] => Ok(*one),
            [] => Err(format!("no module matches '{query}'")),
            many => Err(format!(
                "'{query}' is ambiguous; matches {}{}",
                many.iter().take(5).map(|&i| self.modules[i].id.as_str()).collect::<Vec<_>>().join(", "),
                if many.len() > 5 { ", …" } else { "" }
            )),
        }
    }

    pub fn total_ms(&self) -> f64 {
        self.timings.walk_ms + self.timings.parse_ms + self.timings.graph_ms
    }

    pub fn local_count(&self) -> usize {
        self.modules.iter().filter(|m| m.kind == ModuleKind::Local).count()
    }
}

fn edge_types(base: &'static str, f: ImportFlags) -> Vec<&'static str> {
    let mut t = vec![base];
    if f.type_only {
        t.push("type-only");
    }
    if f.dynamic {
        t.push("dynamic");
    }
    if f.require {
        t.push("require");
    }
    if f.reexport {
        t.push("reexport");
    }
    t
}

#[derive(Default)]
struct PackageDeps {
    deps: HashSet<String>,
    dev: HashSet<String>,
    peer: HashSet<String>,
    optional: HashSet<String>,
}

impl PackageDeps {
    fn read(path: &Path) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        let keys = |field: &str| -> HashSet<String> {
            v.get(field)
                .and_then(|d| d.as_object())
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default()
        };
        Some(Self {
            deps: keys("dependencies"),
            dev: keys("devDependencies"),
            peer: keys("peerDependencies"),
            optional: keys("optionalDependencies"),
        })
    }

    fn kind_of(&self, name: &str) -> Option<&'static str> {
        if self.deps.contains(name) {
            Some("npm")
        } else if self.peer.contains(name) {
            Some("npm-peer")
        } else if self.optional.contains(name) {
            Some("npm-optional")
        } else if self.dev.contains(name) {
            Some("npm-dev")
        } else {
            None
        }
    }
}

/// Caches parsed package.json files by directory.
#[derive(Default)]
struct PackageJsons {
    by_dir: HashMap<PathBuf, Option<PackageDeps>>,
}

impl PackageJsons {
    /// Classifies `pkg` as imported from `file` by walking up through every
    /// enclosing package.json (so monorepo root deps count).
    fn classify(&mut self, file: &Path, pkg: &str) -> &'static str {
        let types_pkg = match pkg.strip_prefix('@') {
            Some(s) => format!("@types/{}", s.replacen('/', "__", 1)),
            None => format!("@types/{pkg}"),
        };
        for dir in file.ancestors().skip(1) {
            let deps = self
                .by_dir
                .entry(dir.to_path_buf())
                .or_insert_with(|| PackageDeps::read(&dir.join("package.json")));
            if let Some(d) = deps
                && let Some(k) = d.kind_of(pkg).or_else(|| d.kind_of(&types_pkg)) {
                    return k;
                }
        }
        "npm-undeclared"
    }
}
