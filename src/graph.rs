//! The dependency graph: modules, deduplicated edges, cycles and metrics.

use std::collections::{HashSet, VecDeque};

use rustc_hash::FxHashMap as HashMap;
use std::path::{Path, PathBuf};

use petgraph::algo::tarjan_scc;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::Serialize;

use crate::config::Options;
use crate::scan::{ImportFlags, ScannedFile, Target, Work};

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
    /// npm package declared in more than one package.json section.
    #[serde(skip)]
    pub multi_type: bool,
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
    /// Module ids per kind (indexed by `ModuleKind as usize`).
    index: [HashMap<String, usize>; 4],
}

impl Graph {
    pub fn build(root: &Path, files: &[ScannedFile], work: Work, opts: &Options) -> Graph {
        let t = std::time::Instant::now();
        let mut g = Graph {
            root: root.to_path_buf(),
            modules: vec![],
            edges: vec![],
            out: vec![],
            inc: vec![],
            cycles: vec![],
            cycle_of: vec![],
            timings: Timings { walk_ms: work.walk_ms, parse_ms: work.parse_ms, graph_ms: 0.0 },
            index: Default::default(),
        };
        for f in files {
            let id = g.rel(&f.path);
            let m = g.intern(ModuleKind::Local, id);
            g.modules[m].scanned = true;
            g.modules[m].parse_errors = f.parse_errors;
        }

        // Scanned files are modules 0..n in order; look targets up by path to
        // avoid a path → string conversion per import.
        let by_path: HashMap<&Path, usize> = files.iter().enumerate().map(|(i, f)| (f.path.as_path(), i)).collect();
        let mut pkgs = PackageJsons::default();
        // Target → edge, for deduplicating imports within the current file.
        let mut seen: HashMap<usize, usize> = HashMap::default();
        let mut bases: Vec<Vec<&'static str>> = vec![];
        let filter = PathFilter::new(opts);
        for (from, f) in files.iter().enumerate() {
            seen.clear();
            for imp in &f.imports {
                if opts.ignore_type_only && imp.flags.type_only {
                    continue;
                }
                if filter.active() {
                    let keep = match &imp.target {
                        Target::Local(p) => filter.keep(&[&g.rel(p)]),
                        Target::Npm(pkg) => filter.keep(&[pkg, &format!("node_modules/{pkg}/")]),
                        Target::Builtin(n) => filter.keep(&[n]),
                        Target::Unresolved => filter.keep(&[&imp.specifier]),
                    };
                    if !keep {
                        continue;
                    }
                }
                let (to, base) = match &imp.target {
                    Target::Local(p) => match by_path.get(p.as_path()) {
                        Some(&i) => (i, vec!["local"]),
                        None => (g.intern(ModuleKind::Local, g.rel(p)), vec!["local"]),
                    },
                    Target::Npm(pkg) => {
                        let base = pkgs.classify(&f.path, pkg);
                        (g.intern_ref(ModuleKind::Npm, pkg), base)
                    }
                    Target::Builtin(n) => (g.intern_ref(ModuleKind::Builtin, n), vec!["core"]),
                    Target::Unresolved => {
                        (g.intern_ref(ModuleKind::Unresolved, &imp.specifier), vec!["unresolvable"])
                    }
                };
                match seen.get(&to) {
                    Some(&e) => g.edges[e].flags = g.edges[e].flags.merge(imp.flags),
                    None => {
                        seen.insert(to, g.edges.len());
                        bases.push(base);
                        g.edges.push(Edge {
                            from,
                            to,
                            specifier: imp.specifier.clone(),
                            flags: imp.flags,
                            types: vec![],
                            circular: false,
                            multi_type: false,
                        });
                    }
                }
            }
        }
        for (e, base) in g.edges.iter_mut().zip(bases) {
            e.multi_type = base.len() > 1;
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

    fn intern_ref(&mut self, kind: ModuleKind, id: &str) -> usize {
        match self.index[kind as usize].get(id) {
            Some(&i) => i,
            None => self.intern(kind, id.to_string()),
        }
    }

    fn intern(&mut self, kind: ModuleKind, id: String) -> usize {
        if let Some(&i) = self.index[kind as usize].get(&id) {
            return i;
        }
        let i = self.modules.len();
        self.index[kind as usize].insert(id.clone(), i);
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
        self.path_where(from, to, |e| !circular_only || e.circular, |_| true)
    }

    /// BFS for the shortest path `from → … → to` using only edges satisfying
    /// `edge_ok` and intermediate/end modules satisfying `node_ok`.
    pub fn path_where(
        &self,
        from: usize,
        to: usize,
        edge_ok: impl Fn(&Edge) -> bool,
        node_ok: impl Fn(usize) -> bool,
    ) -> Option<Vec<usize>> {
        let mut prev: HashMap<usize, usize> = HashMap::default();
        prev.insert(from, from);
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
                if !edge_ok(edge) || !node_ok(edge.to) {
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

    /// Modules reachable from `start` (forward) or reaching it (backward),
    /// following only edges satisfying `edge_ok`.
    pub fn closure_where(&self, start: usize, forward: bool, edge_ok: impl Fn(&Edge) -> bool) -> HashSet<usize> {
        let mut seen = HashSet::from([start]);
        let mut q = VecDeque::from([start]);
        while let Some(m) = q.pop_front() {
            let adj = if forward { &self.out[m] } else { &self.inc[m] };
            for &e in adj {
                let edge = &self.edges[e];
                let next = if forward { edge.to } else { edge.from };
                if edge_ok(edge) && seen.insert(next) {
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
        self.index[kind as usize].get(id).copied()
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

/// `base` is the kind of target — for npm packages every package.json
/// section declaring it (e.g. `["npm", "npm-dev"]`), primary first.
fn edge_types(base: Vec<&'static str>, f: ImportFlags) -> Vec<&'static str> {
    let mut t = base;
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
    if f.resource {
        t.push("resource");
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

    /// Every section declaring `name`, most significant first.
    fn kinds_of(&self, name: &str) -> Vec<&'static str> {
        [(&self.deps, "npm"), (&self.peer, "npm-peer"), (&self.optional, "npm-optional"), (&self.dev, "npm-dev")]
            .into_iter()
            .filter(|(set, _)| set.contains(name))
            .map(|(_, kind)| kind)
            .collect()
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
    /// Returns every dependency type the package is declared as, in the
    /// nearest package.json that declares it.
    fn classify(&mut self, file: &Path, pkg: &str) -> Vec<&'static str> {
        let types_pkg = match pkg.strip_prefix('@') {
            Some(s) => format!("@types/{}", s.replacen('/', "__", 1)),
            None => format!("@types/{pkg}"),
        };
        for dir in file.ancestors().skip(1) {
            if !self.by_dir.contains_key(dir) {
                self.by_dir.insert(dir.to_path_buf(), PackageDeps::read(&dir.join("package.json")));
            }
            let deps = &self.by_dir[dir];
            if let Some(d) = deps {
                let kinds = d.kinds_of(pkg);
                let kinds = if kinds.is_empty() { d.kinds_of(&types_pkg) } else { kinds };
                if !kinds.is_empty() {
                    return kinds;
                }
            }
        }
        vec!["npm-undeclared"]
    }
}

/// `include_only` / `exclude_path` from the options.
pub struct PathFilter {
    include: Option<fancy_regex::Regex>,
    exclude: Option<fancy_regex::Regex>,
}

impl PathFilter {
    /// Patterns were validated when the config was loaded.
    pub fn new(opts: &Options) -> Self {
        let re = |p: &Option<crate::config::Pat>| p.as_ref().and_then(|p| fancy_regex::Regex::new(&p.0).ok());
        PathFilter { include: re(&opts.include_only), exclude: re(&opts.exclude_path) }
    }

    pub fn active(&self) -> bool {
        self.include.is_some() || self.exclude.is_some()
    }

    /// Kept when some name is included and no name is excluded.
    pub fn keep(&self, names: &[&str]) -> bool {
        let hit = |r: &fancy_regex::Regex| names.iter().any(|n| r.is_match(n).unwrap_or(false));
        self.include.as_ref().is_none_or(hit) && !self.exclude.as_ref().is_some_and(hit)
    }
}
