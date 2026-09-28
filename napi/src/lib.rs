//! The Node.js add-on behind detangle's ESLint rules (`npm/eslint.js`).
//!
//! `open(dir, options)` is `detangle check <dir> [--config …] [--mode …]`
//! kept alive: it returns a handle whose `violationsFor(file, text)` says
//! what to show in one linted file. JavaScript only matches the returned
//! import strings to AST nodes.

use std::path::{Path, PathBuf};

use detangle::live::Live;
use napi_derive::napi;

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

enum State {
    Ready(Box<Live>),
    /// The project couldn't be opened.
    Broken(String),
}

/// An open project (a handle, in the design's terms): the scan, kept
/// current by a file watcher and by the buffers it's asked about.
#[napi]
pub struct Project {
    state: State,
}

/// Opens the project at `dir` (absolute), scanning it once and starting a
/// watcher on its root.
#[napi]
pub fn open(dir: String, options: Option<OpenOptions>) -> Project {
    let options = options.unwrap_or(OpenOptions { config: None, mode: None });
    let config = options.config.map(PathBuf::from);
    Project {
        state: match Live::open(Path::new(&dir), config.as_deref(), options.mode.as_deref(), true) {
            Ok(live) => State::Ready(Box::new(live)),
            Err(e) => State::Broken(format!("{e:#}")),
        },
    }
}

#[napi]
impl Project {
    /// What to show in `file` (absolute), whose editor buffer holds `text`.
    #[napi]
    pub fn violations_for(&mut self, file: String, text: String) -> FileResult {
        let live = match &mut self.state {
            State::Ready(live) => live,
            State::Broken(e) => return FileResult { violations: Vec::new(), problems: vec![format!("detangle: {e}")], exotic_require: Vec::new() },
        };
        let (violations, mut problems) = match live.violations_for(&canonical(Path::new(&file)), &text) {
            Ok(vs) => (vs, Vec::new()),
            Err(e) => (Vec::new(), vec![format!("detangle: {e:#}")]),
        };
        problems.extend(live.problems().into_iter().map(|p| format!("detangle: {p}")));
        FileResult {
            violations: violations
                .into_iter()
                .map(|v| Violation { rule: v.rule, severity: severity(v.severity).into(), message: v.message, specifiers: v.specifiers })
                .collect(),
            problems,
            exotic_require: live.project().config().options.exotic_require.clone(),
        }
    }

    /// One-time notices since the last call (the file watcher failed or
    /// stopped), for `process.emitWarning`.
    #[napi]
    pub fn take_warnings(&mut self) -> Vec<String> {
        match &mut self.state {
            State::Ready(live) => live.take_warnings(),
            State::Broken(_) => Vec::new(),
        }
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
