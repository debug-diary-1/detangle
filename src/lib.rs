//! The analysis behind the `detangle` command-line tool.
//!
//! This is an internal API, with no semver guarantees: it exists so the
//! detangle Node.js add-on can share the CLI's code, and may change in any
//! release.

pub mod aliases;
pub mod config;
pub mod dotenv;
pub mod graph;
pub mod groups;
pub mod live;
pub mod migrate;
mod project;
pub mod report;
pub mod rules;
pub mod scan;
pub mod sfc;
pub mod stamps;
pub mod tsconfig_fallback;
pub mod tui;
pub mod watch;

pub use project::{Analysis, CacheArgs, FileViolation, Project};

static NODE: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);

/// The Node.js executable that evaluates JavaScript configs (rules configs,
/// Vite/webpack/Babel configs). The CLI leaves it unset, which runs `node`
/// from PATH; the ESLint add-on sets it to the Node running ESLint
/// (`process.execPath`), which may not be on PATH. One per process: a
/// process runs one Node.
pub fn set_node(path: Option<std::path::PathBuf>) {
    *NODE.write().unwrap_or_else(|e| e.into_inner()) = path;
}

/// A command running the Node.js executable (see `set_node`).
pub(crate) fn node_command() -> std::process::Command {
    match &*NODE.read().unwrap_or_else(|e| e.into_inner()) {
        Some(p) => std::process::Command::new(p),
        None => std::process::Command::new("node"),
    }
}

/// With `DETANGLE_TIMINGS` set, prints how long a phase took to stderr.
pub fn timing(phase: &str, t: std::time::Instant) {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("DETANGLE_TIMINGS").is_some());
    if *ON {
        eprintln!("{phase:>8} {:7.1}ms", t.elapsed().as_secs_f64() * 1000.0);
    }
}
