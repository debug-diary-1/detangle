//! Converts eslint-plugin-import / eslint-plugin-import-x dependency rules:
//! `no-cycle`, `no-restricted-paths` and `no-extraneous-dependencies`, from
//! flat (`eslint.config.*`) or legacy (`.eslintrc*`, package.json
//! `eslintConfig`) configs, honouring `files` / `ignores` / `overrides`
//! scoping and the `import/resolver` settings.

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::glob::{Plain, has_magic, to_regex};
use super::{Imported, run_node};
use crate::config::{Config, FromSpec, Options, Pat, Rule, Severity, ToSpec};

pub const FILES: &[&str] = &[
    "eslint.config.js",
    "eslint.config.mjs",
    "eslint.config.cjs",
    "eslint.config.ts",
    "eslint.config.mts",
    "eslint.config.cts",
    ".eslintrc.js",
    ".eslintrc.cjs",
    ".eslintrc.json",
    ".eslintrc",
];

const LOADER: &str = r#"
const fs = require('fs'), path = require('path'), { pathToFileURL } = require('url');
const strip = (t) => t.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '').replace(/,(\s*[}\]])/g, '$1');
(async () => {
  const file = process.env.TANGLE_CONFIG_FILE, base = path.basename(file);
  let c;
  if (base === 'package.json') c = JSON.parse(fs.readFileSync(file, 'utf8')).eslintConfig || {};
  else if (/\.(c|m)?[jt]s$/.test(base)) { const m = await import(pathToFileURL(file).href); c = await (m.default ?? m); }
  else { const t = fs.readFileSync(file, 'utf8'); try { c = JSON.parse(t); } catch { try { c = JSON.parse(strip(t)); } catch { throw new Error('only JSON .eslintrc files are supported (not YAML)'); } } }
  if (typeof c === 'function') c = await c();
  const flat = base.startsWith('eslint.config.');
  const pick = (rules) => Object.fromEntries(Object.entries(rules || {}).filter(([k]) => /^(import|import-x|i)\/(no-cycle|no-restricted-paths|no-extraneous-dependencies)$/.test(k)));
  const strs = (x) => [].concat(x || []).flat(Infinity).filter((s) => typeof s === 'string');
  const resolver = (s) => (s || {})['import/resolver'] || (s || {})['import-x/resolver'] || null;
  let items;
  if (flat) {
    items = [].concat(c).flat(Infinity).filter(Boolean).map((it) => ({
      files: it.files ? strs(it.files) : null,
      ignores: strs(it.ignores),
      globalIgnores: !!it.ignores && Object.keys(it).every((k) => k === 'ignores' || k === 'name'),
      rules: pick(it.rules), resolver: resolver(it.settings),
    }));
  } else {
    items = [{ files: null, ignores: [], globalIgnores: false, rules: pick(c.rules), resolver: resolver(c.settings) },
      ...(c.overrides || []).map((o) => ({ files: strs(o.files), ignores: strs(o.excludedFiles), globalIgnores: false, rules: pick(o.rules), resolver: resolver(o.settings) }))];
    if (c.ignorePatterns) items.unshift({ files: null, ignores: strs(c.ignorePatterns), globalIgnores: true, rules: {}, resolver: null });
  }
  process.stdout.write(JSON.stringify({ flat, items, extends: flat ? [] : strs(c.extends) }));
})().catch((e) => { console.error((e && e.message) || String(e)); process.exit(1); });
"#;

fn severity(v: &Value) -> (Severity, Vec<Value>) {
    let (level, opts) = match v {
        Value::Array(a) if !a.is_empty() => (&a[0], a[1..].to_vec()),
        other => (other, vec![]),
    };
    let sev = match level {
        Value::Number(n) if n.as_u64() == Some(2) => Severity::Error,
        Value::Number(n) if n.as_u64() == Some(1) => Severity::Warn,
        Value::String(s) if s == "error" => Severity::Error,
        Value::String(s) if s == "warn" => Severity::Warn,
        _ => Severity::Off,
    };
    (sev, opts)
}

/// One active rule while replaying the cascade.
struct Active {
    rule: Rule,
    /// Files it's restricted to (none = all).
    scoped: bool,
}

pub fn import(file: &Path, root: &Path) -> Result<Imported> {
    let v: Value = serde_json::from_str(&run_node(LOADER, file)?)?;
    let flat = v["flat"].as_bool().unwrap_or(false);
    let dir = file.parent().unwrap_or(root);
    let prefix = dir.strip_prefix(root).unwrap_or(Path::new("")).to_string_lossy().replace('\\', "/");
    let mut warnings = vec![];
    let mut options = Options::default();
    let files_re = |globs: &[String]| -> Option<Pat> {
        (!globs.is_empty()).then(|| Pat::any(&globs.iter().map(|g| to_regex(g, Plain::Exact, !flat, &prefix)).collect::<Vec<_>>()))
    };
    let strs = |x: &Value| -> Vec<String> { x.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default() };

    let mut active: Vec<(String, Active)> = vec![];
    for item in v["items"].as_array().cloned().unwrap_or_default() {
        let files = item["files"].as_array().map(|_| strs(&item["files"]));
        let ignores = strs(&item["ignores"]);
        if item["globalIgnores"].as_bool() == Some(true) {
            options.exclude.extend(ignores.iter().map(|g| {
                let g = g.trim_start_matches("./");
                let g = if prefix.is_empty() { g.to_string() } else { format!("{prefix}/{g}") };
                if has_magic(&g) { g } else { format!("{}/**", g.trim_end_matches('/')) }
            }));
            continue;
        }
        if let Some(r) = item.get("resolver").filter(|r| !r.is_null()) {
            resolver_settings(r, &prefix, &mut options, &mut warnings);
        }
        for (name, value) in item["rules"].as_object().cloned().unwrap_or_default() {
            let (sev, opts) = severity(&value);
            let short = name.rsplit('/').next().unwrap_or(&name).to_string();
            match &files {
                None => {
                    // Applies to every file: replaces whatever came before.
                    active.retain(|(n, _)| *n != short);
                    if sev != Severity::Off {
                        for rule in convert(&name, &short, sev, &opts, &prefix, &mut warnings) {
                            active.push((short.clone(), Active { rule, scoped: false }));
                        }
                    }
                }
                Some(globs) => {
                    let scope = files_re(globs);
                    if sev == Severity::Off {
                        // `"off"` for some files carves them out of earlier rules.
                        for (_, a) in active.iter_mut().filter(|(n, _)| *n == short) {
                            if let Some(s) = &scope {
                                a.rule.from.path_not = Some(or(a.rule.from.path_not.take(), s.clone()));
                            }
                        }
                    } else {
                        for mut rule in convert(&name, &short, sev, &opts, &prefix, &mut warnings) {
                            rule.from.path = match (rule.from.path.take(), &scope) {
                                // A zone's target narrowed to these files: keep both via lookahead.
                                (Some(t), Some(s)) => Some(Pat(format!("(?=(?:{}))(?:{})", s.0, t.0))),
                                (t, s) => t.or_else(|| s.clone()),
                            };
                            if let Some(ig) = files_re(&ignores) {
                                rule.from.path_not = Some(or(rule.from.path_not.take(), ig));
                            }
                            active.push((short.clone(), Active { rule, scoped: true }));
                        }
                    }
                }
            }
        }
    }
    let extends = strs(&v["extends"]);
    if extends.iter().any(|e| e.contains("import")) {
        warnings.push(format!("extends {extends:?}: rules enabled only through shared configs aren't converted — list them in the config itself"));
    }
    let scoped = active.iter().filter(|(_, a)| a.scoped).count();
    let mut config = Config::empty();
    config.options = options;
    config.forbidden = active.into_iter().map(|(_, a)| a.rule).collect();
    let summary = describe(&config.forbidden, scoped);
    Ok(Imported { config, warnings, summary, known_violations: None })
}

fn or(a: Option<Pat>, b: Pat) -> Pat {
    match a {
        Some(a) => Pat(format!("(?:{})|(?:{})", a.0, b.0)),
        None => b,
    }
}

fn describe(rules: &[Rule], scoped: usize) -> String {
    let mut names: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
    names.dedup();
    let mut s = if names.is_empty() { "no dependency rules enabled".into() } else { names.join(", ") };
    if scoped > 0 {
        s.push_str(&format!(" ({scoped} file-scoped)"));
    }
    s
}

/// `settings["import/resolver"]`: webpack and typescript resolvers carry over.
fn resolver_settings(r: &Value, prefix: &str, options: &mut Options, warnings: &mut Vec<String>) {
    let join = |p: &str| if prefix.is_empty() { p.to_string() } else { format!("{prefix}/{p}") };
    let obj = match r {
        Value::Object(o) => o.clone(),
        Value::String(s) => [(s.clone(), json!({}))].into_iter().collect(),
        _ => return,
    };
    for (kind, cfg) in obj {
        match kind.as_str() {
            "webpack" => {
                let file = cfg.get("config").and_then(Value::as_str).unwrap_or("webpack.config.js");
                options.webpack_config.get_or_insert_with(|| join(file));
            }
            "typescript" => {
                if let Some(p) = cfg.get("project").and_then(|p| p.as_str().or_else(|| p.get(0).and_then(Value::as_str))) {
                    options.tsconfig.get_or_insert_with(|| join(p));
                }
            }
            "node" | "exports" => {}
            other => warnings.push(format!("import/resolver \"{other}\" isn't supported (tangle resolves like Node + TypeScript)")),
        }
    }
}

fn rule(name: &str, sev: Severity, comment: &str, from: FromSpec, to: ToSpec) -> Rule {
    Rule { name: name.into(), severity: sev, comment: Some(comment.into()), scope: Default::default(), from, to, module: None }
}

fn convert(full: &str, short: &str, sev: Severity, opts: &[Value], prefix: &str, warnings: &mut Vec<String>) -> Vec<Rule> {
    let o = opts.first().cloned().unwrap_or(json!({}));
    match short {
        "no-cycle" => {
            if let Some(d) = o.get("maxDepth").filter(|d| d.as_u64().is_some()) {
                warnings.push(format!("{full}: maxDepth {d} isn't supported — cycles of any length are reported"));
            }
            if o.get("allowUnsafeDynamicCyclicDependency") == Some(&json!(true)) {
                warnings.push(format!("{full}: allowUnsafeDynamicCyclicDependency isn't supported — cycles through dynamic imports are reported"));
            }
            // Like the plugin, `import type` doesn't count (tangle's default).
            vec![rule(full, sev, "Circular dependency (converted from ESLint).", FromSpec::default(), ToSpec { circular: Some(true), ..Default::default() })]
        }
        "no-restricted-paths" => {
            let base = o.get("basePath").and_then(Value::as_str).map(|b| b.trim_start_matches("./").trim_end_matches('/').to_string());
            let prefix = match (prefix, base.as_deref()) {
                (p, Some(b)) if !p.is_empty() => format!("{p}/{b}"),
                (_, Some(b)) => b.to_string(),
                (p, None) => p.to_string(),
            };
            let paths = |x: &Value| -> Vec<String> {
                match x {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(a) => a.iter().filter_map(|s| s.as_str().map(String::from)).collect(),
                    _ => vec![],
                }
            };
            let re = |list: &[String]| Pat::any(&list.iter().map(|p| to_regex(p, Plain::Prefix, false, &prefix)).collect::<Vec<_>>());
            let mut out = vec![];
            for zone in o.get("zones").and_then(Value::as_array).cloned().unwrap_or_default() {
                let (target, from) = (paths(&zone["target"]), paths(&zone["from"]));
                if target.is_empty() || from.is_empty() {
                    continue;
                }
                // `except` paths are relative to each (non-glob) `from` path.
                let except: Vec<String> = paths(&zone["except"])
                    .iter()
                    .flat_map(|e| {
                        from.iter().map(move |f| {
                            if has_magic(f) || has_magic(e) { e.clone() } else { format!("{}/{}", f.trim_end_matches('/'), e.trim_start_matches("./")) }
                        })
                    })
                    .collect();
                let comment = zone["message"].as_str().map(String::from).unwrap_or_else(|| format!("{} may not import from {}.", target.join(", "), from.join(", ")));
                out.push(rule(
                    full,
                    sev,
                    &comment,
                    FromSpec { path: Some(re(&target)), ..Default::default() },
                    ToSpec { path: Some(re(&from)), path_not: (!except.is_empty()).then(|| re(&except)), ..Default::default() },
                ));
            }
            out
        }
        "no-extraneous-dependencies" => {
            if o.get("packageDir").is_some() {
                warnings.push(format!("{full}: packageDir is ignored — tangle checks every package.json above each file"));
            }
            let include_types = o.get("includeTypes") == Some(&json!(true));
            let type_only = (!include_types).then_some(false);
            let mut out = vec![rule(
                full,
                sev,
                "Package isn't declared in package.json (converted from ESLint).",
                FromSpec::default(),
                ToSpec { dependency_types: Some(vec!["npm-undeclared".into()]), type_only, ..Default::default() },
            )];
            // devDependencies / optionalDependencies / peerDependencies: `false`
            // forbids them everywhere, a glob list allows them only there.
            for (key, ty, others) in [
                ("devDependencies", "npm-dev", ["npm", "npm-optional", "npm-peer"]),
                ("optionalDependencies", "npm-optional", ["npm", "npm-dev", "npm-peer"]),
                ("peerDependencies", "npm-peer", ["npm", "npm-dev", "npm-optional"]),
            ] {
                let allowed_in = match o.get(key) {
                    None | Some(Value::Bool(true)) => continue,
                    Some(Value::Bool(false)) => None,
                    Some(Value::Array(globs)) => Some(Pat::any(
                        &globs.iter().filter_map(Value::as_str).map(|g| to_regex(g, Plain::Exact, false, prefix)).collect::<Vec<_>>(),
                    )),
                    Some(_) => continue,
                };
                out.push(rule(
                    full,
                    sev,
                    &format!("Only allowed where {key} are permitted (converted from ESLint)."),
                    FromSpec { path_not: allowed_in, ..Default::default() },
                    ToSpec {
                        dependency_types: Some(vec![ty.into()]),
                        // A package also declared as a regular dependency is fine.
                        dependency_types_not: Some(others.iter().map(|s| s.to_string()).collect()),
                        type_only,
                        ..Default::default()
                    },
                ));
            }
            out
        }
        _ => vec![],
    }
}

/// Fails with a clear message if `file` isn't a config we understand.
pub fn check_supported(file: &Path) -> Result<()> {
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".yml") || name.ends_with(".yaml") {
        bail!("YAML ESLint configs aren't supported; convert {name} to JSON or JS first");
    }
    Ok(())
}
