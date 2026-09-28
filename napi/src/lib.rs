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

use std::path::{Path, PathBuf};

use detangle::live::Live;
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
    live: Live,
}

/// Opens the project at `dir` (absolute), scanning it once and starting a
/// watcher on its root. A project that can't be opened is returned anyway,
/// reporting why until a config file changes.
#[napi(catch_unwind)]
pub fn open(dir: String, options: Option<OpenOptions>) -> Project {
    let options = options.unwrap_or(OpenOptions { config: None, mode: None });
    let config = options.config.map(PathBuf::from);
    Project { live: Live::open(Path::new(&dir), config.as_deref(), options.mode.as_deref(), hooks::watch()) }
}

#[napi]
impl Project {
    /// What to show in `file` (absolute), whose editor buffer holds `text`.
    #[napi(catch_unwind)]
    pub fn violations_for(&mut self, file: String, text: String) -> FileResult {
        let r = self.live.violations_for(&canonical(Path::new(&file)), &text);
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
        self.live.take_warnings()
    }
}

/// Test hooks.
#[cfg(feature = "test-hooks")]
#[napi]
impl Project {
    /// Test hook: the state and step counts.
    #[napi(js_name = "__stats")]
    pub fn stats(&self) -> hooks::Stats {
        let s = self.live.stats;
        hooks::Stats {
            state: self.live.state().into(),
            opens: s.opens as u32,
            open_attempts: s.open_attempts as u32,
            analyses: s.analyses as u32,
            refreshes: s.refreshes as u32,
        }
    }

    /// Test hook: how long a failed project waits before retrying.
    #[napi(js_name = "__setRetryFloorMs")]
    pub fn set_retry_floor_ms(&mut self, ms: u32) {
        self.live.set_retry_floor(std::time::Duration::from_millis(ms.into()));
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
