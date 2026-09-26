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

fn path_str(g: &Graph, path: &[usize]) -> String {
    path.iter().map(|&m| g.modules[m].id.as_str()).collect::<Vec<_>>().join(" → ")
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
        if let Some(c) = &first.comment {
            let _ = writeln!(out, "  {}", p.dim(c));
        }
        for v in list {
            let from = &g.modules[v.from].id;
            match v.to {
                Some(t) => {
                    let to = &g.modules[t];
                    let target = if to.kind == ModuleKind::Local { to.id.clone() } else { p.cyan(&to.id) };
                    let _ = writeln!(out, "  {} {} {}", from, p.dim("→"), target);
                }
                None => {
                    let _ = writeln!(out, "  {from}");
                }
            }
            if v.cycle.len() > 1 {
                let _ = writeln!(out, "    {} {}", p.dim("cycle:"), p.dim(&path_str(g, &v.cycle)));
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
        let mut msg = match v.to {
            Some(t) => format!("depends on {}", g.modules[t].id),
            None => String::new(),
        };
        if v.cycle.len() > 1 {
            msg = format!("cycle: {}", path_str(g, &v.cycle));
        }
        if let Some(c) = &v.comment {
            msg = if msg.is_empty() { c.clone() } else { format!("{msg}\n{c}") };
        }
        let _ = writeln!(
            out,
            "::{level} file={},title={}::{}",
            g.modules[v.from].id,
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
                "from": g.modules[v.from].id,
                "to": v.to.map(|t| &g.modules[t].id),
                "cycle": v.cycle.iter().map(|&m| &g.modules[m].id).collect::<Vec<_>>(),
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
