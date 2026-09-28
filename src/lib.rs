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
pub mod migrate;
mod project;
pub mod report;
pub mod rules;
pub mod scan;
pub mod sfc;
pub mod tui;
pub mod watch;

pub use project::{Analysis, CacheArgs, FileViolation, Project};

/// With `DETANGLE_TIMINGS` set, prints how long a phase took to stderr.
pub fn timing(phase: &str, t: std::time::Instant) {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("DETANGLE_TIMINGS").is_some());
    if *ON {
        eprintln!("{phase:>8} {:7.1}ms", t.elapsed().as_secs_f64() * 1000.0);
    }
}
