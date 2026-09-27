use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::migrate;
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
#   unresolvable, type-only, dynamic, require, reexport, resource, import,
#   aliased, deprecated

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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub options: Options,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden: Vec<Rule>,
    /// Allow-list: every dependency must match at least one of these, or it
    /// is reported as `not-in-allowed` with `allowed_severity`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<AllowedRule>,
    #[serde(default)]
    pub allowed_severity: Severity,
    /// Modules matching `module` must depend on something matching `to`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<RequiredRule>,
    /// Named, tagged groups of modules (features, layers, packages…) for
    /// `scope = "group"` rules and `tags` conditions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<GroupDef>,
}

/// A kind of group. Each distinct match of `path` (at the start of a module
/// path) is one group instance: `^src/features/[^/]+` makes every feature
/// folder its own group. A module belongs to the first definition matching it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupDef {
    /// The group's type; also usable as a tag.
    pub name: String,
    pub path: Pat,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        toml::from_str(DEFAULT_CONFIG).expect("built-in config is valid")
    }
}

impl Config {
    pub fn empty() -> Self {
        Config {
            options: Options::default(),
            forbidden: vec![],
            allowed: vec![],
            allowed_severity: Severity::Warn,
            required: vec![],
            groups: vec![],
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    /// Globs of files to analyse.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Globs of files to skip.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Regex: only modules (files *and* dependency targets) matching this are
    /// part of the graph.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_only: Option<Pat>,
    /// Regex: modules (files and dependency targets) matching this are left
    /// out of the graph entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_path: Option<Pat>,
    #[serde(default = "yes")]
    pub cycles_ignore_type_only: bool,
    /// Drop `import type` dependencies from the graph altogether.
    #[serde(skip_serializing_if = "is_false")]
    pub ignore_type_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsconfig: Option<String>,
    /// Import aliases, e.g. `"@" = "./src"` (`./` = relative to the root).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub aliases: std::collections::BTreeMap<String, String>,
    /// Take `resolve.alias` / `resolve.modules` / `resolve.extensions` from
    /// this webpack config (evaluated with Node).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webpack_config: Option<String>,
    /// Take `resolve.alias` / `resolve.extensions` from this Vite config
    /// (evaluated with Node).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vite_config: Option<String>,
    /// Take babel-plugin-module-resolver's `alias` / `root` from this Babel
    /// config (.babelrc, babel.config.js or package.json).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub babel_config: Option<String>,
    /// Known violations to ignore (a file written by `--write-baseline` or
    /// `tangle migrate`), relative to the root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    /// Discover Nx projects (project.json, package.json "nx") on every run
    /// and treat each as a group with its tags (+ `projectType:<type>`).
    #[serde(skip_serializing_if = "is_false")]
    pub nx_projects: bool,
    /// How Vite / webpack / Babel configs are evaluated.
    #[serde(default, skip_serializing_if = "ConfigEnv::is_default")]
    pub config_env: ConfigEnv,
}

/// What JS build configs see when tangle evaluates them.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigEnv {
    /// Vite `mode` and webpack `argv.mode` (default "development").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Vite `command`: "serve" (default) or "build". Also sets webpack-cli's
    /// `env.WEBPACK_SERVE` / `env.WEBPACK_BUILD`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// webpack's `env` argument, as with `webpack --env production`.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    pub webpack_env: serde_json::Map<String, serde_json::Value>,
    /// Environment variables while the configs are evaluated. These beat the
    /// shell environment, which beats `.env` files.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub vars: std::collections::BTreeMap<String, String>,
    /// `.env` files to load (later override earlier; `{mode}` is replaced).
    /// Default: `.env`, `.env.local`, `.env.{mode}`, `.env.{mode}.local`.
    /// `false` disables loading.
    #[serde(skip_serializing_if = "EnvFiles::is_default")]
    pub env_files: EnvFiles,
    /// Directory holding the `.env` files (default: the project root).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum EnvFiles {
    Enabled(bool),
    List(Vec<String>),
}

impl Default for EnvFiles {
    fn default() -> Self {
        EnvFiles::Enabled(true)
    }
}

impl EnvFiles {
    fn is_default(&self) -> bool {
        *self == EnvFiles::default()
    }
}

impl ConfigEnv {
    fn is_default(&self) -> bool {
        *self == ConfigEnv::default()
    }

    pub fn mode(&self) -> &str {
        self.mode.as_deref().unwrap_or("development")
    }

    pub fn command(&self) -> &str {
        self.command.as_deref().unwrap_or("serve")
    }

    /// The `.env` file names to load, in order, with `{mode}` filled in.
    pub fn env_file_names(&self) -> Vec<String> {
        let names: Vec<String> = match &self.env_files {
            EnvFiles::Enabled(false) => return vec![],
            EnvFiles::Enabled(true) => [".env", ".env.local", ".env.{mode}", ".env.{mode}.local"].map(String::from).to_vec(),
            EnvFiles::List(l) => l.clone(),
        };
        names.into_iter().map(|n| n.replace("{mode}", self.mode())).collect()
    }

    /// `NODE_ENV` unless set explicitly (tangle.toml `vars`, then the shell,
    /// then `.env` files): "production" for a production mode or a build,
    /// else "development" (as Vite does).
    pub fn node_env(&self, files: &std::collections::BTreeMap<String, String>) -> String {
        if let Some(v) = self.vars.get("NODE_ENV") {
            return v.clone();
        }
        if let Ok(v) = std::env::var("NODE_ENV") {
            return v;
        }
        if let Some(v) = files.get("NODE_ENV") {
            return v.clone();
        }
        if self.mode() == "production" || self.command() == "build" { "production" } else { "development" }.into()
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            include: vec![],
            exclude: vec!["**/node_modules/**".into()],
            include_only: None,
            exclude_path: None,
            cycles_ignore_type_only: true,
            ignore_type_only: false,
            tsconfig: None,
            aliases: Default::default(),
            webpack_config: None,
            vite_config: None,
            config_env: ConfigEnv::default(),
            baseline: None,
            nx_projects: false,
            babel_config: None,
        }
    }
}

/// A regular expression, written as a string or a list of alternatives.
#[derive(Debug, Clone, PartialEq)]
pub struct Pat(pub String);

impl<'de> Deserialize<'de> for Pat {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }
        Ok(match OneOrMany::deserialize(d)? {
            OneOrMany::One(s) => Pat(s),
            OneOrMany::Many(v) => Pat::any(&v),
        })
    }
}

impl Serialize for Pat {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl Pat {
    /// Alternation of several regexes.
    pub fn any(v: &[String]) -> Pat {
        match v {
            [one] => Pat(one.clone()),
            many => Pat(many.iter().map(|p| format!("(?:{p})")).collect::<Vec<_>>().join("|")),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub name: String,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// `folder`: evaluate on the folder-level graph (paths match folders
    /// like `src/billing/`; cycles, instability and dependents are counted
    /// between folders).
    #[serde(default, skip_serializing_if = "Scope::is_module")]
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "FromSpec::is_empty")]
    pub from: FromSpec,
    #[serde(default, skip_serializing_if = "ToSpec::is_empty")]
    pub to: ToSpec,
    /// A rule about modules themselves (not their dependencies). With
    /// `module`, `from` restricts which dependents are counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<ModuleSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default, skip_serializing_if = "FromSpec::is_empty")]
    pub from: FromSpec,
    #[serde(default, skip_serializing_if = "ToSpec::is_empty")]
    pub to: ToSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredRule {
    pub name: String,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub module: ModuleSpec,
    pub to: ToSpec,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Module,
    Folder,
    /// The group graph: one node per group instance (see `[[groups]]`).
    Group,
}

impl Scope {
    pub fn is_module(&self) -> bool {
        *self == Scope::Module
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[serde(alias = "ignore")]
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FromSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Pat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_not: Option<Pat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphan: Option<bool>,
    /// Has at least one of these tags (Nx-style patterns: `*` globs, `/regex/`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Has none of these tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags_not: Option<Vec<String>>,
    /// Has all of these tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags_all: Option<Vec<String>>,
}

impl FromSpec {
    fn is_empty(&self) -> bool {
        self.path.is_none()
            && self.path_not.is_none()
            && self.orphan.is_none()
            && self.tags.is_none()
            && self.tags_not.is_none()
            && self.tags_all.is_none()
    }
}

/// `path` / `path_not`, used for `via` and `via_only`. Also accepts a bare
/// string or list, meaning `path`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PathSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Pat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_not: Option<Pat>,
}

impl<'de> Deserialize<'de> for PathSpec {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Full {
            path: Option<Pat>,
            path_not: Option<Pat>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Full(Full),
            Short(Pat),
        }
        Ok(match Either::deserialize(d)? {
            Either::Full(f) => PathSpec { path: f.path, path_not: f.path_not },
            Either::Short(p) => PathSpec { path: Some(p), path_not: None },
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Pat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_not: Option<Pat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub circular: Option<bool>,
    /// Circular only: some cycle through this dependency passes through a
    /// module matching this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<PathSpec>,
    /// Circular only: some cycle through this dependency consists solely of
    /// modules matching this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_only: Option<PathSpec>,
    /// Circular only: the shortest cycle through this dependency has at
    /// most this many modules (2 = the two modules import each other).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cycle_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency_types_not: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub could_not_resolve: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reachable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub more_unstable: Option<bool>,
    /// The package is declared in more than one section of package.json
    /// (e.g. both dependencies and devDependencies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub more_than_one_dependency_type: Option<bool>,
    /// Regex on the installed package's `license`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<Pat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license_not: Option<Pat>,
    /// Target has at least one / none of these tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags_not: Option<Vec<String>>,
    /// Module scope: source and target are both in groups, and in different
    /// (`true`) or the same (`false`) group instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cross_group: Option<bool>,
    /// Regex on the import specifier as written (`@org/lib`, `../x`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub specifier: Option<Pat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub specifier_not: Option<Pat>,
    /// The target, or anything it depends on directly or indirectly, has
    /// one of these tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reaches_tags: Option<Vec<String>>,
    /// The source also loads the target lazily: a chain of dynamic imports
    /// leads from the source to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lazy_loaded: Option<bool>,
}

impl ToSpec {
    fn is_empty(&self) -> bool {
        toml::to_string(self).map(|s| s.trim().is_empty()).unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModuleSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Pat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_not: Option<Pat>,
    /// Fewer than N modules depend on it (e.g. "shared code must be shared").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number_of_dependents_less_than: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number_of_dependents_more_than: Option<usize>,
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

pub struct Loaded {
    pub config: Config,
    pub path: Option<PathBuf>,
    /// Human-readable notes, e.g. about an imported JavaScript config.
    pub notes: Vec<String>,
}

/// Loads `explicit` (a JavaScript config is converted on the fly), else
/// `<root>/tangle.toml`, else the built-in defaults.
pub fn load(root: &Path, explicit: Option<&Path>) -> Result<Loaded> {
    let path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => Some(root.join(CONFIG_FILE)).filter(|p| p.is_file()),
    };
    let Some(path) = path else {
        return Ok(Loaded { config: Config::default(), path: None, notes: vec![] });
    };
    if migrate::is_js_config(&path) {
        let imported = migrate::import(&path)?;
        let mut notes = vec![format!(
            "using {} ({} rules imported; run `tangle init --from {}` to convert it)",
            path.file_name().unwrap_or_default().to_string_lossy(),
            imported.config.forbidden.len() + imported.config.allowed.len() + imported.config.required.len(),
            path.file_name().unwrap_or_default().to_string_lossy(),
        )];
        notes.extend(imported.warnings);
        return Ok(Loaded { config: imported.config, path: Some(path), notes });
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Loaded { config, path: Some(path), notes: vec![] })
}
