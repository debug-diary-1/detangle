//! `detangle migrate`: finds the dependency rules a project already has —
//! JavaScript rules configs, ESLint import rules, madge — and converts them
//! into a single `detangle.toml` (plus a baseline from known-violation files).
//!
//! Configs are recognised by what they contain, not by product names.

mod baseline;
mod eslint;
mod glob;
mod madge;
mod rules_js;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::Value;

use crate::config::{Config, Options, Rule};
use crate::rules::BaselineEntry;

pub use rules_js::{import, is_js_config, to_toml};

/// What one source converted to.
pub struct Imported {
    pub config: Config,
    pub warnings: Vec<String>,
    /// One line for the migration summary.
    pub summary: String,
    /// A known-violations file the source pointed at.
    pub known_violations: Option<String>,
}

pub(crate) fn run_node(script: &str, file: &Path) -> Result<String> {
    let abs = dunce::canonicalize(file).with_context(|| format!("{} not found", file.display()))?;
    let out = Command::new("node")
        .args(["-e", script])
        .env("DETANGLE_CONFIG_FILE", &abs)
        .current_dir(abs.parent().unwrap_or(Path::new(".")))
        .output()
        .context("running `node` (is Node.js installed?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let hint = if err.contains("Cannot find module") || err.contains("ERR_MODULE_NOT_FOUND") {
            "\nhint: the config imports packages that aren't installed — run your package manager's install first"
        } else {
            ""
        };
        bail!("{err}{hint}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// A JS/JSON config with `forbidden` / `allowed` / `required` rules.
    RulesConfig,
    Eslint,
    MadgeRc,
    /// A package.json script running `madge --circular`.
    MadgeScript { script: String, command: String },
    KnownViolations,
}

#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub kind: Kind,
}

impl Source {
    pub fn label(&self, root: &Path) -> String {
        let p = self.path.strip_prefix(root).unwrap_or(&self.path).display().to_string();
        match &self.kind {
            Kind::MadgeScript { script, .. } => format!("{p} (script \"{script}\")"),
            _ => p,
        }
    }
}

const MAX_CONFIG_BYTES: u64 = 1 << 20;

fn read_small(p: &Path) -> Option<String> {
    let meta = std::fs::metadata(p).ok()?;
    (meta.is_file() && meta.len() <= MAX_CONFIG_BYTES).then(|| std::fs::read_to_string(p).ok()).flatten()
}

/// Finds migratable configs in `root` (not recursively).
pub fn discover(root: &Path) -> Vec<Source> {
    let mut found = vec![];
    let rules_shape = Regex::new(r#"(?m)["']?\b(forbidden|allowed|required)\b["']?\s*:\s*\["#).unwrap();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    entries.sort();
    for path in &entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if eslint::FILES.contains(&name) {
            found.push(Source { path: path.clone(), kind: Kind::Eslint });
            continue;
        }
        if name == ".madgerc" {
            found.push(Source { path: path.clone(), kind: Kind::MadgeRc });
            continue;
        }
        if !matches!(ext, "js" | "cjs" | "mjs" | "json") || name == "package.json" || name == "package-lock.json" {
            continue;
        }
        let Some(text) = read_small(path) else { continue };
        if ext == "json" && baseline::looks_like_known_violations(&text) {
            found.push(Source { path: path.clone(), kind: Kind::KnownViolations });
        } else if (name.starts_with('.') || name.contains("config")) && rules_shape.is_match(&text) {
            found.push(Source { path: path.clone(), kind: Kind::RulesConfig });
        }
    }
    // package.json: eslintConfig, madge key, `madge --circular` scripts.
    let pkg = root.join("package.json");
    if let Some(v) = read_small(&pkg).and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        if v.get("eslintConfig").is_some() && !found.iter().any(|s| s.kind == Kind::Eslint) {
            found.push(Source { path: pkg.clone(), kind: Kind::Eslint });
        }
        if v.get("madge").is_some() && !found.iter().any(|s| s.kind == Kind::MadgeRc) {
            found.push(Source { path: pkg.clone(), kind: Kind::MadgeRc });
        }
        if !found.iter().any(|s| s.kind == Kind::MadgeRc) {
            for (script, cmd) in v.get("scripts").and_then(Value::as_object).into_iter().flatten() {
                if let Some(cmd) = cmd.as_str().filter(|c| madge::from_script(c).is_some()) {
                    found.push(Source { path: pkg.clone(), kind: Kind::MadgeScript { script: script.clone(), command: cmd.into() } });
                    break;
                }
            }
        }
    }
    found
}

/// The merged result of a migration.
pub struct Migration {
    pub config: Config,
    /// (source label, summary)
    pub converted: Vec<(String, String)>,
    pub warnings: Vec<String>,
    pub baseline: Option<(String, Vec<BaselineEntry>)>,
    /// package.json scripts that run the old tools: (name, command).
    pub scripts: Vec<(String, String)>,
    /// Rules that were duplicates of another source's rule.
    pub merged: Vec<String>,
    /// Sources that were found but had nothing to convert.
    pub empty: Vec<String>,
}

pub fn migrate(root: &Path, sources: &[Source]) -> Result<Migration> {
    let mut m = Migration {
        config: Config::empty(),
        converted: vec![],
        warnings: vec![],
        baseline: None,
        scripts: vec![],
        merged: vec![],
        empty: vec![],
    };
    let mut known: Vec<PathBuf> = sources.iter().filter(|s| s.kind == Kind::KnownViolations).map(|s| s.path.clone()).collect();
    let mut options: Vec<(String, Options)> = vec![];
    for s in sources {
        let label = s.label(root);
        let imported = match &s.kind {
            Kind::RulesConfig => import(&s.path),
            Kind::Eslint => eslint::import(&s.path, root),
            Kind::MadgeRc => madge::from_rc(&s.path).map(|o| o.expect("discovered with a madge key")),
            Kind::MadgeScript { command, .. } => Ok(madge::from_script(command).expect("discovered as a madge script")),
            Kind::KnownViolations => continue,
        };
        let imported = match imported {
            Ok(i) => i,
            Err(e) => {
                m.warnings.push(format!("{label}: not converted — {e:#}"));
                continue;
            }
        };
        if let Some(kv) = &imported.known_violations {
            let p = s.path.parent().unwrap_or(root).join(kv);
            if !known.contains(&p) {
                known.push(p);
            }
        }
        m.warnings.extend(imported.warnings.iter().map(|w| format!("{label}: {w}")));
        let c = &imported.config;
        let nothing = c.forbidden.is_empty() && c.allowed.is_empty() && c.required.is_empty() && imported.known_violations.is_none();
        if nothing {
            m.empty.push(label);
            continue;
        }
        m.converted.push((label.clone(), imported.summary.clone()));
        options.push((label, imported.config.options.clone()));
        // Rules from earlier sources; one source may use a name several times.
        let earlier = m.config.forbidden.len();
        for rule in imported.config.forbidden {
            add_rule(&mut m, rule, earlier);
        }
        if !imported.config.allowed.is_empty() {
            m.config.allowed.extend(imported.config.allowed);
            m.config.allowed_severity = imported.config.allowed_severity;
        }
        m.config.required.extend(imported.config.required);
        for g in imported.config.groups {
            if !m.config.groups.contains(&g) {
                m.config.groups.push(g);
            }
        }
    }
    m.config.options = merge_options(options, &mut m.warnings);
    let mut seen = std::collections::HashSet::new();
    m.warnings.retain(|w| seen.insert(w.clone()));

    if let Some(file) = known.first() {
        match baseline::convert(file) {
            Ok(entries) => m.baseline = Some((file.strip_prefix(root).unwrap_or(file).display().to_string(), entries)),
            Err(e) => m.warnings.push(format!("{}: {e:#}", file.display())),
        }
    }
    m.scripts = old_scripts(root, sources);
    Ok(m)
}

/// Rule identity for de-duplication: everything but name, severity, comment.
fn shape(r: &Rule) -> String {
    let mut r = r.clone();
    r.name.clear();
    r.comment = None;
    r.severity = Default::default();
    toml::to_string(&r).unwrap_or_default()
}

fn add_rule(m: &mut Migration, rule: Rule, earlier: usize) {
    let s = shape(&rule);
    if let Some(existing) = m.config.forbidden.iter_mut().find(|r| shape(r) == s) {
        m.merged.push(format!("{} = {} (kept the stricter severity)", rule.name, existing.name));
        existing.severity = existing.severity.max(rule.severity);
        return;
    }
    // A different rule from another source with the same name would be
    // confusing in reports, so rename; a source's own same-named rules
    // (e.g. one per Nx constraint) stay grouped under one name.
    let mut rule = rule;
    let taken = |n: &str, rules: &[Rule]| rules[..earlier].iter().any(|r| r.name == n && shape(r) != s);
    if taken(&rule.name, &m.config.forbidden) {
        let mut i = 2;
        while taken(&format!("{}-{i}", rule.name), &m.config.forbidden) {
            i += 1;
        }
        rule.name = format!("{}-{i}", rule.name);
    }
    m.config.forbidden.push(rule);
}

/// Combines each source's options; conflicts are reported, first one wins.
fn merge_options(list: Vec<(String, Options)>, warnings: &mut Vec<String>) -> Options {
    let default = Options::default();
    let mut out = Options::default();
    let mut conflict = |what: &str, a: &str, b: &str| warnings.push(format!("options.{what}: sources disagree ({a} vs {b}); kept {a}"));
    for (label, o) in &list {
        macro_rules! take {
            ($field:ident) => {
                if o.$field != default.$field {
                    if out.$field == default.$field {
                        out.$field = o.$field.clone();
                    } else if out.$field != o.$field {
                        conflict(stringify!($field), &format!("{:?}", out.$field), &format!("{:?} from {label}", o.$field));
                    }
                }
            };
        }
        take!(tsconfig);
        take!(webpack_config);
        take!(babel_config);
        take!(vite_config);
        take!(include_only);
        take!(config_env);
        take!(ignore_type_only);
        take!(cycles_ignore_type_only);
        take!(nx_projects);
        take!(group_match);
        for g in &o.exclude {
            if !out.exclude.contains(g) {
                out.exclude.push(g.clone());
            }
        }
        // Excludes from several tools all apply.
        out.exclude_path = match (out.exclude_path.take(), &o.exclude_path) {
            (Some(a), Some(b)) if a != *b => Some(crate::config::Pat(format!("(?:{})|(?:{})", a.0, b.0))),
            (a, b) => a.or_else(|| b.clone()),
        };
    }
    out
}

/// package.json scripts that run a migrated tool, found by the config file
/// names discovered at runtime (and madge by name).
fn old_scripts(root: &Path, sources: &[Source]) -> Vec<(String, String)> {
    let Some(v) = read_small(&root.join("package.json")).and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
        return vec![];
    };
    let needles: Vec<String> = sources
        .iter()
        .filter(|s| matches!(s.kind, Kind::RulesConfig | Kind::KnownViolations))
        .filter_map(|s| s.path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let mut out = vec![];
    for (name, cmd) in v.get("scripts").and_then(Value::as_object).into_iter().flatten() {
        let Some(cmd) = cmd.as_str() else { continue };
        if needles.iter().any(|n| cmd.contains(n.as_str())) || madge::from_script(cmd).is_some() {
            out.push((name.clone(), cmd.to_string()));
        }
    }
    out
}

/// Renders the migration as a commented `detangle.toml`.
pub fn render(m: &Migration, baseline_file: Option<&str>) -> Result<String> {
    let mut out = String::from("# detangle.toml — generated by `detangle migrate` from:\n");
    for (label, summary) in &m.converted {
        out.push_str(&format!("#   - {label}: {summary}\n"));
    }
    if !m.warnings.is_empty() {
        out.push_str("#\n# Not converted (review these):\n");
        for w in &m.warnings {
            out.push_str(&format!("#   - {}\n", w.replace('\n', "\n#     ")));
        }
    }
    if !m.merged.is_empty() {
        out.push_str("#\n# Duplicate rules merged:\n");
        for w in &m.merged {
            out.push_str(&format!("#   - {w}\n"));
        }
    }
    out.push('\n');
    let mut config = m.config.clone();
    if let Some(b) = baseline_file {
        config.options.baseline = Some(b.to_string());
    }
    out.push_str(&toml::to_string_pretty(&config)?);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_by_content() {
        let dir = std::env::temp_dir().join(format!("detangle-discover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = |n: &str, t: &str| std::fs::write(dir.join(n), t).unwrap();
        w(".rules-something.cjs", "module.exports = { forbidden: [{ name: 'x', from: {}, to: {} }] };");
        w(".prettierrc.js", "module.exports = { semi: false };");
        w("my-tool.config.json", r#"{ "allowed": [ { "from": {}, "to": {} } ] }"#);
        w("known.json", r#"[{"type":"module","from":"a.ts","to":"a.ts","rule":{"name":"no-orphans","severity":"info"}}]"#);
        w("data.json", r#"[{"from": 1}]"#);
        w("eslint.config.js", "export default [];");
        w(".madgerc", "{}");
        w("package.json", r#"{"name":"x","scripts":{"deps":"madge --circular src","build":"tsc"}}"#);
        let found: Vec<(String, Kind)> = discover(&dir)
            .into_iter()
            .map(|s| (s.path.file_name().unwrap().to_string_lossy().into_owned(), s.kind))
            .collect();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            found,
            [
                (".madgerc".into(), Kind::MadgeRc),
                (".rules-something.cjs".into(), Kind::RulesConfig),
                ("eslint.config.js".into(), Kind::Eslint),
                ("known.json".into(), Kind::KnownViolations),
                ("my-tool.config.json".into(), Kind::RulesConfig),
            ]
        );
    }
}
