//! A project kept current for an editor: what the ESLint add-on's handle
//! runs on each lint (the design's D6 steps).
//!
//! 1. Freshen: apply what the file watcher reported. Source changes go
//!    through `Session::update`; lost events, or a config file changing in
//!    a directory with sources, re-open the project.
//! 2. Membership: a linted file that isn't scanned is admitted if the walk
//!    would include it (a new file linted before its event arrives), or
//!    remembered as rejected until the next re-open.
//! 3. Overlay: the editor's buffer is the file's contents.
//! 4. Analyse again, once, if anything above changed the graph.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use anyhow::Result;

use crate::scan::has_source_ext;
use crate::watch::{self, Changes, Disconnected, Watcher};
use crate::{Analysis, CacheArgs, FileViolation, Project, config};

/// Counts of the expensive steps, for tests and benchmarks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub opens: usize,
    pub analyses: usize,
    pub eligibility_checks: usize,
}

enum Member {
    No,
    Yes,
    /// Just added, so the graph changed.
    Admitted,
}

enum Watch {
    /// Being set up on its own thread (tens of ms on Linux), overlapping the scan.
    Starting(JoinHandle<Result<Watcher>>),
    Running(Watcher),
    Off,
}

pub struct Live {
    dir: PathBuf,
    config: Option<PathBuf>,
    mode: Option<String>,
    watch: Watch,
    project: Project,
    analysis: Analysis,
    /// Linted files the walk wouldn't include, until the next re-open.
    rejected: HashSet<PathBuf>,
    /// Why the last re-open failed; the previous project stays in use.
    reopen_error: Option<String>,
    /// One-time notices for the user (the watcher failed or stopped).
    warnings: Vec<String>,
    pub stats: Stats,
    /// Changes a test hands to the next drain, as if the watcher saw them.
    #[cfg(test)]
    injected: Option<Changes>,
}

impl Live {
    /// Opens the project like `detangle check <dir> [--config] [--mode]`.
    /// With `watch`, a recursive watcher on the root is started first, on
    /// its own thread, so setting it up overlaps the scan.
    pub fn open(dir: &Path, config: Option<&Path>, mode: Option<&str>, watch: bool) -> Result<Live> {
        let root = dunce::canonicalize(dir).ok().map(|d| config::find_root(&d));
        let watch = match root {
            Some(root) if watch => Watch::Starting(std::thread::spawn(move || Watcher::new(&root))),
            _ => Watch::Off,
        };
        let project = Project::open(dir, config, mode, &CacheArgs::default())?;
        let analysis = project.analyze()?;
        Ok(Live {
            dir: dir.to_path_buf(),
            config: config.map(Path::to_path_buf),
            mode: mode.map(String::from),
            watch,
            project,
            analysis,
            rejected: HashSet::new(),
            reopen_error: None,
            warnings: Vec::new(),
            stats: Stats { opens: 1, analyses: 1, eligibility_checks: 0 },
            #[cfg(test)]
            injected: None,
        })
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Problems to show with every result: a failed re-open.
    pub fn problems(&self) -> Vec<String> {
        self.reopen_error.iter().cloned().collect()
    }

    /// Notices collected since the last call, each given once.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Whether the watcher is up (false while it's still being set up).
    pub fn watching(&self) -> bool {
        matches!(self.watch, Watch::Running(_))
    }

    /// The violations to show in `file` (absolute, canonical), whose editor
    /// buffer holds `text`.
    pub fn violations_for(&mut self, file: &Path, text: &str) -> Result<Vec<FileViolation>> {
        let mut dirty = match self.drain() {
            Some(changes) => self.apply(&changes)?,
            None => false,
        };
        match self.member(file)? {
            Member::No => {}
            Member::Yes => dirty |= self.project.session_mut().overlay(file, text),
            Member::Admitted => {
                self.project.session_mut().overlay(file, text);
                dirty = true;
            }
        }
        if dirty {
            self.analysis = self.project.analyze()?;
            self.stats.analyses += 1;
        }
        Ok(self.analysis.violations_for(file))
    }

    /// The watcher's pending changes, once it's running.
    fn drain(&mut self) -> Option<Changes> {
        #[cfg(test)]
        if let Some(c) = self.injected.take() {
            return Some(c);
        }
        if let Watch::Starting(h) = &self.watch
            && h.is_finished()
        {
            let Watch::Starting(h) = std::mem::replace(&mut self.watch, Watch::Off) else { unreachable!() };
            match h.join() {
                Ok(Ok(w)) => self.watch = Watch::Running(w),
                Ok(Err(e)) => self.warn(format!("{e:#}")),
                Err(_) => self.warn("its setup panicked".into()),
            }
        }
        let Watch::Running(w) = &mut self.watch else { return None };
        match w.drain() {
            Ok(changes) => Some(changes),
            Err(Disconnected) => {
                self.watch = Watch::Off;
                self.warn("it stopped".into());
                None
            }
        }
    }

    fn warn(&mut self, why: String) {
        self.warnings.push(format!(
            "detangle: no file watcher ({why}); changes to other files are seen when those files are linted"
        ));
    }

    /// Applies watcher changes. Returns whether the graph must be rebuilt.
    fn apply(&mut self, changes: &Changes) -> Result<bool> {
        if changes.rescan {
            // Events were lost, so `paths` may be incomplete.
            return Ok(self.reopen());
        }
        let is_config = |p: &Path| p.file_name().and_then(|n| n.to_str()).is_some_and(watch::is_config);
        let sources: Vec<PathBuf> = changes.paths.iter().filter(|p| !is_config(p)).cloned().collect();
        let mut dirty = false;
        if !sources.is_empty() {
            let session = self.project.session_mut();
            session.update(&sources)?;
            dirty = session.work.graph_changed;
        }
        // Classified after the update, so new sources already make their
        // directories members. A config file elsewhere (build output such as
        // .next/package.json) doesn't configure anything detangle scans.
        let session = self.project.session_mut();
        if changes.paths.iter().any(|p| is_config(p) && p.parent().is_some_and(|d| session.is_member_dir(d))) {
            return Ok(self.reopen());
        }
        Ok(dirty)
    }

    /// Opens the project again. On failure the previous project stays in
    /// use and the error is a problem until a re-open succeeds.
    fn reopen(&mut self) -> bool {
        match Project::open(&self.dir, self.config.as_deref(), self.mode.as_deref(), &CacheArgs::default()) {
            Ok(p) => {
                self.project = p;
                self.rejected.clear();
                self.reopen_error = None;
                self.stats.opens += 1;
                true
            }
            Err(e) => {
                self.reopen_error = Some(format!("{e:#}"));
                false
            }
        }
    }

    /// Whether `file` is scanned, admitting it if the walk would include it.
    fn member(&mut self, file: &Path) -> Result<Member> {
        let session = self.project.session_mut();
        if session.contains(file) {
            return Ok(Member::Yes);
        }
        // Untitled buffers, processor-virtual paths (README.md/0.js), deleted files.
        if !has_source_ext(file) || !file.is_file() || self.rejected.contains(file) {
            return Ok(Member::No);
        }
        self.stats.eligibility_checks += 1;
        if !session.eligible(file) {
            self.rejected.insert(file.to_path_buf());
            return Ok(Member::No);
        }
        session.admit(file)?;
        Ok(if session.contains(file) { Member::Admitted } else { Member::No })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str, files: &[(&str, &str)]) -> Tmp {
            let dir = std::env::temp_dir().join(format!("detangle-live-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let t = Tmp(dunce::canonicalize(&dir).unwrap());
            for (rel, body) in files {
                t.write(rel, body);
            }
            t
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
        fn write(&self, rel: &str, body: &str) {
            let p = self.path(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const CONFIG: &str = r#"
[[forbidden]]
name = "no-legacy"
severity = "error"
from = { path = '^src/' }
to = { path = '^src/legacy/' }
"#;

    fn project(name: &str) -> Tmp {
        Tmp::new(
            name,
            &[
                ("detangle.toml", CONFIG),
                ("package.json", "{}"),
                (".gitignore", "gen/\n"),
                ("src/a.ts", "import './b';"),
                ("src/b.ts", ""),
                ("src/legacy/old.ts", ""),
            ],
        )
    }

    fn rules(l: &mut Live, t: &Tmp, file: &str, text: &str) -> Vec<String> {
        let mut v: Vec<String> = l.violations_for(&t.path(file), text).unwrap().into_iter().map(|v| v.rule).collect();
        v.sort();
        v
    }

    fn changes(paths: &[PathBuf]) -> Changes {
        Changes { paths: paths.iter().cloned().collect(), ..Default::default() }
    }

    #[test]
    fn unsaved_buffers_are_checked_live() {
        let t = project("buffer");
        let mut l = Live::open(&t.0, None, None, false).unwrap();
        assert!(rules(&mut l, &t, "src/a.ts", "import './b';").is_empty());
        // An unchanged buffer does no graph work.
        assert_eq!(l.stats.analyses, 1);
        assert_eq!(rules(&mut l, &t, "src/a.ts", "import './b'; import './legacy/old';"), ["no-legacy"]);
        assert_eq!(l.stats.analyses, 2);
        // Typing on without changing imports: no graph work either.
        assert_eq!(rules(&mut l, &t, "src/a.ts", "import './b'; import './legacy/old'; const x = 1;"), ["no-legacy"]);
        assert_eq!(l.stats.analyses, 2);
        assert!(rules(&mut l, &t, "src/a.ts", "import './b';").is_empty());
    }

    #[test]
    fn config_changes_count_only_where_there_are_sources() {
        let t = project("config-events");
        t.write("packages/p/package.json", "{}");
        t.write("packages/p/src/index.ts", "");
        t.write(".next/package.json", "{}");
        let mut l = Live::open(&t.0, None, None, false).unwrap();
        // Build output: no re-open.
        assert!(!l.apply(&changes(&[t.path(".next/package.json")])).unwrap());
        assert_eq!(l.stats.opens, 1);
        // A nested workspace package.json, next to scanned sources: re-open.
        assert!(l.apply(&changes(&[t.path("packages/p/package.json")])).unwrap());
        assert_eq!(l.stats.opens, 2);
        // The root's own config.
        assert!(l.apply(&changes(&[t.path("detangle.toml")])).unwrap());
        assert_eq!(l.stats.opens, 3);
    }

    #[test]
    fn lost_events_reopen() {
        let t = project("rescan");
        let mut l = Live::open(&t.0, None, None, false).unwrap();
        // A file created while events were lost.
        t.write("src/c.ts", "import './legacy/old';");
        assert!(l.apply(&Changes { rescan: true, ..Default::default() }).unwrap());
        assert_eq!(l.stats.opens, 2);
        assert!(l.project.session_mut().contains(&t.path("src/c.ts")));
    }

    #[test]
    fn a_failed_reopen_keeps_the_last_project() {
        let t = project("reopen-fails");
        let mut l = Live::open(&t.0, None, None, false).unwrap();
        t.write("detangle.toml", "[[forbidden]\n");
        assert!(!l.apply(&changes(&[t.path("detangle.toml")])).unwrap());
        assert_eq!(l.problems().len(), 1);
        assert_eq!(rules(&mut l, &t, "src/a.ts", "import './legacy/old';"), ["no-legacy"]);
        t.write("detangle.toml", CONFIG);
        assert!(l.apply(&changes(&[t.path("detangle.toml")])).unwrap());
        assert!(l.problems().is_empty());
    }

    #[test]
    fn new_files_are_admitted_and_excluded_ones_remembered() {
        let t = project("membership");
        let mut l = Live::open(&t.0, None, None, false).unwrap();
        // Saved and linted before any watcher event.
        t.write("src/new.ts", "import './legacy/old';");
        assert_eq!(rules(&mut l, &t, "src/new.ts", "import './legacy/old';"), ["no-legacy"]);
        assert_eq!(l.stats.eligibility_checks, 1);
        // Gitignored: rejected once, then without another check.
        t.write("gen/out.ts", "import '../src/legacy/old';");
        for _ in 0..3 {
            assert!(rules(&mut l, &t, "gen/out.ts", "import '../src/legacy/old';").is_empty());
        }
        assert_eq!(l.stats.eligibility_checks, 2);
        // Not on disk, or not a source file: no work at all.
        assert!(rules(&mut l, &t, "src/untitled.ts", "import './legacy/old';").is_empty());
        assert!(rules(&mut l, &t, "README.md/0.ts", "").is_empty());
        assert_eq!(l.stats.eligibility_checks, 2);
    }

    #[test]
    fn one_analysis_per_call() {
        let t = project("one-analysis");
        let mut l = Live::open(&t.0, None, None, false).unwrap();
        // A saved change elsewhere (via the watcher), a new file and its
        // buffer, all in one lint.
        t.write("src/b.ts", "import './legacy/old';");
        l.injected = Some(changes(&[t.path("src/b.ts")]));
        t.write("src/new.ts", "");
        assert_eq!(rules(&mut l, &t, "src/new.ts", "import './legacy/old';"), ["no-legacy"]);
        assert_eq!(rules(&mut l, &t, "src/b.ts", "import './legacy/old';"), ["no-legacy"]);
        assert_eq!(l.stats.analyses, 2);
    }

    /// With the real watcher: a saved change to another file is seen at the
    /// next lint. (FSEvents needs the Bash sandbox off on macOS.)
    #[test]
    fn saved_changes_arrive_through_the_watcher() {
        let t = project("watcher");
        let mut l = Live::open(&t.0, None, None, true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !l.watching() {
            let _ = l.violations_for(&t.path("src/a.ts"), "import './b';");
            assert!(std::time::Instant::now() < deadline, "watcher never started: {:?}", l.take_warnings());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // FSEvents may deliver events from before the watch started; let them pass.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = l.violations_for(&t.path("src/a.ts"), "import './b';");
        t.write("src/b.ts", "import './legacy/old';");
        loop {
            if rules(&mut l, &t, "src/b.ts", "import './legacy/old';") == ["no-legacy"] && l.stats.analyses >= 2 {
                break;
            }
            assert!(std::time::Instant::now() < deadline + std::time::Duration::from_secs(5), "change never arrived");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
