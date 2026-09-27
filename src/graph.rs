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
    /// Tags from the module's group (or the group's own, on the group graph).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub specifier: String,
    /// When the file imports the target more than once: every import
    /// (specifier and kind). `flags` merges them.
    #[serde(skip)]
    pub each: Vec<(String, ImportFlags)>,
    pub flags: ImportFlags,
    /// Dependency types, e.g. `["npm-dev", "type-only"]`.
    pub types: Vec<&'static str>,
    pub circular: bool,
    /// npm package declared in more than one package.json section.
    #[serde(skip)]
    pub multi_type: bool,
}

impl Edge {
    /// The individual imports behind the edge: (specifier, kind).
    pub fn imports(&self) -> Vec<(&str, ImportFlags)> {
        if self.each.is_empty() {
            vec![(self.specifier.as_str(), self.flags)]
        } else {
            self.each.iter().map(|(s, f)| (s.as_str(), *f)).collect()
        }
    }
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
    cycles_ignore_type_only: bool,
    /// The folder-level graph, built on first use.
    folders: std::sync::OnceLock<Box<Graph>>,
    /// Folder graphs only: (afferent, efferent) module-level coupling per node.
    couplings: Vec<(u32, u32)>,
    /// Group instance of each module (index into `groups()`), if any.
    pub group_of: Vec<Option<usize>>,
    groups: Option<Box<Graph>>,
    /// Group graphs only: each node's root path.
    pub group_roots: Vec<String>,
    /// Group graphs only: the module-graph edges behind each edge.
    pub members: Vec<Vec<usize>>,
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
            cycles_ignore_type_only: opts.cycles_ignore_type_only,
            folders: Default::default(),
            couplings: vec![],
            group_of: vec![],
            groups: None,
            group_roots: vec![],
            members: vec![],
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
                    Some(&e) => {
                        let edge = &mut g.edges[e];
                        if edge.each.is_empty() {
                            edge.each.push((edge.specifier.clone(), edge.flags));
                        }
                        edge.each.push((imp.specifier.clone(), imp.flags));
                        edge.flags = edge.flags.merge(imp.flags);
                    }
                    None => {
                        seen.insert(to, g.edges.len());
                        bases.push(base);
                        g.edges.push(Edge {
                            from,
                            to,
                            specifier: imp.specifier.clone(),
                            each: vec![],
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
            // Bundling isn't a section of its own.
            e.multi_type = base.iter().filter(|t| **t != "npm-bundled").count() > 1;
            e.types = edge_types(base, e.flags);
        }

        g.link();
        g.timings.graph_ms = t.elapsed().as_secs_f64() * 1000.0;
        g
    }

    /// Builds adjacency lists and finds cycles once modules and edges are set.
    fn link(&mut self) {
        self.out = vec![vec![]; self.modules.len()];
        self.inc = vec![vec![]; self.modules.len()];
        for (i, e) in self.edges.iter().enumerate() {
            self.out[e.from].push(i);
            self.inc[e.to].push(i);
        }
        self.find_cycles(self.cycles_ignore_type_only);
    }

    /// Assigns modules to groups (the first matching definition, or with
    /// `deepest` the one matching the longest path), gives them their
    /// group's tags, and builds the group graph.
    pub fn assign_groups(&mut self, defs: &[crate::groups::Group], deepest: bool) {
        let mut g = Graph {
            root: self.root.clone(),
            modules: vec![],
            edges: vec![],
            out: vec![],
            inc: vec![],
            cycles: vec![],
            cycle_of: vec![],
            timings: self.timings,
            index: Default::default(),
            cycles_ignore_type_only: self.cycles_ignore_type_only,
            folders: Default::default(),
            couplings: vec![],
            group_of: vec![],
            groups: None,
            group_roots: vec![],
            members: vec![],
        };
        self.group_of = vec![None; self.modules.len()];
        if !defs.is_empty() {
            for m in 0..self.modules.len() {
                if self.modules[m].kind != ModuleKind::Local {
                    continue;
                }
                let id = &self.modules[m].id;
                let found = if deepest {
                    // Longest root wins; among equals, the first definition.
                    defs.iter().filter_map(|d| d.instance(id).map(|i| (d, i))).rev().max_by_key(|(_, (_, root))| root.len())
                } else {
                    defs.iter().find_map(|d| d.instance(id).map(|i| (d, i)))
                };
                let Some((def, (label, root))) = found else {
                    continue;
                };
                let i = match g.index[ModuleKind::Local as usize].get(&label) {
                    Some(&i) => i,
                    None => {
                        let i = g.intern(ModuleKind::Local, label);
                        g.modules[i].tags = def.tags.clone();
                        g.group_roots.push(root);
                        i
                    }
                };
                g.modules[i].scanned |= self.modules[m].scanned;
                self.modules[m].tags = def.tags.clone();
                self.group_of[m] = Some(i);
            }
            let mut seen: HashMap<(usize, usize), usize> = HashMap::default();
            for (mi, e) in self.edges.iter().enumerate() {
                let (Some(a), Some(b)) = (self.group_of[e.from], self.group_of[e.to]) else { continue };
                if a == b {
                    continue;
                }
                match seen.get(&(a, b)) {
                    Some(&i) => {
                        g.members[i].push(mi);
                        let ge = &mut g.edges[i];
                        ge.flags = ge.flags.merge(e.flags);
                        for t in &e.types {
                            if !ge.types.contains(t) {
                                ge.types.push(t);
                            }
                        }
                    }
                    None => {
                        seen.insert((a, b), g.edges.len());
                        g.members.push(vec![mi]);
                        g.edges.push(Edge { from: a, to: b, circular: false, each: vec![], ..e.clone() });
                    }
                }
            }
            for ge in &mut g.edges {
                ge.types.retain(|t| !IMPORT_KIND_TYPES.contains(t));
                ge.types = edge_types(std::mem::take(&mut ge.types), ge.flags);
            }
        }
        g.link();
        self.groups = Some(Box::new(g));
    }

    /// The group graph (empty until `assign_groups`).
    pub fn groups(&self) -> &Graph {
        static EMPTY: std::sync::OnceLock<Graph> = std::sync::OnceLock::new();
        match &self.groups {
            Some(g) => g,
            None => EMPTY.get_or_init(|| Graph {
                root: PathBuf::new(),
                modules: vec![],
                edges: vec![],
                out: vec![],
                inc: vec![],
                cycles: vec![],
                cycle_of: vec![],
                timings: Timings::default(),
                index: Default::default(),
                cycles_ignore_type_only: true,
                folders: Default::default(),
                couplings: vec![],
                group_of: vec![],
                groups: None,
                group_roots: vec![],
                members: vec![],
            }),
        }
    }

    /// The folder a module belongs to: `src/features/cart` for
    /// `src/features/cart/cart.ts`, `.` for root-level files.
    pub fn folder_of(&self, m: usize) -> Option<String> {
        let module = &self.modules[m];
        (module.kind == ModuleKind::Local).then(|| match dir_of(&module.id) {
            "" => ".".to_string(),
            d => d.to_string(),
        })
    }

    /// The folder-level graph. Every directory is a node standing for its
    /// whole subtree, so `src/features/cart` includes its subfolders:
    ///
    /// * a module edge `a → b` makes every folder containing `a` but not `b`
    ///   depend on `b`'s own folder (npm packages are `node_modules/<name>`);
    /// * instability uses module-level coupling across the folder boundary
    ///   (Ce = edges leaving the subtree, Ca = edges entering it).
    ///
    /// Cycles are found exactly (strongly connected components), as for modules.
    pub fn folders(&self) -> &Graph {
        self.folders.get_or_init(|| {
            let mut f = Graph {
                root: self.root.clone(),
                modules: vec![],
                edges: vec![],
                out: vec![],
                inc: vec![],
                cycles: vec![],
                cycle_of: vec![],
                timings: self.timings,
                index: Default::default(),
                cycles_ignore_type_only: self.cycles_ignore_type_only,
                folders: Default::default(),
                couplings: vec![],
                group_of: vec![],
                groups: None,
                group_roots: vec![],
                members: vec![],
            };
            for (m, module) in self.modules.iter().enumerate() {
                if module.kind != ModuleKind::Local {
                    continue;
                }
                for x in ancestors(dir_of(&module.id)) {
                    let i = f.intern_ref(ModuleKind::Local, x);
                    f.modules[i].scanned |= self.modules[m].scanned;
                }
            }
            let mut ce: HashMap<usize, u32> = HashMap::default();
            let mut ca: HashMap<usize, u32> = HashMap::default();
            let mut seen: HashMap<(usize, usize), usize> = HashMap::default();
            for e in &self.edges {
                let a = &self.modules[e.from];
                let b = &self.modules[e.to];
                if a.kind != ModuleKind::Local {
                    continue;
                }
                let (kind, target) = match b.kind {
                    ModuleKind::Local => (ModuleKind::Local, match dir_of(&b.id) { "" => ".".to_string(), d => d.to_string() }),
                    ModuleKind::Npm => (ModuleKind::Npm, format!("node_modules/{}", b.id)),
                    k => (k, b.id.clone()),
                };
                let t = f.intern(kind, target);
                for x in ancestors(dir_of(&a.id)) {
                    if b.kind == ModuleKind::Local && inside(&b.id, x) {
                        continue;
                    }
                    let xi = f.index[ModuleKind::Local as usize][x];
                    *ce.entry(xi).or_default() += 1;
                    match seen.get(&(xi, t)) {
                        Some(&i) => {
                            let fe = &mut f.edges[i];
                            fe.flags = fe.flags.merge(e.flags);
                            fe.multi_type |= e.multi_type;
                            for ty in &e.types {
                                if !fe.types.contains(ty) {
                                    fe.types.push(ty);
                                }
                            }
                        }
                        None => {
                            seen.insert((xi, t), f.edges.len());
                            f.edges.push(Edge { from: xi, to: t, circular: false, each: vec![], ..e.clone() });
                        }
                    }
                }
                if b.kind == ModuleKind::Local {
                    for y in ancestors(dir_of(&b.id)) {
                        if !inside(&a.id, y) {
                            *ca.entry(f.index[ModuleKind::Local as usize][y]).or_default() += 1;
                        }
                    }
                } else {
                    *ca.entry(t).or_default() += 1;
                }
            }
            // `type-only` etc. describe the merged edge, not any one import.
            for fe in &mut f.edges {
                fe.types.retain(|t| !matches!(*t, "type-only" | "dynamic" | "require" | "reexport" | "resource"));
                fe.types = edge_types(std::mem::take(&mut fe.types), fe.flags);
            }
            f.couplings = (0..f.modules.len())
                .map(|i| (ca.get(&i).copied().unwrap_or(0), ce.get(&i).copied().unwrap_or(0)))
                .collect();
            f.link();
            Box::new(f)
        })
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
        self.modules.push(Module { id, kind, scanned: false, parse_errors: 0, tags: vec![] });
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
        let (i, o) = match self.couplings.get(m) {
            Some(&(ca, ce)) => (ca as usize, ce as usize),
            None => (self.fan_in(m), self.fan_out(m)),
        };
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
    if f.amd {
        t.push("amd");
    }
    if f.jsdoc {
        t.push("jsdoc");
    }
    if f.triple_slash {
        t.push("triple-slash");
    }
    if f.exotic != 0 {
        t.push("exotic-require");
    }
    if f.builtin_call {
        t.push("process-get-builtin-module");
    }
    t
}

/// Dependency types derived from how a module is imported (not what it is).
pub const IMPORT_KIND_TYPES: &[&str] =
    &["type-only", "dynamic", "require", "reexport", "resource", "amd", "jsdoc", "triple-slash", "exotic-require", "process-get-builtin-module"];

#[derive(Default)]
struct PackageDeps {
    deps: HashSet<String>,
    dev: HashSet<String>,
    peer: HashSet<String>,
    optional: HashSet<String>,
    /// `bundledDependencies` / `bundleDependencies`.
    bundled: HashSet<String>,
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
            bundled: ["bundledDependencies", "bundleDependencies"]
                .iter()
                .find_map(|k| v.get(*k).and_then(|b| b.as_array()))
                .map(|a| a.iter().filter_map(|n| n.as_str().map(String::from)).collect())
                .unwrap_or_default(),
        })
    }

    /// Every section declaring `name`, most significant first.
    fn kinds_of(&self, name: &str) -> Vec<&'static str> {
        [(&self.deps, "npm"), (&self.peer, "npm-peer"), (&self.optional, "npm-optional"), (&self.dev, "npm-dev")]
            .into_iter()
            .filter(|(set, _)| set.contains(name))
            .map(|(_, kind)| kind)
            .chain(self.bundled.contains(name).then_some("npm-bundled"))
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

/// `src/a/b.ts` → `src/a`; root-level → "".
fn dir_of(id: &str) -> &str {
    id.rfind('/').map_or("", |i| &id[..i])
}

/// `src/a/b` → [`src/a/b`, `src/a`, `src`].
fn ancestors(dir: &str) -> impl Iterator<Item = &str> {
    std::iter::successors((!dir.is_empty()).then_some(dir), |d| {
        let p = dir_of(d);
        (!p.is_empty()).then_some(p)
    })
}

/// Is `id` somewhere below `folder`?
fn inside(id: &str, folder: &str) -> bool {
    id.len() > folder.len() && id.starts_with(folder) && id.as_bytes()[folder.len()] == b'/'
}
