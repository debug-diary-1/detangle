//! The Node.js add-on behind detangle's ESLint rules (`npm/eslint.js`).
//!
//! `open(dir, options)` is `detangle check <dir> [--config …] [--mode …]`
//! kept alive: it returns a handle whose `violationsFor(file, text)` says
//! what to show in one linted file. JavaScript only matches the returned
//! import strings to AST nodes. The handle's logic is `detangle::live::Live`.
//!
//! Every export catches panics (`Live` already does; `catch_unwind` here is
//! the backstop), so ESLint never sees a throw. That needs `panic = "unwind"`,
//! the default, in every profile.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use detangle::live::Live;
use napi::Env;
use napi_derive::napi;

// As in the CLI: parsing and graph building allocate heavily from many
// threads, where mimalloc is much faster than the system allocator
// (notably on macOS). It serves only Rust's allocations, not Node's.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[napi(object)]
pub struct OpenOptions {
    /// Config file (like `--config`), absolute.
    pub config: Option<String>,
    /// Mode for evaluating Vite / webpack configs (like `--mode`).
    pub mode: Option<String>,
    /// The Node.js executable for evaluating JavaScript configs
    /// (`process.execPath`); `node` from PATH if unset.
    pub node: Option<String>,
}

#[napi(object)]
pub struct Violation {
    pub rule: String,
    /// "error", "warn" or "info".
    pub severity: String,
    pub message: String,
    /// The import strings in the file to report on; empty means line 1.
    pub specifiers: Vec<String>,
}

#[napi(object)]
pub struct FileResult {
    pub violations: Vec<Violation>,
    /// Problems with the project itself (e.g. a config error), shown at line 1.
    pub problems: Vec<String>,
    /// The config's `exotic_require` names, for matching call expressions.
    pub exotic_require: Vec<String>,
}

/// An open project (a handle, in the design's terms): the scan, kept
/// current by polling, a file watcher and the buffers it's asked about.
/// It never throws: problems come back in `problems` (see `Live`).
#[napi]
pub struct Project {
    /// `None` once closed. Shared with this thread's registry, so the env
    /// cleanup hook can reach it.
    live: Shared,
}

type Shared = Rc<RefCell<Option<Live>>>;

thread_local! {
    /// This thread's open handles. Each env (the main thread, or a worker)
    /// runs on its own thread, so this is the env's registry.
    static HANDLES: RefCell<Vec<Weak<RefCell<Option<Live>>>>> = const { RefCell::new(Vec::new()) };
    static REGISTERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Registers this env's cleanup, once; `mainThread` is worker_threads'
/// `isMainThread`. When the env is torn down:
/// - a worker's handles are freed, which stops their watchers and joins
///   the watcher-setup and dropper threads;
/// - the main env's are leaked, like the CLI's one-shot commands: the
///   process is exiting, the OS reclaims everything at once, and `eslint .`
///   shouldn't wait for a large graph to be freed or a watcher to finish
///   setting up. Their threads touch only state they own.
#[napi(catch_unwind)]
pub fn init(env: Env, main_thread: bool) -> napi::Result<()> {
    if REGISTERED.with(|r| r.replace(true)) {
        return Ok(());
    }
    env.add_env_cleanup_hook(main_thread, |main| {
        let handles: Vec<_> = HANDLES.with(|h| h.borrow_mut().drain(..).collect());
        for h in handles.iter().filter_map(Weak::upgrade) {
            let live = h.borrow_mut().take();
            if main {
                std::mem::forget(live);
            }
        }
    })?;
    Ok(())
}

/// `path` with symlinks resolved and, on Windows, in its on-disk case: the
/// form every path and project key is compared in. Unchanged if it
/// doesn't exist.
#[napi(js_name = "canonical", catch_unwind)]
pub fn canonical_js(path: String) -> String {
    canonical(Path::new(&path)).to_string_lossy().into_owned()
}

/// Opens the project at `dir` (absolute), scanning it once and starting a
/// watcher on its root. A project that can't be opened is returned anyway,
/// reporting why until a config file changes.
#[napi(catch_unwind)]
pub fn open(dir: String, options: Option<OpenOptions>) -> Project {
    let options = options.unwrap_or(OpenOptions { config: None, mode: None, node: None });
    if let Some(node) = options.node {
        detangle::set_node(Some(PathBuf::from(node)));
    }
    let config = options.config.map(PathBuf::from);
    let live = Live::open(Path::new(&dir), config.as_deref(), options.mode.as_deref(), hooks::watch());
    let live = Rc::new(RefCell::new(Some(live)));
    HANDLES.with(|h| h.borrow_mut().push(Rc::downgrade(&live)));
    Project { live }
}

#[napi]
impl Project {
    /// What to show in `file` (absolute), whose editor buffer holds `text`.
    #[napi(catch_unwind)]
    pub fn violations_for(&mut self, file: String, text: String) -> FileResult {
        let mut live = self.live.borrow_mut();
        let Some(live) = live.as_mut() else {
            return FileResult { violations: vec![], problems: vec!["detangle: project closed".into()], exotic_require: vec![] };
        };
        let r = live.violations_for(&canonical(Path::new(&file)), &text);
        FileResult {
            violations: r
                .violations
                .into_iter()
                .map(|v| Violation { rule: v.rule, severity: severity(v.severity).into(), message: v.message, specifiers: v.specifiers })
                .collect(),
            problems: r.problems,
            exotic_require: r.exotic_require,
        }
    }

    /// One-time notices since the last call (the file watcher failed or
    /// stopped), for `process.emitWarning`.
    #[napi(catch_unwind)]
    pub fn take_warnings(&mut self) -> Vec<String> {
        self.live.borrow_mut().as_mut().map(Live::take_warnings).unwrap_or_default()
    }

    /// Stops the watcher and frees the project. Every later call returns
    /// only the problem "detangle project closed". For idle eviction.
    #[napi(catch_unwind)]
    pub fn close(&mut self) {
        let live = self.live.borrow_mut().take();
        drop(live);
    }
}

/// Test hooks.
#[cfg(feature = "test-hooks")]
#[napi]
impl Project {
    /// Test hook: the state and step counts.
    #[napi(js_name = "__stats")]
    pub fn stats(&self) -> hooks::Stats {
        let live = self.live.borrow();
        let Some(live) = live.as_ref() else {
            return hooks::Stats { state: "closed".into(), watching: false, opens: 0, open_attempts: 0, analyses: 0, refreshes: 0 };
        };
        let s = live.stats;
        hooks::Stats {
            state: live.state().into(),
            watching: live.watching(),
            opens: s.opens as u32,
            open_attempts: s.open_attempts as u32,
            analyses: s.analyses as u32,
            refreshes: s.refreshes as u32,
        }
    }

    /// Test hook: how long a failed project waits before retrying.
    #[napi(js_name = "__setRetryFloorMs")]
    pub fn set_retry_floor_ms(&mut self, ms: u32) {
        if let Some(live) = self.live.borrow_mut().as_mut() {
            live.set_retry_floor(std::time::Duration::from_millis(ms.into()));
        }
    }
}

/// Hidden switches for the recovery tests, compiled in only with the
/// `test-hooks` feature.
mod hooks {
    #[cfg(feature = "test-hooks")]
    static WATCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

    /// Whether new projects start a file watcher.
    pub fn watch() -> bool {
        #[cfg(feature = "test-hooks")]
        return WATCH.load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(feature = "test-hooks"))]
        true
    }

    #[cfg(feature = "test-hooks")]
    #[napi_derive::napi(js_name = "__setWatcherEnabled")]
    pub fn set_watcher_enabled(enabled: bool) {
        WATCH.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(feature = "test-hooks")]
    #[napi_derive::napi(object)]
    pub struct Stats {
        pub state: String,
        /// The file watcher is set up (it starts on its own thread).
        pub watching: bool,
        pub opens: u32,
        pub open_attempts: u32,
        pub analyses: u32,
        pub refreshes: u32,
    }
}

/// `path` with symlinks resolved (and on Windows, the on-disk case), as the
/// project's own paths are; unchanged if it doesn't exist.
fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn severity(s: detangle::config::Severity) -> &'static str {
    use detangle::config::Severity::*;
    match s {
        Error => "error",
        Warn => "warn",
        Info => "info",
        Off => "off",
    }
}
