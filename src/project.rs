use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{self, CacheStrategy, Config};
use crate::graph::Graph;
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
        Ok(Analysis { graph, violations, config_path: self.config_path.clone(), suppressed: used.suppressed, stale: used.stale })
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
