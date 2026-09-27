//! Imports JavaScript-format rule configs (`.js`, `.cjs`, `.mjs` or `.json`
//! files exporting `forbidden` / `allowed` / `required` rules and `options`).
//!
//! JS configs are evaluated with Node — the only faithful way to load them,
//! including `extends` chains that point at preset packages. The result is
//! converted rule by rule; anything tangle can't honour exactly makes that
//! rule be skipped with a warning rather than silently loosened.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use super::Imported;
use crate::config::{Config, Options, Pat};
use crate::rules::canonical_type;

/// A config in JavaScript format (anything but tangle's own TOML).
pub fn is_js_config(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e, "js" | "cjs" | "mjs" | "json"))
}


/// Loads (via Node) and merges `extends`, printing plain JSON.
const LOADER: &str = r#"
const path = require('path'), fs = require('fs'), { pathToFileURL } = require('url');
async function load(file) {
  let c;
  if (file.endsWith('.json')) c = JSON.parse(fs.readFileSync(file, 'utf8'));
  else { const m = await import(pathToFileURL(file).href); c = m.default ?? m; }
  if (typeof c === 'function') c = await c();
  let merged = {};
  for (const e of [].concat(c.extends || [])) {
    merged = merge(merged, await load(resolveExtends(e, path.dirname(file))));
  }
  return merge(merged, c);
}
// Preset files inside packages are often missing from the package's
// "exports" map, so fall back to locating the file directly.
function resolveExtends(e, dir) {
  try { return require.resolve(e, { paths: [dir] }); } catch {}
  const parts = e.split('/');
  const n = e.startsWith('@') ? 2 : 1;
  const pkg = parts.slice(0, n).join('/'), sub = parts.slice(n).join('/');
  for (let d = dir; ; d = path.dirname(d)) {
    const base = path.join(d, 'node_modules', pkg, sub);
    for (const ext of ['', '.cjs', '.js', '.mjs', '.json']) {
      const f = base + ext;
      if (fs.existsSync(f) && fs.statSync(f).isFile()) return f;
    }
    if (path.dirname(d) === d) break;
  }
  throw new Error(`can't resolve extends "${e}" from ${dir} — is the package installed?`);
}
function byName(a = [], b = []) {
  const out = [...a];
  for (const r of b) {
    const i = r.name ? out.findIndex((x) => x.name === r.name) : -1;
    if (i >= 0) out[i] = { ...out[i], ...r }; else out.push(r);
  }
  return out;
}
function merge(a, b) {
  return { ...a, ...b, extends: undefined,
    forbidden: byName(a.forbidden, b.forbidden),
    allowed: [...(a.allowed || []), ...(b.allowed || [])],
    required: byName(a.required, b.required),
    options: { ...(a.options || {}), ...(b.options || {}) } };
}
load(process.env.TANGLE_DC_CONFIG).then(
  (c) => process.stdout.write(JSON.stringify(c, (k, v) => (v instanceof RegExp ? v.source : v))),
  (e) => { console.error((e && e.message) || String(e)); process.exit(1); });
"#;

fn load_json(path: &Path) -> Result<Value> {
    // Plain JSON without `extends` needs no Node.
    if path.extension().is_some_and(|e| e == "json") {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let v: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if v.get("extends").is_none() {
            return Ok(v);
        }
    }
    let abs = std::fs::canonicalize(path).with_context(|| format!("{} not found", path.display()))?;
    let out = Command::new("node")
        .args(["-e", LOADER])
        .env("TANGLE_DC_CONFIG", &abs)
        .current_dir(abs.parent().unwrap_or(Path::new(".")))
        .output()
        .context("running `node` to load the JavaScript config (is Node.js installed?)")?;
    if !out.status.success() {
        bail!("loading {}: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    serde_json::from_slice(&out.stdout).context("reading the evaluated JavaScript config")
}

pub fn import(path: &Path) -> Result<Imported> {
    Ok(convert(&load_json(path)?))
}

/// Why a rule can't be imported faithfully.
type Skip = String;

fn camel_to_snake(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Accepts a regex string or a list of them.
fn pattern(v: &Value, what: &str) -> Result<Value, Skip> {
    match v {
        Value::String(_) => Ok(v.clone()),
        Value::Array(a) if a.iter().all(Value::is_string) => Ok(v.clone()),
        _ => Err(format!("{what} must be a regex string or list")),
    }
}

fn dep_types(v: &Value, what: &str) -> Result<Value, Skip> {
    let list: Vec<&str> = match v {
        Value::String(s) => vec![s],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => return Err(format!("{what} must be a list")),
    };
    let mut out = vec![];
    for t in list {
        match canonical_type(t) {
            Some(c) => out.push(json!(c)),
            None => return Err(format!("dependency type {t:?} isn't supported")),
        }
    }
    Ok(Value::Array(out))
}

/// `via` / `viaOnly`: a pattern or `{ path, pathNot }`.
fn via(v: &Value, what: &str) -> Result<Value, Skip> {
    match v {
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, val) in o {
                match k.as_str() {
                    "path" | "pathNot" => {
                        out.insert(camel_to_snake(k), pattern(val, what)?);
                    }
                    "dependencyTypes" | "dependencyTypesNot" => {
                        out.insert(camel_to_snake(k), dep_types(val, &format!("{what}.{k}"))?);
                    }
                    other => return Err(format!("{what}.{other} isn't supported")),
                }
            }
            Ok(Value::Object(out))
        }
        _ => Ok(json!({ "path": pattern(v, what)? })),
    }
}

fn convert_from(v: &Value) -> Result<Value, Skip> {
    let mut out = Map::new();
    for (k, val) in v.as_object().ok_or("`from` must be an object")? {
        match k.as_str() {
            "path" | "pathNot" => {
                out.insert(camel_to_snake(k), pattern(val, &format!("from.{k}"))?);
            }
            "orphan" => {
                out.insert(k.clone(), val.clone());
            }
            other => return Err(format!("from.{other} isn't supported")),
        }
    }
    Ok(Value::Object(out))
}

fn convert_to(v: &Value) -> Result<Value, Skip> {
    let mut out = Map::new();
    for (k, val) in v.as_object().ok_or("`to` must be an object")? {
        let what = format!("to.{k}");
        let (key, value) = match k.as_str() {
            "path" | "pathNot" | "license" | "licenseNot" | "exoticRequire" | "exoticRequireNot" => {
                (camel_to_snake(k), pattern(val, &what)?)
            }
            "circular" | "couldNotResolve" | "dynamic" | "reachable" | "moreUnstable" | "moreThanOneDependencyType"
            | "ancestor" | "exoticallyRequired" => (camel_to_snake(k), val.clone()),
            "preCompilationOnly" => ("type_only".into(), val.clone()),
            "dependencyTypes" | "dependencyTypesNot" => (camel_to_snake(k), dep_types(val, &what)?),
            "via" => ("via".into(), via(val, &what)?),
            "viaOnly" => ("via_only".into(), via(val, &what)?),
            // Deprecated forms with exact equivalents.
            "viaNot" => ("via_only".into(), json!({ "path_not": pattern(val, &what)? })),
            "viaSomeNot" => ("via".into(), json!({ "path_not": pattern(val, &what)? })),
            other => return Err(format!("to.{other} isn't supported")),
        };
        out.insert(key, value);
    }
    Ok(Value::Object(out))
}

fn convert_module(v: &Value) -> Result<Value, Skip> {
    let mut out = Map::new();
    for (k, val) in v.as_object().ok_or("`module` must be an object")? {
        match k.as_str() {
            "path" | "pathNot" => {
                out.insert(camel_to_snake(k), pattern(val, &format!("module.{k}"))?);
            }
            "numberOfDependentsLessThan" | "numberOfDependentsMoreThan" => {
                out.insert(camel_to_snake(k), val.clone());
            }
            other => return Err(format!("module.{other} isn't supported")),
        }
    }
    Ok(Value::Object(out))
}

/// Converts one rule object; `keys` are the top-level keys it may have.
fn convert_rule(v: &Value, keys: &[&str]) -> Result<Value, Skip> {
    let mut out = Map::new();
    for (k, val) in v.as_object().ok_or("rule must be an object")? {
        if !keys.contains(&k.as_str()) {
            return Err(format!("`{k}` isn't supported"));
        }
        let converted = match k.as_str() {
            "from" => convert_from(val)?,
            "to" => convert_to(val)?,
            "module" => convert_module(val)?,
            _ => val.clone(),
        };
        out.insert(k.clone(), converted);
    }
    Ok(Value::Object(out))
}

/// Options that only affect the originating tool's reporting or runtime.
const HARMLESS_OPTIONS: &[&str] = &[
    "reporterOptions", "progress", "cache", "prefix", "outputType", "outputTo", "moduleSystems",
    "combinedDependencies", "preserveSymlinks", "externalModuleResolutionStrategy", "parser",
    "skipAnalysisNotInRules", "metrics", "forceDeriveDependents", "experimentalStats", "baseDir",
    "validate", "ruleSet", "rulesFile", "mainFields",
    "exportsFields", "conditionNames", "extensions",
];

fn regex_option(v: &Value, name: &str, warnings: &mut Vec<String>) -> Option<Pat> {
    let pat = match v {
        Value::Object(o) => {
            for k in o.keys().filter(|k| *k != "path") {
                warnings.push(format!("options.{name}.{k} isn't supported (ignored)"));
            }
            o.get("path")?
        }
        other => other,
    };
    serde_json::from_value(pat.clone()).ok()
}

pub fn convert(v: &Value) -> Imported {
    let mut warnings = vec![];
    let mut known_violations = None;
    let mut config = Config::empty();
    let mut opts = Options { ignore_type_only: true, ..Options::default() };

    if let Some(o) = v.get("options").and_then(Value::as_object) {
        for (k, val) in o {
            match k.as_str() {
                "exclude" => opts.exclude_path = regex_option(val, k, &mut warnings),
                "includeOnly" => opts.include_only = regex_option(val, k, &mut warnings),
                "doNotFollow" => {
                    // tangle never descends into node_modules; anything else can't be honoured.
                    if let Some(p) = regex_option(val, k, &mut warnings)
                        && !p.0.split('|').all(|alt| alt.contains("node_modules"))
                    {
                        warnings.push(format!("options.doNotFollow {:?} isn't supported (ignored)", p.0));
                    }
                }
                "tsConfig" => opts.tsconfig = val.get("fileName").and_then(Value::as_str).map(String::from),
                "webpackConfig" => {
                    opts.webpack_config = val.get("fileName").and_then(Value::as_str).map(String::from);
                    // `env` may be an object or a single flag name (`--env production`).
                    match val.get("env") {
                        Some(Value::Object(o)) => opts.config_env.webpack_env = o.clone(),
                        Some(Value::String(flag)) => {
                            opts.config_env.webpack_env.insert(flag.clone(), json!(true));
                        }
                        _ => {}
                    }
                    if let Some(mode) = val.pointer("/arguments/mode").and_then(Value::as_str) {
                        opts.config_env.mode = Some(mode.to_string());
                    }
                }
                "babelConfig" => opts.babel_config = val.get("fileName").and_then(Value::as_str).map(String::from),
                "tsPreCompilationDeps" => {
                    // true / "specify": type-only imports are dependencies (and
                    // count toward cycles).
                    let on = val != &json!(false);
                    opts.ignore_type_only = !on;
                    opts.cycles_ignore_type_only = !on;
                }
                "detectJSDocImports" => opts.jsdoc_imports = val == &json!(true),
                "detectProcessBuiltinModuleCalls" => opts.builtin_module_calls = val == &json!(true),
                "exoticRequireStrings" => {
                    opts.exotic_require = val.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect()
                }
                "moduleSystems" => {
                    // tangle always reads ES modules, CommonJS, AMD and TypeScript directives.
                    let listed: Vec<&str> = val.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                    let off: Vec<&str> = ["amd", "tsd"].into_iter().filter(|m| !listed.contains(m)).collect();
                    if !off.is_empty() {
                        warnings.push(format!("options.moduleSystems leaves out {off:?}, but tangle always reads those imports"));
                    }
                }
                "builtInModules" => warnings.push("options.builtInModules isn't supported (Node's own list of builtins is used)".into()),
                // Converted to a tangle baseline by `tangle migrate`.
                "knownViolations" => known_violations = val.as_str().map(String::from),
                k if HARMLESS_OPTIONS.contains(&k) => {}
                other => warnings.push(format!("options.{other} isn't supported (ignored)")),
            }
        }
    }
    config.options = opts;

    let rules = |key: &str| v.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let label = |r: &Value, i: usize| r.get("name").and_then(Value::as_str).map_or(format!("#{}", i + 1), String::from);

    for (i, r) in rules("forbidden").iter().enumerate() {
        match convert_rule(r, &["name", "severity", "comment", "scope", "from", "to", "module"])
            .and_then(|j| serde_json::from_value(j).map_err(|e| e.to_string()))
        {
            Ok(rule) => config.forbidden.push(rule),
            Err(why) => warnings.push(format!("forbidden rule '{}' skipped: {why}", label(r, i))),
        }
    }
    for (i, r) in rules("allowed").iter().enumerate() {
        match convert_rule(r, &["name", "comment", "from", "to"]).and_then(|mut j| {
            j.as_object_mut().map(|o| o.remove("name"));
            serde_json::from_value(j).map_err(|e| e.to_string())
        }) {
            Ok(rule) => config.allowed.push(rule),
            Err(why) => {
                // Dropping an allow rule would *add* violations, so say so loudly.
                warnings.push(format!("allowed rule '{}' skipped: {why} — expect extra not-in-allowed reports", label(r, i)))
            }
        }
    }
    if let Some(s) = v.get("allowedSeverity").and_then(|s| serde_json::from_value(s.clone()).ok()) {
        config.allowed_severity = s;
    }
    for (i, r) in rules("required").iter().enumerate() {
        match convert_rule(r, &["name", "severity", "comment", "module", "to"])
            .and_then(|j| serde_json::from_value(j).map_err(|e| e.to_string()))
        {
            Ok(rule) => config.required.push(rule),
            Err(why) => warnings.push(format!("required rule '{}' skipped: {why}", label(r, i))),
        }
    }
    let summary = format!(
        "{} forbidden, {} allowed, {} required rule(s)",
        config.forbidden.len(),
        config.allowed.len(),
        config.required.len()
    );
    Imported { config, warnings, summary, known_violations }
}

/// Renders an imported config as a commented `tangle.toml`.
pub fn to_toml(imported: &Imported, source: &Path) -> Result<String> {
    let mut out = format!(
        "# tangle.toml — converted from {} by `tangle init --from`.\n",
        source.file_name().unwrap_or_default().to_string_lossy()
    );
    if !imported.warnings.is_empty() {
        out.push_str("#\n# Not converted:\n");
        for w in &imported.warnings {
            out.push_str(&format!("#   - {w}\n"));
        }
    }
    out.push('\n');
    out.push_str(&toml::to_string_pretty(&imported.config)?);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_rules_and_reports_what_it_cannot() {
        let v = json!({
            "forbidden": [
                { "name": "no-circular", "severity": "warn", "from": {}, "to": { "circular": true, "viaNot": "^src/types" } },
                { "name": "not-to-dev-dep", "severity": "error",
                  "from": { "path": ["^src", "^lib"] },
                  "to": { "dependencyTypes": ["npm-dev"], "dependencyTypesNot": ["type-only"], "pathNot": ["node_modules/@types/"] } },
                { "name": "no-deprecated", "severity": "ignore", "to": { "dependencyTypes": ["deprecated"] } },
                { "name": "fancy", "severity": "error", "from": {}, "to": { "path": "x", "exoticallyRequired": true, "exoticRequireNot": "^want$", "ancestor": false } },
                { "name": "typed-cycles", "to": { "circular": true, "viaOnly": { "dependencyTypesNot": ["type-only"] } } },
                { "name": "odd", "to": { "dependencyTypes": ["npm-no-such"] } },
                { "name": "folders", "severity": "warn", "scope": "folder", "from": {}, "to": { "circular": true } },
                { "name": "bundled", "to": { "dependencyTypes": ["npm-bundled", "localmodule"] } },
                { "name": "utils-shared", "module": { "path": "^src/utils", "numberOfDependentsLessThan": 2 } },
            ],
            "allowed": [{ "from": { "path": "^src" }, "to": { "path": "^src" } }],
            "allowedSeverity": "error",
            "required": [{ "name": "controllers-use-base", "module": { "path": "controller\\.ts$" }, "to": { "path": "base-controller" } }],
            "options": {
                "doNotFollow": { "path": "node_modules" },
                "exclude": { "path": "^dist" },
                "includeOnly": ["^src", "^lib"],
                "tsConfig": { "fileName": "tsconfig.json" },
                "tsPreCompilationDeps": true,
                "reporterOptions": { "dot": {} },
                "webpackConfig": { "fileName": "webpack.config.js", "env": { "production": true }, "arguments": { "mode": "production" } },
                "babelConfig": { "fileName": ".babelrc" },
                "exoticRequireStrings": ["want"],
                "knownViolations": "known.json"
            }
        });
        let imp = convert(&v);
        let names: Vec<_> = imp.config.forbidden.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["no-circular", "not-to-dev-dep", "no-deprecated", "fancy", "typed-cycles", "folders", "bundled", "utils-shared"]);
        assert_eq!(imp.config.forbidden[5].scope, crate::config::Scope::Folder);
        assert_eq!(imp.config.forbidden[3].to.exotic_require_not.as_ref().unwrap().0, "^want$");
        assert_eq!(imp.config.forbidden[4].to.via_only.as_ref().unwrap().dependency_types_not.as_deref(), Some(&["type-only".to_string()][..]));
        assert_eq!(imp.config.forbidden[6].to.dependency_types.as_deref(), Some(&["npm-bundled".to_string(), "aliased".to_string()][..]));
        assert_eq!(imp.config.forbidden[0].to.via_only.as_ref().unwrap().path_not.as_ref().unwrap().0, "^src/types");
        assert_eq!(imp.config.forbidden[1].from.path.as_ref().unwrap().0, "(?:^src)|(?:^lib)");
        assert_eq!(imp.config.forbidden[2].severity, crate::config::Severity::Off);
        assert_eq!(imp.config.allowed.len(), 1);
        assert_eq!(imp.config.allowed_severity, crate::config::Severity::Error);
        assert_eq!(imp.config.required.len(), 1);
        let o = &imp.config.options;
        assert_eq!(o.exclude_path.as_ref().unwrap().0, "^dist");
        assert_eq!(o.tsconfig.as_deref(), Some("tsconfig.json"));
        assert!(!o.ignore_type_only && !o.cycles_ignore_type_only);
        let w = imp.warnings.join("\n");
        assert!(w.contains("'odd' skipped: dependency type \"npm-no-such\""), "{w}");
        assert_eq!(o.webpack_config.as_deref(), Some("webpack.config.js"));
        assert_eq!(o.config_env.webpack_env.get("production"), Some(&json!(true)));
        assert_eq!(o.config_env.mode.as_deref(), Some("production"));
        assert_eq!(o.babel_config.as_deref(), Some(".babelrc"));
        assert!(!w.contains("webpackConfig") && !w.contains("babelConfig"), "{w}");
        assert_eq!(o.exotic_require, ["want"]);
        assert_eq!(imp.known_violations.as_deref(), Some("known.json"));
        assert!(!w.contains("reporterOptions") && !w.contains("doNotFollow"), "{w}");
        // Round-trips through TOML.
        let text = to_toml(&imp, Path::new("rules.config.js")).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.forbidden.len(), 8);
        crate::rules::validate(&back).unwrap();
    }
}
