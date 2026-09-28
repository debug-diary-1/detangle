//! The Node.js add-on behind detangle's ESLint rules (`npm/eslint.js`).
//!
//! `open(dir, options)` is `detangle check <dir> [--config …] [--mode …]`
//! kept alive: it returns a handle whose `violationsFor(file, text)` says
//! what to show in one linted file. JavaScript only matches the returned
//! import strings to AST nodes.

use std::path::{Path, PathBuf};

use detangle::{Analysis, CacheArgs};
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
    Ready { analysis: Box<Analysis>, exotic_require: Vec<String> },
    /// The project couldn't be opened.
    Broken(String),
}

/// An open project (a handle, in the design's terms).
#[napi]
pub struct Project {
    state: State,
}

/// Opens the project at `dir` (absolute), scanning it once.
#[napi]
pub fn open(dir: String, options: Option<OpenOptions>) -> Project {
    let options = options.unwrap_or(OpenOptions { config: None, mode: None });
    let config = options.config.map(PathBuf::from);
    let opened = detangle::Project::open(Path::new(&dir), config.as_deref(), options.mode.as_deref(), &CacheArgs::default())
        .and_then(|p| Ok((Box::new(p.analyze()?), p.config().options.exotic_require.clone())));
    Project {
        state: match opened {
            Ok((analysis, exotic_require)) => State::Ready { analysis, exotic_require },
            Err(e) => State::Broken(format!("{e:#}")),
        },
    }
}

#[napi]
impl Project {
    /// What to show in `file` (absolute), whose editor buffer holds `text`.
    #[napi]
    pub fn violations_for(&self, file: String, _text: String) -> FileResult {
        match &self.state {
            State::Ready { analysis, exotic_require } => FileResult {
                violations: analysis
                    .violations_for(Path::new(&file))
                    .into_iter()
                    .map(|v| Violation {
                        rule: v.rule,
                        severity: severity(v.severity).into(),
                        message: v.message,
                        specifiers: v.specifiers,
                    })
                    .collect(),
                problems: Vec::new(),
                exotic_require: exotic_require.clone(),
            },
            State::Broken(e) => FileResult { violations: Vec::new(), problems: vec![format!("detangle: {e}")], exotic_require: Vec::new() },
        }
    }
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
