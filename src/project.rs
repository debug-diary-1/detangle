use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

use crate::config::{self, CacheStrategy, Config, Scope, Severity};
use crate::graph::{Graph, ModuleKind};
use crate::rules::{self, Violation};
use crate::{groups, report, scan, timing, watch};

pub struct Analysis {
    pub graph: Graph,
    /// Violations, minus those in the configured baseline.
    pub violations: Vec<Violation>,
    pub config_path: Option<PathBuf>,
    /// How many violations the baseline suppressed.
    pub suppressed: usize,
    /// Baseline entries that no longer occur.
    pub stale: Vec<rules::BaselineEntry>,
    /// Which violations show in which file, built on first use.
    placement: OnceLock<Placement>,
}

/// A violation as shown in one file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileViolation {
    pub rule: String,
    pub severity: Severity,
    /// `rule: from → to (cycle: …) — comment`, from the fields `check -f json` prints.
    pub message: String,
    /// The import strings in the file to show it on; empty means the file
    /// as a whole.
    pub specifiers: Vec<String>,
}

/// Which violations show in which file (the design's placement table).
#[derive(Default)]
struct Placement {
    /// Module scope, by `from`: shown on its imports of `to`, or on the
    /// file as a whole when there's no `to`.
    by_module: rustc_hash::FxHashMap<usize, Vec<usize>>,
    /// Folder scope with a `to`, by (from, to) folder node: shown on every
    /// import behind that folder edge.
    by_folder_edge: rustc_hash::FxHashMap<(usize, usize), Vec<usize>>,
    /// Group scope, by the importing module of each import behind it:
    /// (violation, module edge).
    by_import: rustc_hash::FxHashMap<usize, Vec<(usize, usize)>>,
}

impl Placement {
    fn new(g: &Graph, violations: &[Violation]) -> Self {
        let mut p = Placement::default();
        for (i, v) in violations.iter().enumerate() {
            match v.scope {
                Scope::Module => p.by_module.entry(v.from).or_default().push(i),
                Scope::Folder => {
                    // Without a `to` there's no import to show it on.
                    if let Some(to) = v.to {
                        p.by_folder_edge.entry((v.from, to)).or_default().push(i);
                    }
                }
                Scope::Group => {
                    for &e in &v.imports {
                        p.by_import.entry(g.edges[e].from).or_default().push((i, e));
                    }
                }
            }
        }
        p
    }
}

impl Analysis {
    /// The violations to show in `file` (an absolute, canonical path).
    pub fn violations_for(&self, file: &Path) -> Vec<FileViolation> {
        let g = &self.graph;
        let Ok(rel) = file.strip_prefix(&g.root) else { return Vec::new() };
        let Some(m) = g.find(ModuleKind::Local, &rel.to_string_lossy().replace('\\', "/")) else { return Vec::new() };
        let p = self.placement.get_or_init(|| Placement::new(g, &self.violations));
        // Violation index → import strings, merged across the rows below.
        let mut found: std::collections::BTreeMap<usize, Vec<String>> = Default::default();
        let mut add = |i: usize, specs: &mut dyn Iterator<Item = &str>| {
            let list = found.entry(i).or_default();
            for s in specs {
                if !list.iter().any(|x| x == s) {
                    list.push(s.to_string());
                }
            }
        };
        let specifiers = |e: usize| g.edges[e].imports().into_iter().map(|(s, _)| s);
        for &i in p.by_module.get(&m).into_iter().flatten() {
            let to = self.violations[i].to;
            add(i, &mut g.out[m].iter().filter(|&&e| Some(g.edges[e].to) == to).flat_map(|&e| specifiers(e)));
        }
        if !p.by_folder_edge.is_empty() {
            for &e in &g.out[m] {
                for fe in g.folder_edges(e) {
                    for &i in p.by_folder_edge.get(&fe).into_iter().flatten() {
                        add(i, &mut specifiers(e));
                    }
                }
            }
        }
        for &(i, e) in p.by_import.get(&m).into_iter().flatten() {
            add(i, &mut specifiers(e));
        }
        found
            .into_iter()
            .map(|(i, specifiers)| {
                let v = &self.violations[i];
                FileViolation { rule: v.rule.clone(), severity: v.severity, message: message(g, v), specifiers }
            })
            .collect()
    }
}

/// `rule: from → to (cycle: a → b → a) — comment`, leaving out the parts a
/// violation doesn't have.
fn message(g: &Graph, v: &Violation) -> String {
    let mut m = format!("{}: {}", v.rule, v.source_id(g));
    if let Some(to) = v.target_id(g) {
        m += &format!(" → {to}");
    }
    let cycle = v.cycle_ids(g);
    if cycle.len() > 1 {
        m += &format!(" (cycle: {})", cycle.join(" → "));
    }
    if let Some(c) = &v.comment {
        m += &format!(" — {c}");
    }
    m
}

/// The CLI's `--cache` / `--cache-strategy`, overriding the config's cache
/// settings. The default overrides nothing.
#[derive(Clone, Debug, Default)]
pub struct CacheArgs {
    /// `--cache` alone is `Some(None)`; `--cache DIR` is `Some(Some(DIR))`.
    pub cache: Option<Option<PathBuf>>,
    pub strategy: Option<CacheStrategy>,
}

/// A loaded project whose scan can be kept up to date incrementally.
pub struct Project {
    dir: PathBuf,
    root: PathBuf,
    config_arg: Option<PathBuf>,
    mode_arg: Option<String>,
    cache_args: CacheArgs,
    cfg: Config,
    config_path: Option<PathBuf>,
    /// Notes from loading the config (e.g. JavaScript config import warnings).
    notes: Vec<String>,
    /// `[[groups]]` and discovered Nx projects.
    groups: Vec<groups::Group>,
    session: scan::Session,
}

impl Project {
    pub fn open(path: &Path, config: Option<&Path>, mode: Option<&str>, cache_args: &CacheArgs) -> Result<Self> {
        let dir = dunce::canonicalize(path).with_context(|| format!("{} not found", path.display()))?;
        if !dir.is_dir() {
            bail!("{} is not a directory", path.display());
        }
        let root = config::find_root(&dir);
        let t = std::time::Instant::now();
        let loaded = config::load(&root, config)?;
        let mut cfg: Config = loaded.config;
        if let Some(m) = mode {
            cfg.options.config_env.mode = Some(m.to_string());
        }
        match &cache_args.cache {
            Some(Some(d)) => cfg.options.cache = config::CacheSetting::Dir(std::path::absolute(d)?.to_string_lossy().into_owned()),
            Some(None) => cfg.options.cache = config::CacheSetting::Enabled(true),
            None => {}
        }
        if let Some(s) = cache_args.strategy {
            cfg.options.cache_strategy = s;
        }
        rules::validate(&cfg).with_context(|| match &loaded.path {
            Some(p) => format!("in {}", p.display()),
            None => "in the built-in rules".into(),
        })?;
        timing("config", t);
        let t = std::time::Instant::now();
        let session = scan::Session::new(&root, &dir, &cfg.options)?;
        timing("scan", t);
        let t = std::time::Instant::now();
        let groups = groups::resolve(&root, &cfg)?;
        timing("groups", t);
        Ok(Project {
            groups,
            dir,
            root,
            config_arg: config.map(Path::to_path_buf),
            mode_arg: mode.map(String::from),
            cache_args: cache_args.clone(),
            cfg,
            config_path: loaded.path,
            notes: loaded.notes,
            session,
        })
    }

    /// The project root (where detangle.toml / package.json was found).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The loaded config, with command-line overrides applied.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Notes from loading the config (e.g. JavaScript config import warnings).
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// The files that decide this project's configuration and resolution
    /// (see `Stamps`), as they are now.
    pub fn config_stamps(&self) -> crate::stamps::Stamps {
        let config = self.config_path.as_deref().or(self.config_arg.as_deref());
        crate::stamps::Stamps::collect(&self.root, config, Some(&self.cfg.options))
    }

    /// The scan, for callers keeping it up to date themselves (`Live`).
    pub fn session_mut(&mut self) -> &mut scan::Session {
        &mut self.session
    }

    pub fn analyze(&self) -> Result<Analysis> {
        self.analyze_with(true)
    }

    /// The configured baseline file, if any (it may not exist yet).
    pub fn baseline_path(&self) -> Option<PathBuf> {
        self.cfg.options.baseline.as_ref().map(|b| self.root.join(b))
    }

    pub fn analyze_with(&self, use_baseline: bool) -> Result<Analysis> {
        let t = std::time::Instant::now();
        let mut graph = Graph::build(&self.root, self.session.files(), self.session.work, &self.cfg.options);
        graph.assign_groups(&self.groups, self.cfg.options.group_match == config::GroupMatch::Deepest);
        timing("graph", t);
        let t = std::time::Instant::now();
        let mut violations = rules::evaluate(&graph, &self.cfg)?;
        timing("rules", t);
        let used = match self.baseline_path().filter(|p| use_baseline && p.is_file()) {
            Some(p) => rules::apply_baseline(&graph, &mut violations, &p)?,
            None => Default::default(),
        };
        Ok(Analysis {
            graph,
            violations,
            config_path: self.config_path.clone(),
            suppressed: used.suppressed,
            stale: used.stale,
            placement: OnceLock::new(),
        })
    }

    /// Applies filesystem changes and re-analyses. Returns the new analysis
    /// and a one-line description of the work done. On error (e.g. a
    /// half-edited detangle.toml) the previous state is kept.
    /// Returns `None` for the analysis when it can't have changed: the edited
    /// files still import exactly what they did (the common case of editing
    /// code rather than imports).
    pub fn rebuild(&mut self, changes: &watch::Changes) -> Result<(Option<Analysis>, String)> {
        let t = std::time::Instant::now();
        if changes.needs_full() {
            *self = Project::open(&self.dir, self.config_arg.as_deref(), self.mode_arg.as_deref(), &self.cache_args)?;
        } else {
            self.session.update(&changes.paths.iter().cloned().collect::<Vec<_>>())?;
        }
        let w = self.session.work;
        let a = if w.graph_changed { Some(self.analyze()?) } else { None };
        let work = if changes.needs_full() {
            format!("full rebuild of {} files", w.reparsed)
        } else if w.walked && w.reresolved > w.reparsed {
            format!("{} reparsed, all re-resolved", w.reparsed)
        } else if a.is_none() {
            report::plural(w.reparsed, "file") + " reparsed, imports unchanged"
        } else {
            report::plural(w.reparsed, "file") + " reparsed"
        };
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let what = if changes.paths.is_empty() { String::new() } else { format!("{} · ", changes.describe(&self.root)) };
        Ok((a, format!("↻ {what}{work} · {ms:.0}ms")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project in a fresh temp directory.
    fn temp_project(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let tmp = std::env::temp_dir().join(format!("detangle-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for (rel, body) in files {
            let path = tmp.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        dunce::canonicalize(&tmp).unwrap()
    }

    #[test]
    fn violations_for_a_file() {
        let root = temp_project(
            "violations-for",
            &[
                (
                    "detangle.toml",
                    r#"
[[forbidden]]
name = "no-b"
severity = "error"
comment = "Keep away from b"
from = { path = '^src/a' }
to = { path = '^src/b' }

[[forbidden]]
name = "no-cycles"
severity = "warn"
to = { circular = true }
"#,
                ),
                ("src/a.ts", "import type { B } from './b';\nimport { b } from './b.ts';\nimport './c';"),
                ("src/b.ts", "export const b = 1; export type B = 1;"),
                ("src/c.ts", "import './d';"),
                ("src/d.ts", "import './c';"),
            ],
        );
        let p = Project::open(&root, None, None, &CacheArgs::default()).unwrap();
        let a = p.analyze().unwrap();
        let summary = |file: &str| -> Vec<(String, Severity, String, Vec<String>)> {
            let mut v: Vec<_> = a
                .violations_for(&root.join(file))
                .into_iter()
                .map(|v| (v.rule, v.severity, v.message, v.specifiers))
                .collect();
            v.sort();
            v
        };
        // Both imports of b, each once.
        assert_eq!(
            summary("src/a.ts"),
            [("no-b".into(), Severity::Error, "no-b: src/a.ts → src/b.ts — Keep away from b".into(), vec!["./b".into(), "./b.ts".into()])]
        );
        assert_eq!(
            summary("src/c.ts"),
            [("no-cycles".into(), Severity::Warn, "no-cycles: src/c.ts → src/d.ts (cycle: src/c.ts → src/d.ts → src/c.ts)".into(), vec!["./d".into()])]
        );
        assert!(summary("src/b.ts").is_empty());
        assert!(summary("elsewhere.ts").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Folder and group violations show on the imports behind them, in
    /// every file that has one; module violations without a `to` show on
    /// the file as a whole; ones with no import behind them nowhere.
    #[test]
    fn violations_for_every_scope() {
        let root = dunce::canonicalize(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/eslint")).unwrap();
        let a = Project::open(&root, None, None, &CacheArgs::default()).unwrap().analyze().unwrap();
        let shown = |file: &str| -> Vec<(String, Vec<String>)> {
            let mut v: Vec<_> = a.violations_for(&root.join(file)).into_iter().map(|v| (v.rule, v.specifiers)).collect();
            v.sort();
            v
        };
        let s = |x: &str| vec![x.to_string()];
        assert_eq!(
            shown("src/features/cart/ui/button.ts"),
            [("features-independent".into(), s("../../orders/model")), ("no-cross-feature".into(), s("../../orders/model"))]
        );
        assert_eq!(
            shown("src/features/cart/index.ts"),
            [("features-independent".into(), s("../orders/api")), ("no-cross-feature".into(), s("../orders/api"))]
        );
        assert_eq!(shown("src/lonely.ts"), [("no-orphans".into(), vec![])]);
        assert_eq!(shown("src/pages/home.ts"), [("no-legacy".into(), s("../legacy/old")), ("pages-use-shell".into(), vec![])]);
        // orders-folder-unused and orders-group-unused have no import to show on.
        assert!(a.violations.iter().any(|v| v.rule == "orders-folder-unused"));
        assert!(shown("src/features/orders/api.ts").is_empty());
    }

    /// After lost events, rebuild re-opens the project, so it sees files
    /// no event reported.
    #[test]
    fn rescan_reopens_the_project() {
        let tmp = std::env::temp_dir().join(format!("detangle-rescan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("package.json"), "{}").unwrap();
        std::fs::write(tmp.join("src/a.ts"), "import './b';").unwrap();
        let mut p = Project::open(&tmp, None, None, &CacheArgs::default()).unwrap();
        let local = |a: &Analysis| a.graph.local_count();
        assert_eq!(local(&p.analyze().unwrap()), 1);

        // Created without a watcher event reaching us.
        std::fs::write(tmp.join("src/b.ts"), "").unwrap();
        let (a, status) = p.rebuild(&watch::Changes::default()).unwrap();
        assert!(a.is_none(), "{status}");

        let lost = watch::Changes { rescan: true, ..Default::default() };
        let (a, status) = p.rebuild(&lost).unwrap();
        assert!(status.contains("full rebuild of 2 files"), "{status}");
        assert_eq!(local(&a.unwrap()), 2);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
