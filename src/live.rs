//! A project kept current for an editor: the ESLint add-on's handle, as
//! plain Rust. Each lint runs the design's steps (D6):
//!
//! 1. Freshen. Poll the stamps of the files that decide configuration and
//!    resolution, and drain the file watcher. Source changes go through
//!    `Session::update`. A polled config change, lost watcher events, or a
//!    config file changing in a directory with sources re-open the
//!    project; a lockfile change re-resolves imports (packages were
//!    installed). At most one of each, and a re-open replaces the rest.
//! 2. Membership: a linted file that isn't scanned is admitted if the walk
//!    would include it (a new file linted before its event arrives), or
//!    remembered as rejected until the next re-open.
//! 3. Overlay: the editor's buffer is the file's contents.
//! 4. Analyse again, once, if anything above changed the graph.
//!
//! The project moves between the states of D8: `Ready`, `Stale` (a re-open
//! failed; the last good project stays in use), `Broken` (nothing could be
//! opened) and `Failed` (a panic). Stamps are recorded at every open
//! attempt, so a failed one is retried only after a polled file changes,
//! never once per keystroke. Nothing here panics out: a panic moves to
//! `Failed`, which retries at most once per 30 s, and only after something
//! changed.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::scan::has_source_ext;
use crate::stamps::{Kind, Stamps};
use crate::watch::{self, Changes, Disconnected, Watcher};
use crate::{Analysis, CacheArgs, FileViolation, Project, config};

/// A lint's text containing this panics, for testing recovery.
#[cfg(any(test, feature = "test-hooks"))]
pub const TEST_PANIC: &str = "__detangle_test_panic__";

/// Counts of the expensive steps, for tests and benchmarks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Opens that succeeded, and all attempts.
    pub opens: usize,
    pub open_attempts: usize,
    pub analyses: usize,
    pub eligibility_checks: usize,
    /// Re-resolutions after a lockfile changed.
    pub refreshes: usize,
}

/// What to show in one linted file.
#[derive(Debug, Default)]
pub struct Report {
    pub violations: Vec<FileViolation>,
    /// Problems with the project itself, shown at line 1.
    pub problems: Vec<String>,
    /// The config's exotic-require names, for matching calls.
    pub exotic_require: Vec<String>,
}

struct Loaded {
    project: Project,
    analysis: Analysis,
}

enum State {
    Ready(Box<Loaded>),
    /// A re-open failed with this error: the last good project stays in use.
    Stale(Box<Loaded>, String),
    /// Nothing is loaded: the config didn't load, or `dir` is missing.
    Broken(String),
    /// A panic. It's retried at most once per `retry_floor`, and only after
    /// something changed: a polled file, a watcher event for one of `paths`
    /// (any source file if the open itself panicked), or a lint of other
    /// text than `call`'s.
    Failed { message: String, paths: HashSet<PathBuf>, call: Option<(PathBuf, u64)>, at: Instant, changed: bool },
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
    state: State,
    /// The polled files, as of the last open attempt.
    stamps: Stamps,
    /// Linted files the walk wouldn't include, until the next re-open.
    rejected: HashSet<PathBuf>,
    /// One-time notices for the user (the watcher failed or stopped).
    warnings: Vec<String>,
    retry_floor: Duration,
    /// Frees replaced analyses and projects off the linting thread.
    dropper: Dropper,
    pub stats: Stats,
    /// Changes a test hands to the next drain, as if the watcher saw them.
    #[cfg(test)]
    injected: Option<Changes>,
}

impl Live {
    /// Opens the project like `detangle check <dir> [--config] [--mode]`,
    /// never failing: a project that can't be opened is `Broken` (or
    /// `Failed`) until a polled file changes. With `watch`, a recursive
    /// watcher on the root is started first, on its own thread, so setting
    /// it up overlaps the scan; it stays for the handle's lifetime,
    /// whatever the state.
    pub fn open(dir: &Path, config: Option<&Path>, mode: Option<&str>, watch: bool) -> Live {
        let root = dunce::canonicalize(dir).ok().map(|d| config::find_root(&d));
        let watch = match root {
            Some(root) if watch => Watch::Starting(std::thread::spawn(move || Watcher::new(&root))),
            _ => Watch::Off,
        };
        let mut live = Live {
            dir: dir.to_path_buf(),
            config: config.map(Path::to_path_buf),
            mode: mode.map(String::from),
            watch,
            state: State::Broken(String::new()),
            stamps: Stamps::collect(dir, config, None),
            rejected: HashSet::new(),
            warnings: Vec::new(),
            retry_floor: Duration::from_secs(30),
            dropper: Dropper::new(),
            stats: Stats::default(),
            #[cfg(test)]
            injected: None,
        };
        live.attempt_open();
        live
    }

    /// How often a `Failed` project may retry (tests shorten it).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn set_retry_floor(&mut self, floor: Duration) {
        self.retry_floor = floor;
    }

    /// Notices collected since the last call, each given once.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Whether the watcher is up (false while it's still being set up).
    pub fn watching(&self) -> bool {
        matches!(self.watch, Watch::Running(_))
    }

    /// The state's name: "ready", "stale", "broken" or "failed".
    pub fn state(&self) -> &'static str {
        match self.state {
            State::Ready(_) => "ready",
            State::Stale(..) => "stale",
            State::Broken(_) => "broken",
            State::Failed { .. } => "failed",
        }
    }

    fn loaded(&mut self) -> Option<&mut Loaded> {
        match &mut self.state {
            State::Ready(l) | State::Stale(l, _) => Some(l),
            _ => None,
        }
    }

    /// What to show in `file` (absolute, canonical), whose editor buffer
    /// holds `text`. Never panics.
    pub fn violations_for(&mut self, file: &Path, text: &str) -> Report {
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = rustc_hash::FxHasher::default();
            text.hash(&mut h);
            h.finish()
        };
        let result = catch_unwind(AssertUnwindSafe(|| self.lint(file, text, hash)));
        let mut report = match result {
            Ok(Ok(violations)) => Report { violations, ..Default::default() },
            // An error (not a panic), e.g. a directory that can't be read:
            // shown, and the next lint tries again.
            Ok(Err(e)) => Report { problems: vec![format!("detangle: {e:#}")], ..Default::default() },
            Err(payload) => {
                self.fail(panic_message(&*payload), Some((file.to_path_buf(), hash)));
                Report::default()
            }
        };
        report.problems.extend(self.problems());
        if let Some(l) = self.loaded() {
            report.exotic_require = l.project.config().options.exotic_require.clone();
        }
        report
    }

    /// Problems to show with every result while the state lasts.
    fn problems(&self) -> Vec<String> {
        match &self.state {
            State::Ready(_) => vec![],
            State::Stale(_, e) | State::Broken(e) => vec![format!("detangle: {e}")],
            State::Failed { message, .. } => vec![format!(
                "detangle: internal error ({message}); it retries after the next change. \
                 Please report it at https://github.com/debug-diary-1/detangle/issues"
            )],
        }
    }

    fn lint(&mut self, file: &Path, text: &str, hash: u64) -> Result<Vec<FileViolation>> {
        let changes = self.drain();
        let polled = self.stamps.check();
        let mut dirty = false;
        match &mut self.state {
            State::Broken(_) => {
                if polled.is_none() {
                    return Ok(Vec::new());
                }
                dirty = self.attempt_open();
            }
            State::Failed { paths, call, at, changed, .. } => {
                let dir = &self.dir;
                let touched = changes.as_ref().is_some_and(|c| {
                    c.rescan
                        || c.paths.iter().any(|p| if paths.is_empty() { p.starts_with(dir) && has_source_ext(p) } else { paths.contains(p) })
                });
                *changed |= polled.is_some() || touched || call.as_ref().is_some_and(|(f, h)| f != file || *h != hash);
                if !*changed || at.elapsed() < self.retry_floor {
                    return Ok(Vec::new());
                }
                dirty = self.attempt_open();
            }
            State::Ready(_) | State::Stale(..) => {
                let mut reopen = polled == Some(Kind::Config);
                if !reopen && let Some(c) = &changes {
                    let (d, r) = self.apply(c)?;
                    dirty |= d;
                    reopen |= r;
                }
                if reopen {
                    dirty |= self.attempt_open();
                } else if polled == Some(Kind::Lockfile) {
                    let l = self.loaded().expect("ready or stale");
                    l.project.session_mut().refresh_resolution()?;
                    dirty |= l.project.session_mut().work.graph_changed;
                    self.stamps = l.project.config_stamps();
                    self.stats.refreshes += 1;
                }
            }
        }
        #[cfg(any(test, feature = "test-hooks"))]
        if text.contains(TEST_PANIC) {
            panic!("test panic");
        }
        if self.loaded().is_none() {
            return Ok(Vec::new());
        }
        match self.member(file)? {
            Member::No => {}
            Member::Yes => dirty |= self.loaded().expect("loaded").project.session_mut().overlay(file, text),
            Member::Admitted => {
                self.loaded().expect("loaded").project.session_mut().overlay(file, text);
                dirty = true;
            }
        }
        let l = self.loaded().expect("loaded");
        if dirty {
            let old = std::mem::replace(&mut l.analysis, l.project.analyze()?);
            self.dropper.drop_later(old);
            self.stats.analyses += 1;
        }
        let l = self.loaded().expect("loaded");
        Ok(l.analysis.violations_for(file))
    }

    /// Opens the project again, recording the polled stamps whatever
    /// happens, and moves to the resulting state (D8). Returns whether a
    /// new project was loaded (its analysis is fresh).
    fn attempt_open(&mut self) -> bool {
        self.stats.open_attempts += 1;
        let (dir, config, mode) = (&self.dir, self.config.as_deref(), self.mode.as_deref());
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<Loaded> {
            if let Ok(d) = dunce::canonicalize(dir)
                && let Some(why) = refusal(&config::find_root(&d), std::env::home_dir().as_deref())
            {
                anyhow::bail!(why);
            }
            let project = Project::open(dir, config, mode, &CacheArgs::default())?;
            let analysis = project.analyze()?;
            Ok(Loaded { project, analysis })
        }));
        self.stamps = match &result {
            Ok(Ok(l)) => l.project.config_stamps(),
            _ => {
                let root = dunce::canonicalize(&self.dir).map(|d| config::find_root(&d)).unwrap_or_else(|_| self.dir.clone());
                Stamps::collect(&root, self.config.as_deref(), None)
            }
        };
        let previous = std::mem::replace(&mut self.state, State::Broken(String::new()));
        let had = match previous {
            State::Ready(l) | State::Stale(l, _) => Some(l),
            _ => None,
        };
        match result {
            Ok(Ok(loaded)) => {
                if let Some(old) = had {
                    self.dropper.drop_later(old);
                }
                self.state = State::Ready(Box::new(loaded));
                self.rejected.clear();
                self.stats.opens += 1;
                self.stats.analyses += 1;
                return true;
            }
            Ok(Err(e)) => {
                let e = format!("{e:#}");
                self.state = match had {
                    // A config error keeps the last good project; a missing
                    // directory leaves nothing to keep.
                    Some(l) if self.dir.is_dir() => State::Stale(l, e),
                    _ => State::Broken(e),
                };
            }
            Err(payload) => {
                let paths = had.map(|l| l.project.session_paths()).unwrap_or_default();
                self.state = State::Failed { message: panic_message(&*payload), paths, call: None, at: Instant::now(), changed: false };
            }
        }
        false
    }

    /// Moves to `Failed` after a panic during a lint.
    fn fail(&mut self, message: String, call: Option<(PathBuf, u64)>) {
        let paths = match &mut self.state {
            State::Ready(l) | State::Stale(l, _) => l.project.session_paths(),
            State::Failed { paths, .. } => std::mem::take(paths),
            State::Broken(_) => HashSet::new(),
        };
        self.state = State::Failed { message, paths, call, at: Instant::now(), changed: false };
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

    /// Applies watcher changes to the loaded project. Returns whether the
    /// graph changed, and whether the project must be re-opened: lost
    /// events, or a config file in a directory with scanned files (checked
    /// after the update, so new sources count; build output such as
    /// .next/package.json configures nothing detangle scans).
    fn apply(&mut self, changes: &Changes) -> Result<(bool, bool)> {
        if changes.rescan {
            // Events were lost, so `paths` may be incomplete.
            return Ok((false, true));
        }
        let is_config = |p: &Path| p.file_name().and_then(|n| n.to_str()).is_some_and(watch::is_config);
        let sources: Vec<PathBuf> = changes.paths.iter().filter(|p| !is_config(p)).cloned().collect();
        let session = self.loaded().expect("ready or stale").project.session_mut();
        let mut dirty = false;
        if !sources.is_empty() {
            session.update(&sources)?;
            dirty = session.work.graph_changed;
        }
        let reopen = changes.paths.iter().any(|p| is_config(p) && p.parent().is_some_and(|d| session.is_member_dir(d)));
        Ok((dirty, reopen))
    }

    /// Whether `file` is scanned, admitting it if the walk would include it.
    fn member(&mut self, file: &Path) -> Result<Member> {
        let rejected = self.rejected.contains(file);
        let session = self.loaded().expect("loaded").project.session_mut();
        if session.contains(file) {
            return Ok(Member::Yes);
        }
        // Untitled buffers, processor-virtual paths (README.md/0.js), deleted files.
        if !has_source_ext(file) || !file.is_file() || rejected {
            return Ok(Member::No);
        }
        self.stats.eligibility_checks += 1;
        let session = self.loaded().expect("loaded").project.session_mut();
        if !session.eligible(file) {
            self.rejected.insert(file.to_path_buf());
            return Ok(Member::No);
        }
        session.admit(file)?;
        Ok(if session.contains(file) { Member::Admitted } else { Member::No })
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // Don't leave a watcher being set up behind (at most its setup
        // time); the dropper joins its thread when it's dropped next.
        if let Watch::Starting(h) = std::mem::replace(&mut self.watch, Watch::Off) {
            let _ = h.join();
        }
    }
}

/// Why a project rooted at `root` is refused: the filesystem root, or the
/// home directory or one of its ancestors (reached through a stray
/// package.json, say), would be a scan of everything the user has.
fn refusal(root: &Path, home: Option<&Path>) -> Option<String> {
    let home = home.map(|h| dunce::canonicalize(h).unwrap_or_else(|_| h.to_path_buf()));
    let refused = root.parent().is_none() || home.is_some_and(|h| h.starts_with(root));
    refused.then(|| format!("refusing to scan {}; set the \"dir\" option", root.display()))
}

/// Frees values on its own thread: a replaced analysis or project can take
/// tens of ms to free (VS Code's graph: ~20 ms), which a lint shouldn't wait
/// for. Dropping the Dropper joins its thread after the queue is empty.
struct Dropper {
    tx: Option<std::sync::mpsc::Sender<Box<dyn Send>>>,
    thread: Option<JoinHandle<()>>,
}

impl Dropper {
    fn new() -> Dropper {
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn Send>>();
        match std::thread::Builder::new().name("detangle-dropper".into()).spawn(move || rx.into_iter().for_each(drop)) {
            Ok(t) => Dropper { tx: Some(tx), thread: Some(t) },
            Err(_) => Dropper { tx: None, thread: None },
        }
    }

    /// Frees `v` on the dropper thread, or here if there isn't one.
    fn drop_later<T: Send + 'static>(&self, v: T) {
        if let Some(tx) = &self.tx {
            // On failure the value comes back in the error and is dropped here.
            let _ = tx.send(Box::new(v));
        }
    }
}

impl Drop for Dropper {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".into())
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

    /// Opened without a watcher: changes come from polling, or from `injected`.
    fn open(t: &Tmp) -> Live {
        let l = Live::open(&t.0, None, None, false);
        assert_eq!(l.state(), "ready");
        l
    }

    fn rules(l: &mut Live, t: &Tmp, file: &str, text: &str) -> Vec<String> {
        let r = l.violations_for(&t.path(file), text);
        let mut v: Vec<String> = r.violations.into_iter().map(|v| v.rule).collect();
        v.extend(r.problems.into_iter().map(|p| format!("problem: {p}")));
        v.sort();
        v
    }

    fn changes(paths: &[PathBuf]) -> Changes {
        Changes { paths: paths.iter().cloned().collect(), ..Default::default() }
    }

    /// Lints src/a.ts with the watcher change `c`; returns the re-opens it caused.
    fn reopens_after(l: &mut Live, t: &Tmp, c: Changes) -> usize {
        let before = l.stats.opens;
        l.injected = Some(c);
        rules(l, t, "src/a.ts", "import './b';");
        l.stats.opens - before
    }

    #[test]
    fn unsaved_buffers_are_checked_live() {
        let t = project("buffer");
        let mut l = open(&t);
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
    fn config_events_count_only_where_there_are_sources() {
        let t = project("config-events");
        t.write("packages/p/package.json", "{}");
        t.write("packages/p/src/index.ts", "");
        t.write(".next/package.json", "{}");
        let mut l = open(&t);
        // Build output: no re-open.
        assert_eq!(reopens_after(&mut l, &t, changes(&[t.path(".next/package.json")])), 0);
        // A nested workspace package.json, next to scanned sources: re-open.
        assert_eq!(reopens_after(&mut l, &t, changes(&[t.path("packages/p/package.json")])), 1);
        // Lost events.
        assert_eq!(reopens_after(&mut l, &t, Changes { rescan: true, ..Default::default() }), 1);
    }

    #[test]
    fn lost_events_find_files_no_event_reported() {
        let t = project("rescan");
        let mut l = open(&t);
        t.write("src/c.ts", "import './legacy/old';");
        assert_eq!(reopens_after(&mut l, &t, Changes { rescan: true, ..Default::default() }), 1);
        assert_eq!(rules(&mut l, &t, "src/c.ts", "import './legacy/old';"), ["no-legacy"]);
        assert_eq!(l.stats.eligibility_checks, 0, "found by the re-open, not admitted");
    }

    #[test]
    fn config_edits_are_polled() {
        let t = project("poll-config");
        let mut l = open(&t);
        assert!(rules(&mut l, &t, "src/a.ts", "import './b';").is_empty());
        // No watcher: the next lint's poll sees the edit.
        t.write("detangle.toml", &CONFIG.replace("legacy/", "b"));
        assert_eq!(rules(&mut l, &t, "src/a.ts", "import './b';"), ["no-legacy"]);
        assert_eq!((l.stats.opens, l.stats.open_attempts), (2, 2));
        // Nothing changed since: no more opens.
        rules(&mut l, &t, "src/a.ts", "import './b';");
        assert_eq!(l.stats.open_attempts, 2);
    }

    #[test]
    fn a_config_outside_the_root_is_polled() {
        let t = project("poll-outside");
        t.write("../detangle-live-rules-outside.toml", CONFIG);
        let config = t.0.parent().unwrap().join("detangle-live-rules-outside.toml");
        let mut l = Live::open(&t.0, Some(&config), None, false);
        assert_eq!(rules(&mut l, &t, "src/a.ts", "import './legacy/old';"), ["no-legacy"]);
        std::fs::write(&config, "").unwrap();
        assert!(rules(&mut l, &t, "src/a.ts", "import './legacy/old';").is_empty());
        let _ = std::fs::remove_file(&config);
    }

    #[test]
    fn a_failed_reopen_goes_stale_and_waits_for_the_next_change() {
        let t = project("stale");
        let mut l = open(&t);
        t.write("detangle.toml", "[[forbidden]\n");
        let r = rules(&mut l, &t, "src/a.ts", "import './legacy/old';");
        // The last good results, and the error.
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[0], "no-legacy");
        assert!(r[1].starts_with("problem: detangle: parsing"), "{r:?}");
        assert_eq!(l.state(), "stale");
        // Not retried on every lint.
        for _ in 0..3 {
            rules(&mut l, &t, "src/a.ts", "import './legacy/old';");
        }
        assert_eq!(l.stats.open_attempts, 2);
        t.write("detangle.toml", CONFIG);
        assert_eq!(rules(&mut l, &t, "src/a.ts", "import './legacy/old';"), ["no-legacy"]);
        assert_eq!((l.state(), l.stats.open_attempts), ("ready", 3));
    }

    #[test]
    fn broken_recovers_through_polling() {
        let t = project("broken");
        t.write("detangle.toml", "[[forbidden]\n");
        let mut l = Live::open(&t.0, None, None, false);
        assert_eq!(l.state(), "broken");
        let r = rules(&mut l, &t, "src/a.ts", "import './legacy/old';");
        assert!(r.len() == 1 && r[0].starts_with("problem: detangle: parsing"), "{r:?}");
        rules(&mut l, &t, "src/a.ts", "import './legacy/old';");
        assert_eq!(l.stats.open_attempts, 1);
        t.write("detangle.toml", CONFIG);
        assert_eq!(rules(&mut l, &t, "src/a.ts", "import './legacy/old';"), ["no-legacy"]);
        assert_eq!(l.state(), "ready");
    }

    #[test]
    fn a_missing_dir_is_broken_until_it_returns() {
        let t = project("dir-gone");
        let dir = t.path("app");
        std::fs::rename(t.path("src"), t.path("src-away")).unwrap();
        t.write("app/detangle.toml", CONFIG);
        t.write("app/src/x.ts", "import './legacy/old';");
        t.write("app/src/legacy/old.ts", "");
        let x = dir.join("src/x.ts");
        let mut l = Live::open(&dir, None, None, false);
        assert_eq!(l.violations_for(&x, "import './legacy/old';").violations.len(), 1);
        // A branch switch removes it.
        std::fs::rename(&dir, t.path("app-away")).unwrap();
        l.injected = Some(Changes { rescan: true, ..Default::default() });
        let r = l.violations_for(&x, "import './legacy/old';");
        assert_eq!(l.state(), "broken");
        assert!(r.violations.is_empty() && r.problems[0].contains("not found"), "{r:?}");
        std::fs::rename(t.path("app-away"), &dir).unwrap();
        assert_eq!(l.violations_for(&x, "import './legacy/old';").violations.len(), 1);
        assert_eq!(l.state(), "ready");
    }

    #[test]
    fn a_lockfile_change_re_resolves_without_reopening() {
        let t = project("lockfile");
        t.write("package-lock.json", "{}");
        let mut l = open(&t);
        let text = "import 'pkg';";
        t.write("src/a.ts", text);
        let r = l.violations_for(&t.path("src/a.ts"), text);
        assert!(r.violations.is_empty());
        // `npm install pkg`: the package appears and the lockfile changes.
        t.write("node_modules/pkg/index.js", "");
        t.write("package-lock.json", r#"{"lockfileVersion":3}"#);
        l.violations_for(&t.path("src/a.ts"), text);
        assert_eq!((l.stats.refreshes, l.stats.opens), (1, 1));
        let targets = &l.loaded().unwrap().analysis.graph;
        assert!(targets.find(crate::graph::ModuleKind::Npm, "pkg").is_some());
        // Once.
        l.violations_for(&t.path("src/a.ts"), text);
        assert_eq!(l.stats.refreshes, 1);
    }

    #[test]
    fn a_panic_fails_and_retries_only_after_a_change_and_the_floor() {
        let t = project("panic");
        let mut l = open(&t);
        l.set_retry_floor(Duration::from_millis(300));
        let crash = format!("import './legacy/old'; // {TEST_PANIC}");
        let r = rules(&mut l, &t, "src/a.ts", &crash);
        assert_eq!(l.state(), "failed");
        assert!(r.len() == 1 && r[0].contains("internal error (test panic)"), "{r:?}");
        // The same text: nothing changed, so no retry, even after the floor.
        std::thread::sleep(Duration::from_millis(350));
        rules(&mut l, &t, "src/a.ts", &crash);
        assert_eq!((l.state(), l.stats.open_attempts), ("failed", 1));
        // Fixed text is a change, and the floor since the panic has passed: retried.
        let fixed = "import './legacy/old';";
        assert_eq!(rules(&mut l, &t, "src/a.ts", fixed), ["no-legacy"]);
        assert_eq!((l.state(), l.stats.open_attempts), ("ready", 2));
        // Crash again, then fix straight away: within the floor, so not yet.
        rules(&mut l, &t, "src/a.ts", &crash);
        let r = rules(&mut l, &t, "src/a.ts", fixed);
        assert!(r[0].contains("internal error"), "{r:?}");
        assert_eq!(l.stats.open_attempts, 2);
        std::thread::sleep(Duration::from_millis(350));
        assert_eq!(rules(&mut l, &t, "src/a.ts", fixed), ["no-legacy"]);
        assert_eq!(l.stats.open_attempts, 3);
    }

    #[test]
    fn new_files_are_admitted_and_excluded_ones_remembered() {
        let t = project("membership");
        let mut l = open(&t);
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
        let mut l = open(&t);
        // A saved change elsewhere (via the watcher), a new file and its
        // buffer, all in one lint.
        t.write("src/b.ts", "import './legacy/old';");
        l.injected = Some(changes(&[t.path("src/b.ts")]));
        t.write("src/new.ts", "");
        assert_eq!(rules(&mut l, &t, "src/new.ts", "import './legacy/old';"), ["no-legacy"]);
        assert_eq!(rules(&mut l, &t, "src/b.ts", "import './legacy/old';"), ["no-legacy"]);
        assert_eq!(l.stats.analyses, 2);
    }

    #[test]
    fn refuses_the_filesystem_root_and_home() {
        let home = std::env::temp_dir().join("detangle-refusal-home");
        std::fs::create_dir_all(home.join("projects/app")).unwrap();
        let home = dunce::canonicalize(&home).unwrap();
        let root = Path::new(if cfg!(windows) { "C:\\" } else { "/" });
        assert!(refusal(root, Some(&home)).unwrap().starts_with("refusing to scan"));
        assert!(refusal(&home, Some(&home)).is_some());
        assert!(refusal(home.parent().unwrap(), Some(&home)).is_some());
        assert_eq!(refusal(&home.join("projects/app"), Some(&home)), None);
        assert_eq!(refusal(&home.join("projects/app"), None), None);
    }

    #[test]
    fn replaced_analyses_are_freed_elsewhere() {
        let t = project("dropper");
        let mut l = open(&t);
        let main = std::thread::current().id();
        rules(&mut l, &t, "src/a.ts", "import './legacy/old';");
        // The dropper thread is alive and isn't this one.
        let dropper = l.dropper.thread.as_ref().unwrap().thread().id();
        assert_ne!(dropper, main);
        drop(l);
    }

    /// With the real watcher: a saved change to another file is seen at the
    /// next lint. (FSEvents needs the Bash sandbox off on macOS.)
    #[test]
    fn saved_changes_arrive_through_the_watcher() {
        let t = project("watcher");
        let mut l = Live::open(&t.0, None, None, true);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !l.watching() {
            l.violations_for(&t.path("src/a.ts"), "import './b';");
            assert!(Instant::now() < deadline, "watcher never started: {:?}", l.take_warnings());
            std::thread::sleep(Duration::from_millis(20));
        }
        // FSEvents may deliver events from before the watch started; let them pass.
        std::thread::sleep(Duration::from_millis(200));
        l.violations_for(&t.path("src/a.ts"), "import './b';");
        let analyses = l.stats.analyses;
        t.write("src/b.ts", "import './legacy/old';");
        loop {
            // Lint a, not b: b's change must come from the watcher.
            l.violations_for(&t.path("src/a.ts"), "import './b';");
            if l.stats.analyses > analyses {
                break;
            }
            assert!(Instant::now() < deadline + Duration::from_secs(5), "change never arrived");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(rules(&mut l, &t, "src/b.ts", "import './legacy/old';"), ["no-legacy"]);
    }
}
