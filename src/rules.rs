//! Evaluates `forbidden`, `allowed` and `required` rules against the graph.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fancy_regex::Regex;
use rustc_hash::FxHashMap as HashMap;
use serde::{Deserialize, Serialize};

use crate::config::{Config, FromSpec, ModuleSpec, Pat, PathSpec, Rule, Scope, Severity, ToSpec};
use crate::graph::{Edge, Graph, ModuleKind};
use crate::scan::is_bare;

pub const NOT_IN_ALLOWED: &str = "not-in-allowed";

#[derive(Debug, Clone)]
pub struct Violation {
    pub rule: String,
    pub severity: Severity,
    pub comment: Option<String>,
    /// Which graph `from` / `to` / `cycle` index into.
    pub scope: Scope,
    pub from: usize,
    pub to: Option<usize>,
    /// For circular violations: the cycle, as module indices.
    pub cycle: Vec<usize>,
}

impl Violation {
    /// The graph this violation's indices refer to.
    pub fn graph<'g>(&self, g: &'g Graph) -> &'g Graph {
        match self.scope {
            Scope::Module => g,
            Scope::Folder => g.folders(),
            Scope::Group => g.groups(),
        }
    }

    pub fn source_id<'g>(&self, g: &'g Graph) -> &'g str {
        &self.graph(g).modules[self.from].id
    }

    pub fn target_id<'g>(&self, g: &'g Graph) -> Option<&'g str> {
        self.to.map(|t| self.graph(g).modules[t].id.as_str())
    }

    pub fn cycle_ids<'g>(&self, g: &'g Graph) -> Vec<&'g str> {
        let sg = self.graph(g);
        self.cycle.iter().map(|&m| sg.modules[m].id.as_str()).collect()
    }
}

/// Dependency types a rule may name, including alternative spellings used by
/// imported JavaScript configs.
pub fn canonical_type(name: &str) -> Option<&'static str> {
    Some(match name {
        "local" => "local",
        "npm" => "npm",
        "npm-dev" => "npm-dev",
        "npm-peer" => "npm-peer",
        "npm-optional" => "npm-optional",
        "npm-undeclared" | "npm-no-pkg" | "npm-unknown" => "npm-undeclared",
        "core" => "core",
        "unresolvable" | "unknown" | "undetermined" => "unresolvable",
        "type-only" | "type-import" | "pre-compilation-only" => "type-only",
        "dynamic" | "dynamic-import" => "dynamic",
        "require" | "exotic-require" | "import-equals" => "require",
        "reexport" | "export" => "reexport",
        "resource" => "resource",
        "relative" => "relative",
        "import" => "import",
        "deprecated" => "deprecated",
        "aliased" | "aliased-tsconfig" | "aliased-tsconfig-paths" | "aliased-tsconfig-base-url"
        | "aliased-subpath-import" | "aliased-workspace" | "aliased-webpack" => "aliased",
        _ => return None,
    })
}

/// Captures of a `from.path` / `module.path` match, for `$1`..`$9`.
type Caps = Vec<String>;

fn regex(p: &Pat, rule: &str) -> Result<Regex> {
    Regex::new(&p.0).with_context(|| format!("rule '{rule}': invalid regex {:?}", p.0))
}

fn matches(re: &Regex, s: &str) -> bool {
    re.is_match(s).unwrap_or(false)
}

/// `path` / `path_not` on the source side; yields captures.
struct Source {
    path: Option<Regex>,
    not: Option<Regex>,
}

impl Source {
    fn new(path: &Option<Pat>, not: &Option<Pat>, rule: &str) -> Result<Self> {
        Ok(Self {
            path: path.as_ref().map(|p| regex(p, rule)).transpose()?,
            not: not.as_ref().map(|p| regex(p, rule)).transpose()?,
        })
    }

    fn matches(&self, names: &[&str]) -> Option<Caps> {
        if let Some(n) = &self.not
            && names.iter().any(|s| matches(n, s))
        {
            return None;
        }
        let Some(re) = &self.path else { return Some(vec![]) };
        names.iter().find_map(|s| {
            let c = re.captures(s).ok().flatten()?;
            Some((0..c.len()).map(|i| c.get(i).map_or(String::new(), |m| m.as_str().to_string())).collect())
        })
    }
}

/// A target-side pattern that may reference source captures (`$1`).
enum Pattern {
    Static(Regex),
    Template(String),
}

impl Pattern {
    fn new(p: &Pat, rule: &str) -> Result<Self> {
        let src = &p.0;
        if src.as_bytes().windows(2).any(|w| w[0] == b'$' && w[1].is_ascii_digit()) {
            regex(&Pat(substitute(src, &[])), rule)?;
            Ok(Pattern::Template(src.clone()))
        } else {
            Ok(Pattern::Static(regex(p, rule)?))
        }
    }

    fn is_match(&self, s: &str, caps: &[String], cache: &mut HashMap<String, Regex>) -> bool {
        match self {
            Pattern::Static(r) => matches(r, s),
            Pattern::Template(t) => {
                let src = substitute(t, caps);
                let re = cache.entry(src).or_insert_with_key(|k| Regex::new(k).expect("validated"));
                matches(re, s)
            }
        }
    }
}

/// Replaces `$1`..`$9` with the (regex-escaped) capture group values.
fn substitute(template: &str, caps: &[String]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$'
            && let Some(d) = chars.peek().and_then(|d| d.to_digit(10))
        {
            chars.next();
            out.push_str(&fancy_regex::escape(caps.get(d as usize).map_or("", String::as_str)));
            continue;
        }
        out.push(c);
    }
    out
}

/// `path` / `path_not` on the target side.
struct Target {
    path: Option<Pattern>,
    not: Option<Pattern>,
}

impl Target {
    fn new(path: &Option<Pat>, not: &Option<Pat>, rule: &str) -> Result<Self> {
        Ok(Self {
            path: path.as_ref().map(|p| Pattern::new(p, rule)).transpose()?,
            not: not.as_ref().map(|p| Pattern::new(p, rule)).transpose()?,
        })
    }

    fn matches(&self, names: &[&str], caps: &[String], cache: &mut HashMap<String, Regex>) -> bool {
        self.path.as_ref().is_none_or(|p| names.iter().any(|s| p.is_match(s, caps, cache)))
            && !self.not.as_ref().is_some_and(|p| names.iter().any(|s| p.is_match(s, caps, cache)))
    }
}

fn types(list: &Option<Vec<String>>, rule: &str) -> Result<Option<Vec<&'static str>>> {
    list.as_ref()
        .map(|l| {
            l.iter()
                .map(|t| canonical_type(t).with_context(|| format!("rule '{rule}': unknown dependency type {t:?}")))
                .collect()
        })
        .transpose()
}

/// A compiled `from` + `to` pair.
struct Compiled<'r> {
    from: &'r FromSpec,
    to: &'r ToSpec,
    from_orphan: Option<bool>,
    source: Source,
    target: Target,
    via: Option<Target>,
    via_only: Option<Target>,
    types: Option<Vec<&'static str>>,
    types_not: Option<Vec<&'static str>>,
    license: Option<Regex>,
    license_not: Option<Regex>,
}

fn compile<'r>(name: &str, from: &'r FromSpec, to: &'r ToSpec) -> Result<Compiled<'r>> {
    if to.reachable.is_some() && from.path.is_none() {
        bail!("rule '{name}': `to.reachable` needs `from.path` to name the entry points");
    }
    if to.max_cycle_length.is_some() && (to.via.is_some() || to.via_only.is_some()) {
        bail!("rule '{name}': `to.max_cycle_length` can't be combined with `via` / `via_only`");
    }
    let via = |v: &Option<PathSpec>| v.as_ref().map(|v| Target::new(&v.path, &v.path_not, name)).transpose();
    Ok(Compiled {
        from,
        to,
        from_orphan: from.orphan,
        source: Source::new(&from.path, &from.path_not, name)?,
        target: Target::new(&to.path, &to.path_not, name)?,
        via: via(&to.via)?,
        via_only: via(&to.via_only)?,
        types: types(&to.dependency_types, name)?,
        types_not: types(&to.dependency_types_not, name)?,
        license: to.license.as_ref().map(|p| regex(p, name)).transpose()?,
        license_not: to.license_not.as_ref().map(|p| regex(p, name)).transpose()?,
    })
}

struct CompiledModule {
    source: Source,
    less_than: Option<usize>,
    more_than: Option<usize>,
}

fn compile_module(name: &str, m: &ModuleSpec) -> Result<CompiledModule> {
    Ok(CompiledModule {
        source: Source::new(&m.path, &m.path_not, name)?,
        less_than: m.number_of_dependents_less_than,
        more_than: m.number_of_dependents_more_than,
    })
}

pub fn validate(cfg: &Config) -> Result<()> {
    for r in &cfg.forbidden {
        if r.scope == Scope::Folder && (r.from.orphan.is_some() || r.to.reachable.is_some()) {
            bail!("rule '{}': `orphan` and `reachable` only apply to modules, not scope = \"folder\"", r.name);
        }
        compile(&r.name, &r.from, &r.to)?;
        if let Some(m) = &r.module {
            compile_module(&r.name, m)?;
        }
    }
    for r in &cfg.allowed {
        compile(NOT_IN_ALLOWED, &r.from, &r.to)?;
    }
    for r in &cfg.required {
        compile_module(&r.name, &r.module)?;
        Target::new(&r.to.path, &r.to.path_not, &r.name)?;
    }
    Ok(())
}

/// Installed-package metadata, for `license` rules and the `deprecated` type.
struct PkgMeta {
    license: Option<String>,
    deprecated: bool,
}

struct Ctx<'g> {
    g: &'g Graph,
    /// npm packages also answer to `node_modules/<pkg>/`, so path rules
    /// written against installed files keep working.
    alt: Vec<Option<String>>,
    cache: HashMap<String, Regex>,
    meta: HashMap<String, Option<PkgMeta>>,
}

impl<'g> Ctx<'g> {
    fn new(g: &'g Graph) -> Self {
        let alt = g
            .modules
            .iter()
            .map(|m| (m.kind == ModuleKind::Npm).then(|| format!("node_modules/{}/", m.id)))
            .collect();
        Ctx { g, alt, cache: HashMap::default(), meta: HashMap::default() }
    }

    /// The names module `m` can be matched by.
    fn names(&self, m: usize) -> Vec<&str> {
        let mut v = vec![self.g.modules[m].id.as_str()];
        v.extend(self.alt[m].as_deref());
        v
    }

    /// Does module `m` match `t`?
    fn target_matches(&mut self, t: &Target, m: usize, caps: &[String]) -> bool {
        let (id, alt) = (self.g.modules[m].id.as_str(), self.alt[m].as_deref());
        match alt {
            Some(a) => t.matches(&[id, a], caps, &mut self.cache),
            None => t.matches(&[id], caps, &mut self.cache),
        }
    }

    /// Package metadata, found by walking up from the importing module.
    fn pkg_meta(&mut self, from: usize, pkg: &str) -> Option<&PkgMeta> {
        if !self.meta.contains_key(pkg) {
            let start = self.g.root.join(&self.g.modules[from].id);
            let meta = start.ancestors().skip(1).find_map(|dir| {
                let text = std::fs::read_to_string(dir.join("node_modules").join(pkg).join("package.json")).ok()?;
                let v: serde_json::Value = serde_json::from_str(&text).ok()?;
                let license = match v.get("license") {
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    Some(o) => o.get("type").and_then(|t| t.as_str()).map(String::from),
                    None => v
                        .get("licenses")
                        .and_then(|l| l.get(0))
                        .and_then(|l| l.get("type"))
                        .and_then(|t| t.as_str())
                        .map(String::from),
                };
                let deprecated = match v.get("deprecated") {
                    Some(serde_json::Value::Bool(b)) => *b,
                    Some(serde_json::Value::String(s)) => !s.is_empty(),
                    _ => false,
                };
                Some(PkgMeta { license, deprecated })
            });
            self.meta.insert(pkg.to_string(), meta);
        }
        self.meta[pkg].as_ref()
    }

    fn edge_has_type(&mut self, e: &Edge, t: &str) -> bool {
        let g = self.g;
        match t {
            "import" => !e.flags.require && !e.flags.dynamic,
            "relative" => e.specifier.starts_with('.') || e.specifier.starts_with('/'),
            "aliased" => g.modules[e.to].kind == ModuleKind::Local && is_bare(&e.specifier),
            "deprecated" => {
                g.modules[e.to].kind == ModuleKind::Npm && self.pkg_meta(e.from, &g.modules[e.to].id).is_some_and(|m| m.deprecated)
            }
            _ => e.types.contains(&t),
        }
    }
}

/// `tags` (any of) / `tags_not` (none of) / `tags_all` (all of).
fn tags_ok(tags: &[String], any: &Option<Vec<String>>, none: &Option<Vec<String>>, all: Option<&Vec<String>>) -> bool {
    use crate::groups::{has_any, tag_matches};
    any.as_ref().is_none_or(|p| has_any(p, tags))
        && !none.as_ref().is_some_and(|p| has_any(p, tags))
        && all.is_none_or(|p| p.iter().all(|pat| tags.iter().any(|t| tag_matches(pat, t))))
}

impl Compiled<'_> {
    /// Does module `m` satisfy `from` (tags and path)? Yields path captures.
    fn source_ok(&self, cx: &Ctx, m: usize) -> Option<Caps> {
        let f = self.from;
        if !tags_ok(&cx.g.modules[m].tags, &f.tags, &f.tags_not, f.tags_all.as_ref()) {
            return None;
        }
        self.source.matches(&cx.names(m))
    }

    /// Whether edge `i` matches; returns the cycle witness for circular edges.
    fn edge_matches(&self, cx: &mut Ctx, i: usize, caps: &[String]) -> Option<Vec<usize>> {
        let g = cx.g;
        let e = &g.edges[i];
        let to = &g.modules[e.to];
        let t = self.to;
        let is = |want: Option<bool>, actual: bool| want.is_none_or(|w| w == actual);
        let ok = is(t.circular, e.circular)
            && is(t.could_not_resolve, to.kind == ModuleKind::Unresolved)
            && is(t.type_only, e.flags.type_only)
            && is(t.dynamic, e.flags.dynamic)
            && is(t.more_than_one_dependency_type, e.multi_type)
            && is(t.more_unstable, to.kind == ModuleKind::Local && g.instability(e.to) > g.instability(e.from));
        if !ok {
            return None;
        }
        if let Some(list) = &self.types
            && !list.iter().any(|x| cx.edge_has_type(e, x))
        {
            return None;
        }
        if let Some(list) = &self.types_not
            && list.iter().any(|x| cx.edge_has_type(e, x))
        {
            return None;
        }
        if !tags_ok(&to.tags, &t.tags, &t.tags_not, None) {
            return None;
        }
        if let Some(want) = t.cross_group {
            let cross = match (g.group_of.get(e.from).copied().flatten(), g.group_of.get(e.to).copied().flatten()) {
                (Some(a), Some(b)) => a != b,
                // On the group graph every edge joins two different groups.
                _ => g.group_of.is_empty(),
            };
            if cross != want {
                return None;
            }
        }
        if !cx.target_matches(&self.target, e.to, caps) {
            return None;
        }
        if self.license.is_some() || self.license_not.is_some() {
            if to.kind != ModuleKind::Npm {
                return None;
            }
            let lic = cx.pkg_meta(e.from, &to.id)?.license.clone()?;
            if self.license.as_ref().is_some_and(|r| !matches(r, &lic))
                || self.license_not.as_ref().is_some_and(|r| matches(r, &lic))
            {
                return None;
            }
        }
        if !e.circular {
            return Some(vec![]);
        }
        // Circular: find a witness cycle, honouring `via` / `via_only`. Unlike
        // checking one arbitrary cycle, these consider *every* cycle through
        // the dependency.
        if let Some(spec) = &self.via_only {
            let scc = &g.cycles[g.cycle_of[e.from]?];
            let ok: HashSet<usize> = scc.iter().copied().filter(|&m| cx.target_matches(spec, m, caps)).collect();
            if !ok.contains(&e.from) || !ok.contains(&e.to) {
                return None;
            }
            let back = g.path_where(e.to, e.from, |x| x.circular, |m| ok.contains(&m))?;
            return Some(std::iter::once(e.from).chain(back).collect());
        }
        if let Some(spec) = &self.via {
            // A *simple* cycle from → to ⇝ hit ⇝ from through a matching
            // module: the two legs may only share `hit`. (Exact search is
            // NP-hard in general; trying each candidate with shortest legs is
            // sound — it never reports a non-cycle — and finds nearly all.)
            let fwd = g.closure_where(e.to, true, |x| x.circular);
            let bwd = g.closure_where(e.from, false, |x| x.circular);
            let mut members: Vec<usize> = fwd.intersection(&bwd).copied().collect();
            members.sort_unstable();
            for hit in members {
                if !cx.target_matches(spec, hit, caps) {
                    continue;
                }
                let Some(a) = g.path_where(e.to, hit, |x| x.circular, |m| m != e.from || hit == e.from) else {
                    continue;
                };
                let used: HashSet<usize> = a.iter().copied().filter(|&m| m != hit).collect();
                let Some(b) = g.path_where(hit, e.from, |x| x.circular, |m| !used.contains(&m)) else { continue };
                return Some(std::iter::once(e.from).chain(a).chain(b.into_iter().skip(1)).collect());
            }
            return None;
        }
        let cycle = g.cycle_path(i);
        // The path repeats `from` at the end: n modules, n edges.
        if t.max_cycle_length.is_some_and(|max| cycle.len() - 1 > max) {
            return None;
        }
        Some(cycle)
    }
}

/// Calls `f(edge, cycle)` for every edge matching `c`. `from` is matched once
/// per module, then its outgoing edges are tested.
fn for_each_edge(cx: &mut Ctx, c: &Compiled, mut f: impl FnMut(usize, Vec<usize>)) {
    let g = cx.g;
    for m in 0..g.modules.len() {
        if g.out[m].is_empty() {
            continue;
        }
        let Some(caps) = c.source_ok(cx, m) else { continue };
        for &i in &g.out[m] {
            if let Some(cycle) = c.edge_matches(cx, i, &caps) {
                f(i, cycle);
            }
        }
    }
}

/// Evaluates one `forbidden` rule against `cx.g` (the module or folder graph).
fn eval_forbidden(cx: &mut Ctx, rule: &Rule, out: &mut Vec<Violation>) -> Result<()> {
    let g = cx.g;
    let n = g.modules.len();
    let violation = |from, to, cycle| Violation {
        rule: rule.name.clone(),
        severity: rule.severity,
        comment: rule.comment.clone(),
        scope: rule.scope,
        from,
        to,
        cycle,
    };
    if let Some(spec) = &rule.module {
        let c = compile_module(&rule.name, spec)?;
        // `from` restricts which dependents are counted.
        let counted = Source::new(&rule.from.path, &rule.from.path_not, &rule.name)?;
        let counts_all = counted.path.is_none() && counted.not.is_none();
        for m in 0..n {
            let dependents = if counts_all {
                g.fan_in(m)
            } else {
                g.inc[m].iter().filter(|&&i| counted.matches(&cx.names(g.edges[i].from)).is_some()).count()
            };
            if c.less_than.is_some_and(|x| dependents >= x) || c.more_than.is_some_and(|x| dependents <= x) {
                continue;
            }
            if c.source.matches(&cx.names(m)).is_some() {
                out.push(violation(m, None, vec![]));
            }
        }
        return Ok(());
    }
    let c = compile(&rule.name, &rule.from, &rule.to)?;
    if c.from_orphan == Some(true) {
        for m in 0..n {
            if g.is_orphan(m) && c.source_ok(cx, m).is_some() {
                out.push(violation(m, None, vec![]));
            }
        }
    } else if let Some(reachable) = rule.to.reachable {
        let entries: Vec<usize> = (0..n)
            .filter(|&m| g.modules[m].kind == ModuleKind::Local)
            .filter(|&m| c.source_ok(cx, m).is_some())
            .collect();
        if entries.is_empty() {
            return Ok(());
        }
        let reach = g.closure(&entries, true);
        for m in 0..n {
            let module = &g.modules[m];
            if module.kind != ModuleKind::Local || !module.scanned || entries.contains(&m) {
                continue;
            }
            if reach.contains(&m) == reachable && cx.target_matches(&c.target, m, &[]) {
                out.push(violation(m, None, vec![]));
            }
        }
    } else {
        for_each_edge(cx, &c, |i, cycle| {
            let e = &g.edges[i];
            out.push(violation(e.from, Some(e.to), cycle));
        });
    }
    Ok(())
}

pub fn evaluate(g: &Graph, cfg: &Config) -> Result<Vec<Violation>> {
    let mut out = vec![];
    let mut cx = Ctx::new(g);
    let n = g.modules.len();
    let mut folder_cx: Option<Ctx> = None;
    let mut group_cx: Option<Ctx> = None;
    for rule in cfg.forbidden.iter().filter(|r| r.severity != Severity::Off) {
        match rule.scope {
            Scope::Module => eval_forbidden(&mut cx, rule, &mut out)?,
            Scope::Folder => {
                let fcx = folder_cx.get_or_insert_with(|| Ctx::new(g.folders()));
                eval_forbidden(fcx, rule, &mut out)?;
            }
            Scope::Group => {
                let gcx = group_cx.get_or_insert_with(|| Ctx::new(g.groups()));
                eval_forbidden(gcx, rule, &mut out)?;
            }
        }
    }

    // Allow-list: every dependency must match some `allowed` rule.
    if !cfg.allowed.is_empty() && cfg.allowed_severity != Severity::Off {
        let rules: Vec<Compiled> =
            cfg.allowed.iter().map(|r| compile(NOT_IN_ALLOWED, &r.from, &r.to)).collect::<Result<_>>()?;
        let mut allowed = vec![false; g.edges.len()];
        for c in &rules {
            for_each_edge(&mut cx, c, |i, _| allowed[i] = true);
        }
        for (i, e) in g.edges.iter().enumerate() {
            if !allowed[i] {
                out.push(Violation {
                    rule: NOT_IN_ALLOWED.into(),
                    severity: cfg.allowed_severity,
                    comment: Some("This dependency isn't covered by any `allowed` rule.".into()),
                    scope: Scope::Module,
                    from: e.from,
                    to: Some(e.to),
                    cycle: if e.circular { g.cycle_path(i) } else { vec![] },
                });
            }
        }
    }

    // Required: matching modules must depend on something matching `to`.
    for rule in cfg.required.iter().filter(|r| r.severity != Severity::Off) {
        let c = compile_module(&rule.name, &rule.module)?;
        let target = Target::new(&rule.to.path, &rule.to.path_not, &rule.name)?;
        for m in 0..n {
            if g.modules[m].kind != ModuleKind::Local || !g.modules[m].scanned {
                continue;
            }
            let Some(caps) = c.source.matches(&[g.modules[m].id.as_str()]) else { continue };
            let satisfied = g.out[m].iter().any(|&i| cx.target_matches(&target, g.edges[i].to, &caps));
            if !satisfied {
                out.push(Violation {
                    rule: rule.name.clone(),
                    severity: rule.severity,
                    comment: rule.comment.clone(),
                    scope: Scope::Module,
                    from: m,
                    to: None,
                    cycle: vec![],
                });
            }
        }
    }

    out.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.rule.cmp(&b.rule))
            .then_with(|| a.source_id(g).cmp(b.source_id(g)))
            .then_with(|| a.target_id(g).cmp(&b.target_id(g)))
    });
    Ok(out)
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
        from: v.source_id(g).to_string(),
        to: v.target_id(g).map(String::from),
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
        let caps = vec!["src/features/a.b/".to_string(), "a.b".to_string()];
        assert_eq!(substitute("^src/features/$1/", &caps), r"^src/features/a\.b/");
        assert_eq!(substitute("end$", &caps), "end$");
    }

    #[test]
    fn via_requires_a_simple_cycle() {
        use crate::scan::{ImportFlags, Import, ScannedFile, Target as T, Work};
        use std::path::PathBuf;
        // a → b, b → a, b → s, s → a: b → a is only on the cycle b → a → b.
        let root = PathBuf::from("/r");
        let file = |name: &str, deps: &[&str]| {
            ScannedFile::for_test(
                root.join(name),
                deps.iter()
                    .map(|d| Import { specifier: d.to_string(), flags: ImportFlags::default(), target: T::Local(root.join(d)) })
                    .collect(),
            )
        };
        let files = vec![file("a.ts", &["b.ts"]), file("b.ts", &["a.ts", "s.ts"]), file("s.ts", &["a.ts"])];
        let g = Graph::build(&root, &files, Work::default(), &crate::config::Options::default());
        let cfg: Config = toml::from_str(
            r#"
            [[forbidden]]
            name = "via-s"
            to = { circular = true, via = "^s\\.ts$" }
            "#,
        )
        .unwrap();
        let mut got: Vec<(String, String)> = evaluate(&g, &cfg)
            .unwrap()
            .into_iter()
            .map(|v| (g.modules[v.from].id.clone(), g.modules[v.to.unwrap()].id.clone()))
            .collect();
        got.sort();
        let want = [("a.ts", "b.ts"), ("b.ts", "s.ts"), ("s.ts", "a.ts")];
        assert_eq!(got, want.map(|(a, b)| (a.to_string(), b.to_string())));
    }

    #[test]
    fn folder_scope() {
        use crate::scan::{ImportFlags, Import, ScannedFile, Target as T, Work};
        use std::path::PathBuf;
        // No module cycle, but folders a/ and b/ depend on each other.
        let root = PathBuf::from("/r");
        let file = |name: &str, deps: &[&str]| {
            ScannedFile::for_test(
                root.join(name),
                deps.iter()
                    .map(|d| Import { specifier: d.to_string(), flags: ImportFlags::default(), target: T::Local(root.join(d)) })
                    .collect(),
            )
        };
        let files = vec![
            file("a/x.ts", &["b/y.ts"]),
            file("a/w.ts", &[]),
            file("b/y.ts", &[]),
            file("b/z.ts", &["a/w.ts", "c/q.ts"]),
            file("c/q.ts", &[]),
            file("index.ts", &["a/x.ts", "b/z.ts"]),
        ];
        let g = Graph::build(&root, &files, Work::default(), &crate::config::Options::default());
        let cfg: Config = toml::from_str(
            r#"
            [[forbidden]]
            name = "module-cycles"
            to = { circular = true }

            [[forbidden]]
            name = "folder-cycles"
            scope = "folder"
            to = { circular = true }

            [[forbidden]]
            name = "lonely-folders"
            scope = "folder"
            module = { path = '^[a-z]$', number_of_dependents_less_than = 2 }
            "#,
        )
        .unwrap();
        validate(&cfg).unwrap();
        let mut got: Vec<String> = evaluate(&g, &cfg)
            .unwrap()
            .iter()
            .map(|v| format!("{} {} -> {} | {}", v.rule, v.source_id(&g), v.target_id(&g).unwrap_or("-"), v.cycle_ids(&g).join(" > ")))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                "folder-cycles a -> b | a > b > a",
                "folder-cycles b -> a | b > a > b",
                // Each has a single dependent folder (root-level files aren't in one).
                "lonely-folders a -> - | ",
                "lonely-folders b -> - | ",
                "lonely-folders c -> - | ",
            ]
        );
        let bad: Config = toml::from_str("[[forbidden]]\nname = \"x\"\nscope = \"folder\"\nfrom = { orphan = true }").unwrap();
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn folders_include_subfolders() {
        use crate::scan::{ImportFlags, Import, ScannedFile, Target as T, Work};
        use std::path::PathBuf;
        let root = PathBuf::from("/r");
        let file = |name: &str, deps: &[&str]| {
            ScannedFile::for_test(
                root.join(name),
                deps.iter()
                    .map(|d| Import { specifier: d.to_string(), flags: ImportFlags::default(), target: T::Local(root.join(d)) })
                    .collect(),
            )
        };
        // features/cart/ui/button.ts reaches into features/user/api/.
        let files = vec![
            file("src/features/cart/ui/button.ts", &["src/features/user/api/client.ts", "src/features/cart/model.ts"]),
            file("src/features/cart/model.ts", &[]),
            file("src/features/user/api/client.ts", &[]),
        ];
        let g = Graph::build(&root, &files, Work::default(), &crate::config::Options::default());
        let cfg: Config = toml::from_str(
            r#"
            [[forbidden]]
            name = "no-cross-feature"
            scope = "folder"
            from = { path = '^src/features/([^/]+)$' }
            to = { path = '^src/features/', path_not = '^src/features/$1(/|$)' }
            "#,
        )
        .unwrap();
        let got: Vec<String> =
            evaluate(&g, &cfg).unwrap().iter().map(|v| format!("{} -> {}", v.source_id(&g), v.target_id(&g).unwrap())).collect();
        // The whole cart feature (not just files directly in it) depends on user/api.
        assert_eq!(got, ["src/features/cart -> src/features/user/api"]);
        let f = g.folders();
        let cart = f.find(ModuleKind::Local, "src/features/cart").unwrap();
        // One module edge leaves the cart subtree, none enter it.
        assert_eq!(f.instability(cart), 1.0);
    }

    #[test]
    fn js_style_regexes_compile() {
        for p in [r"^(?!src/)", r"\/", r"(?<=a)b", r"\.(spec|test)\.[cm]?[jt]sx?$"] {
            assert!(Regex::new(p).is_ok(), "{p}");
        }
    }
}
