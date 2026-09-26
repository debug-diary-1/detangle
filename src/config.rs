use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "tangle.toml";

/// The default configuration. `tangle init` writes this verbatim, and it is
/// used as-is when a project has no `tangle.toml`.
pub const DEFAULT_CONFIG: &str = r#"# tangle.toml — dependency rules for this project.
#
# Paths are relative to the project root and matched as regular expressions.
# Inside `to.path` / `to.path_not`, $1..$9 refer to capture groups of `from.path`.
#
# Dependency types usable in `to.dependency_types`:
#   local, npm, npm-dev, npm-peer, npm-optional, npm-undeclared, core,
#   unresolvable, type-only, dynamic, require, reexport, resource

[options]
# Globs of files to analyse (empty = everything that isn't gitignored).
include = []
exclude = ["**/node_modules/**", "**/dist/**", "**/build/**", "**/coverage/**", "**/.next/**"]
# Type-only imports are erased at compile time, so they don't create runtime cycles.
cycles_ignore_type_only = true
# Explicit tsconfig for path aliases. Default: nearest tsconfig.json per file.
# tsconfig = "tsconfig.json"

[[forbidden]]
name = "no-circular"
severity = "warn"
comment = "Circular dependencies make code hard to reason about, test and tree-shake."
to = { circular = true }

[[forbidden]]
name = "no-orphans"
severity = "info"
comment = "Nothing imports this module and it imports nothing — likely dead code."
from = { orphan = true, path_not = '(^|/)\.[^/]+\.[cm]?[jt]s$|\.d\.[cm]?ts$|(^|/)[^/]+\.config\.[cm]?[jt]s$|(^|/)(scripts|bin)/' }

[[forbidden]]
name = "not-to-unresolvable"
severity = "error"
comment = "This import can't be resolved to a file or an installed package."
to = { could_not_resolve = true }

[[forbidden]]
name = "no-undeclared-deps"
severity = "error"
comment = "Package is imported but not declared in any package.json — it only works through hoisting."
to = { dependency_types = ["npm-undeclared"] }

[[forbidden]]
name = "not-to-dev-dep"
severity = "error"
comment = "Production code must not depend on devDependencies (type-only imports are fine)."
from = { path = '^(src|lib|app|packages/[^/]+/src)/', path_not = '\.(spec|test|stories)\.[cm]?[jt]sx?$|(^|/)__(tests|mocks)__/' }
to = { dependency_types = ["npm-dev"], type_only = false }

[[forbidden]]
name = "not-to-test"
severity = "error"
comment = "Production code should not import test files."
from = { path_not = '\.(spec|test)\.[cm]?[jt]sx?$|(^|/)__(tests|mocks)__/|(^|/)(test|tests|e2e)/' }
to = { path = '\.(spec|test)\.[cm]?[jt]sx?$|(^|/)__(tests|mocks)__/' }

# Example — features must not reach into each other:
# [[forbidden]]
# name = "no-cross-feature"
# severity = "error"
# from = { path = '^src/features/([^/]+)/' }
# to = { path = '^src/features/', path_not = '^src/features/$1/' }

# Example — everything under src/ must be reachable from the entry point:
# [[forbidden]]
# name = "no-dead-files"
# severity = "warn"
# from = { path = '^src/index\.ts$' }
# to = { path = '^src/', path_not = '\.(spec|test)\.ts$', reachable = false }
"#;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub options: Options,
    #[serde(default)]
    pub forbidden: Vec<Rule>,
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str(DEFAULT_CONFIG).expect("built-in config is valid")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub cycles_ignore_type_only: bool,
    pub tsconfig: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            include: vec![],
            exclude: vec!["**/node_modules/**".into()],
            cycles_ignore_type_only: true,
            tsconfig: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub name: String,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub from: FromSpec,
    #[serde(default)]
    pub to: ToSpec,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Off,
    Info,
    #[default]
    Warn,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Off => "off",
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FromSpec {
    pub path: Option<String>,
    pub path_not: Option<String>,
    pub orphan: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToSpec {
    pub path: Option<String>,
    pub path_not: Option<String>,
    pub circular: Option<bool>,
    pub dependency_types: Option<Vec<String>>,
    pub dependency_types_not: Option<Vec<String>>,
    pub could_not_resolve: Option<bool>,
    pub type_only: Option<bool>,
    pub dynamic: Option<bool>,
    pub reachable: Option<bool>,
    pub more_unstable: Option<bool>,
}

/// Finds the project root: the nearest ancestor of `start` holding a
/// `tangle.toml`, else one holding a `package.json`, else `start` itself.
pub fn find_root(start: &Path) -> PathBuf {
    for marker in [CONFIG_FILE, "package.json"] {
        if let Some(dir) = start.ancestors().find(|d| d.join(marker).is_file()) {
            return dir.to_path_buf();
        }
    }
    start.to_path_buf()
}

pub fn load(root: &Path, explicit: Option<&Path>) -> Result<(Config, Option<PathBuf>)> {
    let path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => Some(root.join(CONFIG_FILE)).filter(|p| p.is_file()),
    };
    let Some(path) = path else {
        return Ok((Config::default(), None));
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok((cfg, Some(path)))
}
