//! Non-interactive output: rule reports, graph exports and stats.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::IsTerminal;

use regex::Regex;
use serde::Serialize;
use serde_json::json;

use crate::config::{Scope, Severity};
use crate::graph::{Graph, ModuleKind};
use crate::rules::{BaselineEntry, Violation};

pub struct Paint(bool);

impl Paint {
    pub fn stdout() -> Self {
        Paint(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }
    pub fn stderr() -> Self {
        Paint(std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.0 { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    pub fn bold(&self, s: &str) -> String { self.wrap("1", s) }
    pub fn dim(&self, s: &str) -> String { self.wrap("2", s) }
    pub fn red(&self, s: &str) -> String { self.wrap("31", s) }
    pub fn yellow(&self, s: &str) -> String { self.wrap("33", s) }
    pub fn blue(&self, s: &str) -> String { self.wrap("34", s) }
    pub fn green(&self, s: &str) -> String { self.wrap("32", s) }
    pub fn cyan(&self, s: &str) -> String { self.wrap("36", s) }

    pub fn severity(&self, s: Severity) -> String {
        match s {
            Severity::Error => self.red(&self.bold("error")),
            Severity::Warn => self.yellow(&self.bold("warn")),
            Severity::Info => self.blue(&self.bold("info")),
            Severity::Off => self.dim("off"),
        }
    }
}

pub fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

pub fn counts(vs: &[Violation]) -> (usize, usize, usize) {
    let n = |s| vs.iter().filter(|v| v.severity == s).count();
    (n(Severity::Error), n(Severity::Warn), n(Severity::Info))
}

/// The module imports behind a group violation: (file, specifier, target).
fn imports<'g>(g: &'g Graph, v: &Violation) -> impl Iterator<Item = (&'g str, &'g str, &'g str)> {
    v.imports.iter().map(|&i| {
        let e = &g.edges[i];
        (g.modules[e.from].id.as_str(), &*e.specifier, g.modules[e.to].id.as_str())
    })
}

pub fn text(g: &Graph, vs: &[Violation], suppressed: usize, stale: &Stale) -> String {
    let p = Paint::stdout();
    let mut out = String::new();
    // Group by rule, preserving the severity-first order from `evaluate`.
    let mut groups: Vec<(&str, Vec<&Violation>)> = vec![];
    for v in vs {
        match groups.iter_mut().find(|(r, _)| *r == v.rule) {
            Some((_, list)) => list.push(v),
            None => groups.push((&v.rule, vec![v])),
        }
    }
    for (rule, list) in &groups {
        let first = list[0];
        let _ = writeln!(
            out,
            "{} {} {}",
            p.severity(first.severity),
            p.bold(rule),
            p.dim(&format!("({})", list.len()))
        );
        // One rule name can cover several checks (e.g. converted Nx rules):
        // then each violation carries its own explanation.
        let shared = list.iter().all(|v| v.comment == first.comment);
        if shared && let Some(c) = &first.comment {
            let _ = writeln!(out, "  {}", p.dim(c));
        }
        for v in list {
            let vg = v.graph(g);
            let from = &vg.modules[v.from].id;
            match v.to {
                Some(t) => {
                    let to = &vg.modules[t];
                    let target = if to.kind == ModuleKind::Local { to.id.clone() } else { p.cyan(&to.id) };
                    let _ = writeln!(out, "  {} {} {}", from, p.dim("→"), target);
                }
                None => {
                    let _ = writeln!(out, "  {from}");
                }
            }
            if !shared && let Some(c) = &v.comment {
                let _ = writeln!(out, "    {}", p.dim(c));
            }
            if v.cycle.len() > 1 {
                let _ = writeln!(out, "    {} {}", p.dim("cycle:"), p.dim(&v.cycle_ids(g).join(" → ")));
            }
            const SHOWN: usize = 3;
            for (file, specifier, _) in imports(g, v).take(SHOWN) {
                let _ = writeln!(out, "    {} {file} {} {specifier}", p.dim("via"), p.dim("imports"));
            }
            if v.imports.len() > SHOWN {
                let _ = writeln!(out, "    {}", p.dim(&format!("… and {} more imports", v.imports.len() - SHOWN)));
            }
        }
        out.push('\n');
    }
    let stale_list = stale.reported();
    if !stale_list.is_empty() {
        let _ = writeln!(out, "{} {} {}", p.severity(stale.severity), p.bold(STALE_RULE), p.dim(&format!("({})", stale_list.len())));
        let _ = writeln!(out, "  {}", p.dim(STALE_COMMENT));
        for e in stale_list {
            let target = e.to.as_ref().map(|t| format!(" {} {t}", p.dim("→"))).unwrap_or_default();
            let _ = writeln!(out, "  {} {}{target}", p.dim(&format!("{}:", e.rule)), e.from);
        }
        out.push('\n');
    }
    let (e, w, i) = totals(vs, stale);
    let status_empty = vs.is_empty() && stale_list.is_empty();
    let summary = format!(
        "{} modules, {} dependencies, {} · {:.0}ms",
        g.local_count(),
        g.edges.len(),
        plural(g.cycles.len(), "cycle"),
        g.total_ms()
    );
    let status = if status_empty {
        p.green(&p.bold("✔ no violations"))
    } else {
        let mark = if e > 0 { p.red("✖") } else { p.yellow("⚠") };
        format!("{mark} {}, {}, {} info", p.bold(&plural(e, "error")), plural(w, "warning"), i)
    };
    let _ = write!(out, "{status}  {}", p.dim(&summary));
    if suppressed > 0 {
        let _ = write!(out, "  {}", p.dim(&format!("({suppressed} baselined)")));
    }
    out.push('\n');
    out
}

/// Baseline entries that no longer occur, and how to report them.
pub struct Stale<'a> {
    pub entries: &'a [BaselineEntry],
    pub severity: Severity,
}

pub const STALE_RULE: &str = "stale-baseline-entry";
const STALE_COMMENT: &str =
    "This baseline entry no longer occurs. Remove fixed entries with `detangle check --write-baseline --baseline-mode shrink-only`.";

impl Stale<'_> {
    pub fn none() -> Stale<'static> {
        Stale { entries: &[], severity: Severity::Off }
    }

    /// The entries to report (none when reporting is off).
    pub fn reported(&self) -> &[BaselineEntry] {
        if self.severity == Severity::Off { &[] } else { self.entries }
    }
}

/// Errors, warnings and info, stale baseline entries included.
pub fn totals(vs: &[Violation], stale: &Stale) -> (usize, usize, usize) {
    let (mut e, mut w, mut i) = counts(vs);
    let n = stale.reported().len();
    match stale.severity {
        Severity::Error => e += n,
        Severity::Warn => w += n,
        Severity::Info => i += n,
        Severity::Off => {}
    }
    (e, w, i)
}

/// One annotation for a CI system: a finding on a file.
struct Finding {
    rule: String,
    severity: Severity,
    comment: Option<String>,
    file: String,
    message: String,
    /// The import string behind it, when there is one.
    specifier: Option<String>,
}

/// Violations (group ones on each import behind them) and stale entries.
fn findings(g: &Graph, vs: &[Violation], stale: &Stale) -> Vec<Finding> {
    let mut out = vec![];
    for v in vs {
        let base = |file: &str, message: String, specifier: Option<&str>| Finding {
            rule: v.rule.clone(),
            severity: v.severity,
            comment: v.comment.clone(),
            file: file.to_string(),
            message,
            specifier: specifier.map(String::from),
        };
        if !v.imports.is_empty() {
            for (file, specifier, _) in imports(g, v) {
                out.push(base(file, format!("imports {specifier} ({} → {})", v.source_id(g), v.target_id(g).unwrap_or("")), Some(specifier)));
            }
            continue;
        }
        let message = if v.cycle.len() > 1 {
            format!("cycle: {}", v.cycle_ids(g).join(" → "))
        } else {
            v.target_id(g).map(|t| format!("depends on {t}")).unwrap_or_default()
        };
        // A module's dependency on another: the import that makes it.
        let specifier = match (v.scope, v.to) {
            (Scope::Module, Some(to)) => g.out[v.from].iter().map(|&e| &g.edges[e]).find(|e| e.to == to).map(|e| &*e.specifier),
            _ => None,
        };
        out.push(base(v.source_id(g), message, specifier));
    }
    for e in stale.reported() {
        out.push(Finding {
            rule: STALE_RULE.into(),
            severity: stale.severity,
            comment: Some(STALE_COMMENT.into()),
            file: e.from.clone(),
            message: format!("{}: {}{}", e.rule, e.from, e.to.as_ref().map(|t| format!(" → {t}")).unwrap_or_default()),
            specifier: None,
        });
    }
    out
}

impl Finding {
    fn text(&self) -> String {
        match &self.comment {
            Some(c) if self.message.is_empty() => c.clone(),
            Some(c) => format!("{}\n{c}", self.message),
            None => self.message.clone(),
        }
    }
}

/// GitHub Actions workflow commands → inline PR annotations.
pub fn github(g: &Graph, vs: &[Violation], stale: &Stale) -> String {
    let esc = |s: &str| s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A");
    let mut sources: HashMap<String, Option<String>> = HashMap::new();
    let mut out = String::new();
    for f in findings(g, vs, stale) {
        let level = match f.severity {
            Severity::Error => "error",
            Severity::Warn => "warning",
            _ => "notice",
        };
        let line = f.specifier.as_deref().and_then(|spec| {
            let source = sources.entry(f.file.clone()).or_insert_with(|| std::fs::read_to_string(g.root.join(&f.file)).ok());
            import_line(source.as_deref()?, spec)
        });
        let line = line.map(|n| format!(",line={n}")).unwrap_or_default();
        let _ = writeln!(out, "::{level} file={}{line},title={}::{}", f.file, f.rule, esc(&f.text()));
    }
    out
}

/// The 1-based line where `spec` first appears as a quoted string, outside
/// lines that are comments.
fn import_line(source: &str, spec: &str) -> Option<usize> {
    let quoted = ['"', '\'', '`'].map(|q| format!("{q}{spec}{q}"));
    let comment = |l: &str| ["//", "/*", "*"].iter().any(|c| l.trim_start().starts_with(c));
    source.lines().position(|l| !comment(l) && quoted.iter().any(|q| l.contains(q.as_str()))).map(|i| i + 1)
}

/// TeamCity service messages: an inspection type per rule, an inspection
/// per finding.
pub fn teamcity(g: &Graph, vs: &[Violation], stale: &Stale) -> String {
    let esc = |s: &str| {
        s.replace('|', "||").replace('\'', "|'").replace('\n', "|n").replace('\r', "|r").replace('[', "|[").replace(']', "|]")
    };
    let all = findings(g, vs, stale);
    let mut out = String::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for f in &all {
        if seen.insert(&f.rule) {
            let _ = writeln!(
                out,
                "##teamcity[inspectionType id='{0}' name='{0}' description='{1}' category='detangle']",
                esc(&f.rule),
                esc(f.comment.as_deref().unwrap_or(&f.rule))
            );
        }
    }
    for f in &all {
        let severity = match f.severity {
            Severity::Error => "ERROR",
            Severity::Warn => "WARNING",
            _ => "INFO",
        };
        let _ = writeln!(
            out,
            "##teamcity[inspection typeId='{}' message='{}' file='{}' SEVERITY='{severity}']",
            esc(&f.rule),
            esc(&f.message),
            esc(&f.file)
        );
    }
    out
}

/// Azure DevOps logging commands: an issue per finding, then the result.
pub fn azure(g: &Graph, vs: &[Violation], stale: &Stale) -> String {
    let prop = |s: &str| s.replace('%', "%25").replace(';', "%3B").replace('\r', "%0D").replace('\n', "%0A").replace(']', "%5D");
    let msg = |s: &str| s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A");
    let mut out = String::new();
    for f in findings(g, vs, stale) {
        let kind = if f.severity == Severity::Error { "error" } else { "warning" };
        let _ = writeln!(out, "##vso[task.logissue type={kind};sourcepath={};code={};]{}", prop(&f.file), prop(&f.rule), msg(&f.text()));
    }
    let (e, w, i) = totals(vs, stale);
    let result = if e > 0 {
        "Failed"
    } else if w + i > 0 {
        "SucceededWithIssues"
    } else {
        "Succeeded"
    };
    let _ = writeln!(out, "##vso[task.complete result={result};]{}, {}, {i} info", plural(e, "error"), plural(w, "warning"));
    out
}

/// Markdown, e.g. for a pull-request comment or a CI job summary.
pub fn markdown(g: &Graph, vs: &[Violation], stale: &Stale) -> String {
    let cell = |s: &str| s.replace('|', "\\|").replace('\n', " ");
    let code = |s: &str| format!("`{}`", s.replace('`', "'"));
    let mut out = String::from("## Dependency check\n\n");
    let (e, w, i) = totals(vs, stale);
    let summary = format!(
        "{} modules, {} dependencies, {}",
        g.local_count(),
        g.edges.len(),
        plural(g.cycles.len(), "cycle")
    );
    if e + w + i == 0 {
        let _ = writeln!(out, "✅ **No rule violations.** {summary}");
        return out;
    }
    let mark = if e > 0 { "❌" } else { "⚠️" };
    let _ = writeln!(out, "{mark} **{}, {}, {i} info** · {summary}\n", plural(e, "error"), plural(w, "warning"));
    // Rules, severity first, as `evaluate` sorts them.
    let mut rules: Vec<(&str, Severity, Option<&str>, Vec<&Violation>)> = vec![];
    for v in vs {
        match rules.iter_mut().find(|r| r.0 == v.rule) {
            Some(r) => {
                if r.2 != v.comment.as_deref() {
                    r.2 = None;
                }
                r.3.push(v);
            }
            None => rules.push((&v.rule, v.severity, v.comment.as_deref(), vec![v])),
        }
    }
    let _ = writeln!(out, "| Rule | Severity | Violations | Description |\n|---|---|---:|---|");
    for (rule, sev, comment, list) in &rules {
        let _ = writeln!(out, "| {} | {} | {} | {} |", code(rule), sev.as_str(), list.len(), cell(comment.unwrap_or("")));
    }
    let stale_list = stale.reported();
    if !stale_list.is_empty() {
        let _ = writeln!(out, "| {} | {} | {} | {} |", code(STALE_RULE), stale.severity.as_str(), stale_list.len(), cell(STALE_COMMENT));
    }
    let total = vs.len() + stale_list.len();
    let _ = writeln!(out, "\n<details>\n<summary>All {total} findings</summary>\n");
    for (rule, sev, comment, list) in &rules {
        let _ = writeln!(out, "### {} ({})\n", code(rule), sev.as_str());
        for v in list {
            let target = v.target_id(g).map(|t| format!(" → {}", code(t))).unwrap_or_default();
            let _ = writeln!(out, "- {}{target}", code(v.source_id(g)));
            if comment.is_none()
                && let Some(c) = &v.comment
            {
                let _ = writeln!(out, "  - {}", cell(c));
            }
            if v.cycle.len() > 1 {
                let _ = writeln!(out, "  - cycle: {}", v.cycle_ids(g).iter().map(|c| code(c)).collect::<Vec<_>>().join(" → "));
            }
            for (file, specifier, _) in imports(g, v) {
                let _ = writeln!(out, "  - via {} importing {}", code(file), code(specifier));
            }
        }
        out.push('\n');
    }
    if !stale_list.is_empty() {
        let _ = writeln!(out, "### {} ({})\n", code(STALE_RULE), stale.severity.as_str());
        for e in stale_list {
            let target = e.to.as_ref().map(|t| format!(" → {}", code(t))).unwrap_or_default();
            let _ = writeln!(out, "- {}: {}{target}", code(&e.rule), code(&e.from));
        }
        out.push('\n');
    }
    out.push_str("</details>\n");
    out
}

/// Stale entries as JSON violation records.
pub fn stale_json(stale: &Stale) -> Vec<serde_json::Value> {
    stale
        .reported()
        .iter()
        .map(|e| {
            json!({
                "rule": STALE_RULE, "severity": stale.severity, "comment": STALE_COMMENT, "scope": "module",
                "from": e.from, "to": e.to, "cycle": [], "imports": [], "baseline_rule": e.rule,
            })
        })
        .collect()
}

/// A violation as JSON (`check -f json`, `graph -f json`).
#[derive(Serialize)]
pub struct ViolationJson<'g> {
    rule: &'g str,
    severity: Severity,
    comment: Option<&'g str>,
    scope: crate::config::Scope,
    from: &'g str,
    to: Option<&'g str>,
    cycle: Vec<&'g str>,
    imports: Vec<ImportJson<'g>>,
}

#[derive(Serialize)]
struct ImportJson<'g> {
    from: &'g str,
    specifier: &'g str,
    to: &'g str,
}

pub fn violations_json<'g>(g: &'g Graph, vs: &'g [Violation]) -> Vec<ViolationJson<'g>> {
    vs.iter()
        .map(|v| ViolationJson {
            rule: &v.rule,
            severity: v.severity,
            comment: v.comment.as_deref(),
            scope: v.scope,
            from: v.source_id(g),
            to: v.target_id(g),
            cycle: v.cycle_ids(g),
            imports: imports(g, v).map(|(from, specifier, to)| ImportJson { from, specifier, to }).collect(),
        })
        .collect()
}

/// `check -f json`: the violations, then stale baseline entries.
pub fn check_json<'g>(g: &'g Graph, vs: &'g [Violation], stale: &Stale) -> impl Serialize + 'g {
    #[derive(Serialize)]
    #[serde(untagged)]
    enum Finding<'g> {
        Violation(ViolationJson<'g>),
        Stale(serde_json::Value),
    }
    let mut all: Vec<Finding> = violations_json(g, vs).into_iter().map(Finding::Violation).collect();
    all.extend(stale_json(stale).into_iter().map(Finding::Stale));
    all
}

// Field order is the JSON key order.
#[derive(Serialize)]
struct GraphJson<'g> {
    summary: SummaryJson,
    modules: Vec<ModuleJson<'g>>,
    cycles: Vec<Vec<&'g str>>,
    violations: Vec<ViolationJson<'g>>,
}

#[derive(Serialize)]
struct SummaryJson {
    modules: usize,
    dependencies: usize,
    cycles: usize,
    errors: usize,
    warnings: usize,
    info: usize,
    timings: crate::graph::Timings,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModuleJson<'g> {
    id: &'g str,
    kind: ModuleKind,
    fan_in: usize,
    fan_out: usize,
    instability: f64,
    cycle: Option<usize>,
    dependencies: Vec<DependencyJson<'g>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    highlighted: Option<bool>,
}

#[derive(Serialize)]
struct DependencyJson<'g> {
    module: &'g str,
    specifier: &'g str,
    types: &'g [&'static str],
    circular: bool,
}

/// How `--collapse` merges modules.
pub enum Collapse {
    /// Local modules to their first N path segments.
    Depth(usize),
    /// Modules matching the regex to the text it matches (e.g.
    /// `^packages/[^/]+/` → one node per package).
    Pattern(Regex),
}

/// The whole graph as JSON, narrowed to the modules `--focus` / `--reaches`
/// select, with `--highlight`ed modules marked.
pub fn graph_json<'g>(g: &'g Graph, vs: &'g [Violation], view: &GraphView) -> impl Serialize + 'g {
    let selected = view.selected(g);
    let keep: Option<HashSet<&str>> = selected.as_ref().map(|s| s.iter().map(|&m| g.modules[m].id.as_str()).collect());
    let kept = |id: &str| keep.as_ref().is_none_or(|k| k.contains(id));
    let (e, w, i) = counts(vs);
    GraphJson {
        summary: SummaryJson {
            modules: g.local_count(),
            dependencies: g.edges.len(),
            cycles: g.cycles.len(),
            errors: e,
            warnings: w,
            info: i,
            timings: g.timings,
        },
        modules: g
            .modules
            .iter()
            .enumerate()
            .filter(|(_, module)| kept(&module.id))
            .map(|(m, module)| ModuleJson {
                id: &module.id,
                kind: module.kind,
                fan_in: g.fan_in(m),
                fan_out: g.fan_out(m),
                instability: (g.instability(m) * 1000.0).round() / 1000.0,
                cycle: g.cycle_of[m],
                dependencies: g.out[m]
                    .iter()
                    .map(|&e| &g.edges[e])
                    .filter(|edge| kept(&g.modules[edge.to].id))
                    .map(|edge| DependencyJson {
                        module: &g.modules[edge.to].id,
                        specifier: &edge.specifier,
                        types: edge.types,
                        circular: edge.circular,
                    })
                    .collect(),
                highlighted: view.highlight.as_ref().map(|re| re.is_match(&module.id)),
            })
            .collect(),
        cycles: g.cycles.iter().map(|c| c.iter().map(|&m| g.modules[m].id.as_str()).collect()).collect(),
        violations: violations_json(g, vs),
    }
}

pub struct GraphView {
    pub collapse: Option<Collapse>,
    pub focus: Option<Regex>,
    /// How many steps from a focused module to show (in both directions).
    pub focus_depth: usize,
    /// Only modules matching this, and every module that (indirectly) depends on them.
    pub reaches: Option<Regex>,
    pub highlight: Option<Regex>,
    /// Entry points: only modules they reach (within `max_depth` steps).
    pub from: Option<Regex>,
    pub max_depth: Option<usize>,
    pub externals: bool,
    pub type_only: bool,
}

impl GraphView {
    /// Modules left by `focus` / `reaches` (None = all).
    pub fn selected(&self, g: &Graph) -> Option<HashSet<usize>> {
        let hits = |re: &Regex| -> Vec<usize> { (0..g.modules.len()).filter(|&m| re.is_match(&g.modules[m].id)).collect() };
        let focused = self.focus.as_ref().map(|re| {
            let mut keep: HashSet<usize> = HashSet::new();
            for forward in [true, false] {
                let mut level: Vec<usize> = hits(re);
                keep.extend(&level);
                for _ in 0..self.focus_depth {
                    let mut next = vec![];
                    for &m in &level {
                        let adj = if forward { &g.out[m] } else { &g.inc[m] };
                        for &e in adj {
                            let n = if forward { g.edges[e].to } else { g.edges[e].from };
                            if keep.insert(n) {
                                next.push(n);
                            }
                        }
                    }
                    level = next;
                }
            }
            keep
        });
        let reaching = self.reaches.as_ref().map(|re| g.closure(&hits(re), false));
        let reached = self.from.as_ref().map(|re| {
            let mut keep: HashSet<usize> = hits(re).into_iter().collect();
            let mut level: Vec<usize> = keep.iter().copied().collect();
            let mut depth = 0;
            while !level.is_empty() && self.max_depth.is_none_or(|max| depth < max) {
                level = level
                    .iter()
                    .flat_map(|&m| g.out[m].iter().map(|&e| g.edges[e].to))
                    .filter(|&n| keep.insert(n))
                    .collect();
                depth += 1;
            }
            keep
        });
        [focused, reaching, reached].into_iter().flatten().reduce(|a, b| a.intersection(&b).copied().collect())
    }

    pub fn highlighted(&self, g: &Graph, m: usize) -> bool {
        self.highlight.as_ref().is_some_and(|re| re.is_match(&g.modules[m].id))
    }
}

struct Projected {
    nodes: Vec<(String, ModuleKind, bool, bool)>, // id, kind, in a cycle, highlighted
    edges: Vec<(usize, usize, bool, bool, bool)>, // from, to, circular, type-only, dynamic
}

/// Applies collapse/focus/filters and aggregates the graph for export.
fn project(g: &Graph, view: &GraphView) -> Projected {
    let name = |m: usize| -> String {
        let module = &g.modules[m];
        match (&view.collapse, module.kind) {
            (Some(Collapse::Depth(depth)), ModuleKind::Local) => {
                let parts: Vec<&str> = module.id.split('/').collect();
                if parts.len() > *depth { parts[..*depth].join("/") + "/" } else { module.id.clone() }
            }
            (Some(Collapse::Pattern(re)), _) => re.find(&module.id).map_or(module.id.clone(), |m| m.as_str().to_string()),
            _ => module.id.clone(),
        }
    };
    let keep_module = |m: usize| view.externals || g.modules[m].kind == ModuleKind::Local;
    let selected = view.selected(g);
    let visible = |m: usize| keep_module(m) && selected.as_ref().is_none_or(|f| f.contains(&m));

    let mut index: HashMap<String, usize> = HashMap::new();
    let mut p = Projected { nodes: vec![], edges: vec![] };
    let mut node = |p: &mut Projected, m: usize| -> usize {
        let id = name(m);
        let n = *index.entry(id.clone()).or_insert_with(|| {
            p.nodes.push((id, g.modules[m].kind, false, false));
            p.nodes.len() - 1
        });
        // A collapsed node is highlighted if any module in it is.
        p.nodes[n].3 |= view.highlighted(g, m);
        n
    };
    for m in 0..g.modules.len() {
        if visible(m) && (g.modules[m].kind == ModuleKind::Local || !g.inc[m].is_empty()) {
            let n = node(&mut p, m);
            if g.cycle_of[m].is_some() && view.collapse.is_none() {
                p.nodes[n].2 = true;
            }
        }
    }
    let mut agg: BTreeMap<(usize, usize), (bool, bool, bool)> = BTreeMap::new();
    for e in &g.edges {
        if !visible(e.from) || !visible(e.to) || (!view.type_only && e.flags.type_only) {
            continue;
        }
        let (a, b) = (node(&mut p, e.from), node(&mut p, e.to));
        if a == b && view.collapse.is_some() {
            continue;
        }
        let entry = agg.entry((a, b)).or_insert((false, true, true));
        entry.0 |= e.circular;
        entry.1 &= e.flags.type_only;
        entry.2 &= e.flags.dynamic;
    }
    p.edges = agg.into_iter().map(|((a, b), (c, t, d))| (a, b, c, t, d)).collect();
    p
}

pub fn dot(g: &Graph, view: &GraphView) -> String {
    let p = project(g, view);
    let mut out = String::from(
        "digraph detangle {\n  rankdir=LR;\n  splines=true;\n  node [shape=box, style=\"rounded,filled\", fontname=\"Helvetica\", fontsize=10, fillcolor=\"#ffffff\", color=\"#999999\"];\n  edge [color=\"#00000055\", arrowsize=0.6];\n",
    );
    for (i, (id, kind, cyclic, highlighted)) in p.nodes.iter().enumerate() {
        let (fill, border) = match kind {
            ModuleKind::Local if *cyclic => ("#fde2e1", "#d33"),
            ModuleKind::Local if id.ends_with('/') => ("#eef3ff", "#6b8cce"),
            ModuleKind::Local => ("#ffffff", "#999999"),
            ModuleKind::Npm => ("#e0f4f7", "#2a9bb0"),
            ModuleKind::Builtin => ("#e5f5e5", "#3a3"),
            ModuleKind::Unresolved => ("#ffd6d6", "#c00"),
        };
        let (fill, border, pen) = if *highlighted { ("#fff3b0", "#e6a100", ", penwidth=2") } else { (fill, border, "") };
        let _ = writeln!(out, "  n{i} [label=\"{}\", fillcolor=\"{fill}\", color=\"{border}\"{pen}];", id.replace('"', "\\\""));
    }
    for (a, b, circular, type_only, dynamic) in &p.edges {
        let mut attrs = vec![];
        if *circular {
            attrs.push("color=\"#dd3333\", penwidth=1.5".to_string());
        }
        if *type_only {
            attrs.push("style=dashed".into());
        } else if *dynamic {
            attrs.push("style=dotted".into());
        }
        let _ = writeln!(out, "  n{a} -> n{b}{};", if attrs.is_empty() { String::new() } else { format!(" [{}]", attrs.join(", ")) });
    }
    out.push_str("}\n");
    out
}

pub fn mermaid(g: &Graph, view: &GraphView) -> String {
    let p = project(g, view);
    let mut out = String::from("flowchart LR\n");
    for (i, (id, kind, cyclic, highlighted)) in p.nodes.iter().enumerate() {
        let class = match kind {
            _ if *highlighted => ":::highlight",
            ModuleKind::Local if *cyclic => ":::cycle",
            ModuleKind::Local => "",
            ModuleKind::Npm => ":::npm",
            ModuleKind::Builtin => ":::core",
            ModuleKind::Unresolved => ":::unresolved",
        };
        let _ = writeln!(out, "  n{i}[\"{}\"]{class}", id.replace('"', "#quot;"));
    }
    let mut red = vec![];
    for (k, (a, b, circular, type_only, dynamic)) in p.edges.iter().enumerate() {
        let arrow = if *type_only || *dynamic { "-.->" } else { "-->" };
        let _ = writeln!(out, "  n{a} {arrow} n{b}");
        if *circular {
            red.push(k.to_string());
        }
    }
    if !red.is_empty() {
        let _ = writeln!(out, "  linkStyle {} stroke:#d33,stroke-width:2px", red.join(","));
    }
    out.push_str("  classDef npm fill:#e0f4f7,stroke:#2a9bb0\n  classDef core fill:#e5f5e5,stroke:#3a3\n  classDef unresolved fill:#ffd6d6,stroke:#c00\n  classDef cycle fill:#fde2e1,stroke:#d33\n  classDef highlight fill:#fff3b0,stroke:#e6a100,stroke-width:2px\n");
    out
}

/// D2: modules nested in containers by path segment.
pub fn d2(g: &Graph, view: &GraphView) -> String {
    let p = project(g, view);
    let key = |id: &str| {
        id.trim_end_matches('/').split('/').map(|s| format!("\"{}\"", s.replace('"', "\\\""))).collect::<Vec<_>>().join(".")
    };
    let mut out = String::from("# modules\n\n");
    for (id, kind, cyclic, highlighted) in &p.nodes {
        let class = match kind {
            _ if *highlighted => "highlight",
            ModuleKind::Local if *cyclic => "cycle",
            ModuleKind::Local => "module",
            ModuleKind::Npm => "npm",
            ModuleKind::Builtin => "core",
            ModuleKind::Unresolved => "unresolved",
        };
        let _ = writeln!(out, "{}: {{class: {class}; link: \"{}\"}}", key(id), id.replace('"', "\\\""));
    }
    out.push_str("\n# dependencies\n\n");
    for (a, b, circular, type_only, dynamic) in &p.edges {
        let mut style = vec![];
        if *circular {
            style.push("style.stroke: \"#dd3333\"");
        }
        if *type_only || *dynamic {
            style.push("style.stroke-dash: 3");
        }
        let attrs = if style.is_empty() { String::new() } else { format!(": {{{}}}", style.join("; ")) };
        let _ = writeln!(out, "{} -> {}{attrs}", key(&p.nodes[*a].0), key(&p.nodes[*b].0));
    }
    out.push_str(
        "\n# styling\n\nclasses: {\n  module: {height: 30; style.border-radius: 10}\n  cycle: {height: 30; style.border-radius: 10; style.fill: \"#fde2e1\"; style.stroke: \"#dd3333\"}\n  npm: {height: 30; style.fill: \"#e0f4f7\"; style.stroke: \"#2a9bb0\"}\n  core: {height: 30; style.fill: \"#e5f5e5\"; style.stroke: \"#33aa33\"}\n  unresolved: {height: 30; style.fill: \"#ffd6d6\"; style.stroke: \"#cc0000\"}\n  highlight: {height: 30; style.border-radius: 10; style.fill: \"#fff3b0\"; style.stroke: \"#e6a100\"; style.stroke-width: 3}\n}\n",
    );
    out
}

/// CSV adjacency matrix: a row per module, "true" where it depends on the column's module.
pub fn csv(g: &Graph, view: &GraphView) -> String {
    let p = project(g, view);
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut order: Vec<usize> = (0..p.nodes.len()).collect();
    order.sort_by(|&a, &b| p.nodes[a].0.cmp(&p.nodes[b].0));
    let deps: HashSet<(usize, usize)> = p.edges.iter().map(|e| (e.0, e.1)).collect();
    let mut out = String::new();
    let header: Vec<String> = std::iter::once(q("")).chain(order.iter().map(|&i| q(&p.nodes[i].0))).chain([q("")]).collect();
    let _ = writeln!(out, "{}", header.join(","));
    for &row in &order {
        let cells: Vec<String> = std::iter::once(q(&p.nodes[row].0))
            .chain(order.iter().map(|&col| q(if deps.contains(&(row, col)) { "true" } else { "false" })))
            .chain([q("")])
            .collect();
        let _ = writeln!(out, "{}", cells.join(","));
    }
    out
}

pub fn stats(g: &Graph, vs: &[Violation], top: usize) -> String {
    let p = Paint::stdout();
    let mut out = String::new();
    let kinds = |k| g.modules.iter().filter(|m| m.kind == k).count();
    let parse_errors: usize = g.modules.iter().map(|m| m.parse_errors).sum();
    let (e, w, i) = counts(vs);
    let _ = writeln!(out, "{}", p.bold("Overview"));
    let _ = writeln!(out, "  modules        {}  ({} parsed)", g.local_count(), g.modules.iter().filter(|m| m.scanned).count());
    let _ = writeln!(out, "  dependencies   {}", g.edges.len());
    let _ = writeln!(out, "  npm packages   {}", kinds(ModuleKind::Npm));
    let _ = writeln!(out, "  node builtins  {}", kinds(ModuleKind::Builtin));
    let _ = writeln!(out, "  unresolved     {}", kinds(ModuleKind::Unresolved));
    let _ = writeln!(out, "  cycles         {}  ({} modules involved)", g.cycles.len(), g.cycles.iter().map(Vec::len).sum::<usize>());
    let _ = writeln!(out, "  orphans        {}", (0..g.modules.len()).filter(|&m| g.is_orphan(m)).count());
    let _ = writeln!(out, "  violations     {e} errors, {w} warnings, {i} info");
    if parse_errors > 0 {
        let _ = writeln!(out, "  parse errors   {}", p.yellow(&parse_errors.to_string()));
    }
    let t = g.timings;
    let _ = writeln!(out, "  time           {:.0}ms {}", g.total_ms(), p.dim(&format!("(scan {:.0}ms · graph {:.0}ms)", t.scan_ms, t.graph_ms)));

    let mut section = |title: &str, rows: Vec<(usize, usize)>| {
        let _ = writeln!(out, "\n{}", p.bold(title));
        for (m, n) in rows.into_iter().take(top).filter(|(_, n)| *n > 0) {
            let _ = writeln!(out, "  {:>5}  {}", n, g.modules[m].id);
        }
    };
    let ranked = |key: &dyn Fn(usize) -> usize, kind: Option<ModuleKind>| {
        let mut v: Vec<(usize, usize)> = (0..g.modules.len())
            .filter(|&m| kind.is_none_or(|k| g.modules[m].kind == k))
            .map(|m| (m, key(m)))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| g.modules[a.0].id.cmp(&g.modules[b.0].id)));
        v
    };
    section("Most depended-on modules (fan-in)", ranked(&|m| g.fan_in(m), Some(ModuleKind::Local)));
    section("Most dependencies (fan-out)", ranked(&|m| g.fan_out(m), Some(ModuleKind::Local)));
    section("Most used packages", ranked(&|m| g.fan_in(m), Some(ModuleKind::Npm)));
    out
}

const HTML_TEMPLATE: &str = include_str!("report.html");

/// A self-contained HTML report (no external requests).
pub fn html(g: &Graph, vs: &[Violation], config: Option<&std::path::Path>) -> String {
    let kind = |k: ModuleKind| match k {
        ModuleKind::Local => 0,
        ModuleKind::Npm => 1,
        ModuleKind::Builtin => 2,
        ModuleKind::Unresolved => 3,
    };
    let mut edges = Vec::with_capacity(g.edges.len() * 3);
    for e in &g.edges {
        let f = e.flags;
        let mut bits = 0u32;
        for (on, bit) in [
            (f.type_only, 1),
            (f.dynamic, 2),
            (f.require, 4),
            (f.reexport, 8),
            (f.resource, 16),
            (e.circular, 32),
            (e.types.contains(&"npm-dev"), 64),
            (e.types.contains(&"npm-peer"), 128),
            (e.types.contains(&"npm-optional"), 256),
            (e.types.contains(&"npm-undeclared"), 512),
        ] {
            if on {
                bits |= bit;
            }
        }
        edges.extend([e.from as u32, e.to as u32, bits]);
    }
    let mut comments = serde_json::Map::new();
    for v in vs {
        if let Some(c) = &v.comment {
            comments.entry(v.rule.clone()).or_insert_with(|| json!(c));
        }
    }
    // A rule covering several checks has no single comment: each row shows its own.
    for v in vs {
        if comments.get(&v.rule).is_some_and(|c| c.as_str() != v.comment.as_deref()) {
            comments.insert(v.rule.clone(), json!(""));
        }
    }
    let generated = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let data = json!({
        "project": g.root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| g.root.display().to_string()),
        "root": g.root.display().to_string(),
        "generated": generated,
        "ms": g.total_ms(),
        "version": env!("CARGO_PKG_VERSION"),
        "config": config.map(|p| p.display().to_string()),
        "m": g.modules.iter().map(|m| &m.id).collect::<Vec<_>>(),
        "k": g.modules.iter().map(|m| kind(m.kind)).collect::<Vec<_>>(),
        "cy": g.cycle_of.iter().map(|c| c.map_or(-1, |c| c as i64)).collect::<Vec<_>>(),
        "e": edges,
        "cycles": g.cycles,
        "loops": (0..g.cycles.len()).map(|c| g.representative_cycle(c)).collect::<Vec<_>>(),
        "v": vs.iter().map(|v| json!({
            "r": v.rule,
            "s": v.severity.as_str(),
            "sc": v.scope,
            "f": v.source_id(g),
            "t": v.target_id(g),
            "c": v.cycle_ids(g),
            "i": imports(g, v).map(|(from, specifier, _)| [from, specifier]).collect::<Vec<_>>(),
            // Its own explanation, when the rule's other violations differ.
            "m": v.comment.as_ref().filter(|c| comments.get(&v.rule).and_then(|r| r.as_str()) != Some(c.as_str())),
        })).collect::<Vec<_>>(),
        "rc": comments,
    });
    // Keep `</script>` inside strings from closing the data block.
    let data = data.to_string().replace("</", "<\\/");
    HTML_TEMPLATE.replacen("/*__DETANGLE_DATA__*/", &data, 1)
}
