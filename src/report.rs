//! Non-interactive output: rule reports, graph exports and stats.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::IsTerminal;

use regex::Regex;
use serde_json::json;

use crate::config::Severity;
use crate::graph::{Graph, ModuleKind};
use crate::rules::Violation;

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
        (g.modules[e.from].id.as_str(), e.specifier.as_str(), g.modules[e.to].id.as_str())
    })
}

pub fn text(g: &Graph, vs: &[Violation], suppressed: usize) -> String {
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
    let (e, w, i) = counts(vs);
    let summary = format!(
        "{} modules, {} dependencies, {} · {:.0}ms",
        g.local_count(),
        g.edges.len(),
        plural(g.cycles.len(), "cycle"),
        g.total_ms()
    );
    let status = if vs.is_empty() {
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

/// GitHub Actions workflow commands → inline PR annotations.
pub fn github(g: &Graph, vs: &[Violation]) -> String {
    let esc = |s: &str| s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A");
    let mut out = String::new();
    for v in vs {
        let level = match v.severity {
            Severity::Error => "error",
            Severity::Warn => "warning",
            _ => "notice",
        };
        let mut msg = match v.target_id(g) {
            Some(t) => format!("depends on {t}"),
            None => String::new(),
        };
        if v.cycle.len() > 1 {
            msg = format!("cycle: {}", v.cycle_ids(g).join(" → "));
        }
        if let Some(c) = &v.comment {
            msg = if msg.is_empty() { c.clone() } else { format!("{msg}\n{c}") };
        }
        // Group violations are annotated on the imports behind them.
        if !v.imports.is_empty() {
            for (file, specifier, _) in imports(g, v) {
                let what = format!("imports {specifier} ({} → {})\n{msg}", v.source_id(g), v.target_id(g).unwrap_or(""));
                let _ = writeln!(out, "::{level} file={file},title={}::{}", v.rule, esc(&what));
            }
            continue;
        }
        let _ = writeln!(
            out,
            "::{level} file={},title={}::{}",
            v.source_id(g),
            v.rule,
            esc(&msg)
        );
    }
    out
}

pub fn violations_json(g: &Graph, vs: &[Violation]) -> serde_json::Value {
    vs.iter()
        .map(|v| {
            json!({
                "rule": v.rule,
                "severity": v.severity,
                "comment": v.comment,
                "scope": v.scope,
                "from": v.source_id(g),
                "to": v.target_id(g),
                "cycle": v.cycle_ids(g),
                "imports": imports(g, v).map(|(from, specifier, to)| json!({ "from": from, "specifier": specifier, "to": to })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

pub fn full_json(g: &Graph, vs: &[Violation]) -> serde_json::Value {
    let (e, w, i) = counts(vs);
    json!({
        "summary": {
            "modules": g.local_count(),
            "dependencies": g.edges.len(),
            "cycles": g.cycles.len(),
            "errors": e, "warnings": w, "info": i,
            "timings": g.timings,
        },
        "modules": g.modules.iter().enumerate().map(|(m, module)| json!({
            "id": module.id,
            "kind": module.kind,
            "fanIn": g.fan_in(m),
            "fanOut": g.fan_out(m),
            "instability": (g.instability(m) * 1000.0).round() / 1000.0,
            "cycle": g.cycle_of[m],
            "dependencies": g.out[m].iter().map(|&e| {
                let edge = &g.edges[e];
                json!({
                    "module": g.modules[edge.to].id,
                    "specifier": edge.specifier,
                    "types": edge.types,
                    "circular": edge.circular,
                })
            }).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "cycles": g.cycles.iter().map(|c| c.iter().map(|&m| &g.modules[m].id).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "violations": violations_json(g, vs),
    })
}

pub struct GraphView {
    pub collapse: Option<usize>,
    pub focus: Option<Regex>,
    pub externals: bool,
    pub type_only: bool,
}

struct Projected {
    nodes: Vec<(String, ModuleKind, bool)>, // id, kind, in a cycle
    edges: Vec<(usize, usize, bool, bool, bool)>, // from, to, circular, type-only, dynamic
}

/// Applies collapse/focus/filters and aggregates the graph for export.
fn project(g: &Graph, view: &GraphView) -> Projected {
    let name = |m: usize| -> String {
        let module = &g.modules[m];
        match (view.collapse, module.kind) {
            (Some(depth), ModuleKind::Local) => {
                let parts: Vec<&str> = module.id.split('/').collect();
                if parts.len() > depth { parts[..depth].join("/") + "/" } else { module.id.clone() }
            }
            _ => module.id.clone(),
        }
    };
    let keep_module = |m: usize| view.externals || g.modules[m].kind == ModuleKind::Local;
    let focused: Option<HashSet<usize>> = view.focus.as_ref().map(|re| {
        let hits: Vec<usize> = (0..g.modules.len()).filter(|&m| re.is_match(&g.modules[m].id)).collect();
        let mut keep: HashSet<usize> = hits.iter().copied().collect();
        for &m in &hits {
            keep.extend(g.out[m].iter().map(|&e| g.edges[e].to));
            keep.extend(g.inc[m].iter().map(|&e| g.edges[e].from));
        }
        keep
    });
    let visible = |m: usize| keep_module(m) && focused.as_ref().is_none_or(|f| f.contains(&m));

    let mut index: HashMap<String, usize> = HashMap::new();
    let mut p = Projected { nodes: vec![], edges: vec![] };
    let mut node = |p: &mut Projected, m: usize| -> usize {
        let id = name(m);
        *index.entry(id.clone()).or_insert_with(|| {
            p.nodes.push((id, g.modules[m].kind, false));
            p.nodes.len() - 1
        })
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
        "digraph tangle {\n  rankdir=LR;\n  splines=true;\n  node [shape=box, style=\"rounded,filled\", fontname=\"Helvetica\", fontsize=10, fillcolor=\"#ffffff\", color=\"#999999\"];\n  edge [color=\"#00000055\", arrowsize=0.6];\n",
    );
    for (i, (id, kind, cyclic)) in p.nodes.iter().enumerate() {
        let (fill, border) = match kind {
            ModuleKind::Local if *cyclic => ("#fde2e1", "#d33"),
            ModuleKind::Local if id.ends_with('/') => ("#eef3ff", "#6b8cce"),
            ModuleKind::Local => ("#ffffff", "#999999"),
            ModuleKind::Npm => ("#e0f4f7", "#2a9bb0"),
            ModuleKind::Builtin => ("#e5f5e5", "#3a3"),
            ModuleKind::Unresolved => ("#ffd6d6", "#c00"),
        };
        let _ = writeln!(out, "  n{i} [label=\"{}\", fillcolor=\"{fill}\", color=\"{border}\"];", id.replace('"', "\\\""));
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
    for (i, (id, kind, cyclic)) in p.nodes.iter().enumerate() {
        let class = match kind {
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
    out.push_str("  classDef npm fill:#e0f4f7,stroke:#2a9bb0\n  classDef core fill:#e5f5e5,stroke:#3a3\n  classDef unresolved fill:#ffd6d6,stroke:#c00\n  classDef cycle fill:#fde2e1,stroke:#d33\n");
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
    let _ = writeln!(out, "  time           {:.0}ms {}", g.total_ms(), p.dim(&format!("(walk {:.0}ms · parse+resolve {:.0}ms · graph {:.0}ms)", t.walk_ms, t.parse_ms, t.graph_ms)));

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
    HTML_TEMPLATE.replacen("/*__TANGLE_DATA__*/", &data, 1)
}
