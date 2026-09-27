//! Converts eslint-plugin-import / eslint-plugin-import-x dependency rules:
//! `no-cycle`, `no-restricted-paths` and `no-extraneous-dependencies`, from
//! flat (`eslint.config.*`) or legacy (`.eslintrc*` in JSON, YAML or JS,
//! package.json `eslintConfig`) configs, honouring `files` / `ignores` /
//! `overrides` scoping, legacy `extends` chains and the `import/resolver`
//! settings.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use super::glob::{Plain, has_magic, to_regex};
use super::{Imported, run_node};
use crate::config::{Config, FromSpec, GroupDef, Options, Pat, Rule, Scope, Severity, ToSpec};

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
    ".eslintrc.yaml",
    ".eslintrc.yml",
    ".eslintrc",
];

const LOADER: &str = r#"
const fs = require('fs'), path = require('path'), { pathToFileURL } = require('url'), { createRequire } = require('module');
const strip = (t) => t.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '').replace(/,(\s*[}\]])/g, '$1');
const strs = (x) => [].concat(x || []).flat(Infinity).filter((s) => typeof s === 'string');
const pick = (rules) => Object.fromEntries(Object.entries(rules || {}).filter(([k]) =>
  /^(import|import-x|i)\/(no-cycle|no-restricted-paths|no-extraneous-dependencies)$/.test(k) ||
  /^@(nx|nrwl\/nx)\/enforce-module-boundaries$/.test(k) || /^boundaries\//.test(k)));
const boundaries = (s) => (s && (s['boundaries/elements'] || s['boundaries/ignore'])) ? { elements: s['boundaries/elements'] || null, ignore: strs(s['boundaries/ignore']) } : null;
const resolver = (s) => (s || {})['import/resolver'] || (s || {})['import-x/resolver'] || null;
const warnings = [];

// js-yaml comes with ESLint 8 and @eslint/eslintrc; look for it from the config.
const yaml = (text, from) => {
  const req = createRequire(from);
  for (const get of [() => req('js-yaml'), () => createRequire(req.resolve('@eslint/eslintrc'))('js-yaml'), () => createRequire(req.resolve('eslint'))('js-yaml')]) {
    let y;
    try { y = get(); } catch { continue; }
    return y.load(text);
  }
  throw new Error(`${path.basename(from)} is YAML, which needs js-yaml — install the project's ESLint dependencies first`);
};

// A legacy config file, read as ESLint reads it.
const loadLegacy = (file) => {
  const base = path.basename(file), text = fs.readFileSync(file, 'utf8');
  if (base === 'package.json') return JSON.parse(text).eslintConfig || {};
  if (/\.c?js$/.test(base)) return require(file);
  if (/\.ya?ml$/.test(base)) return yaml(text, file);
  try { return JSON.parse(text); } catch {}
  try { return JSON.parse(strip(text)); } catch {}
  return yaml(text, file);
};

// eslint-config-* / eslint-plugin-* package naming, as ESLint does it.
const pkgName = (name, kind) => {
  const pre = `eslint-${kind}`;
  if (name.startsWith('@')) {
    const [scope, rest] = name.split('/');
    return !rest ? `${scope}/${pre}` : rest.startsWith(pre) ? name : `${scope}/${pre}-${rest}`;
  }
  return name.startsWith(`${pre}-`) ? name : `${pre}-${name}`;
};

const resolveExtends = (name, from) => {
  if (name.startsWith('eslint:')) return null; // core rules only
  const req = createRequire(from);
  if (name.startsWith('plugin:')) {
    const rest = name.slice(7), i = rest.lastIndexOf('/');
    const file = req.resolve(pkgName(rest.slice(0, i), 'plugin'));
    const config = (require(file).configs || {})[rest.slice(i + 1)];
    if (!config) throw new Error('the plugin has no such config');
    return { config, file };
  }
  const file = name.startsWith('.') || path.isAbsolute(name) ? path.resolve(path.dirname(from), name) : req.resolve(pkgName(name, 'config'));
  return { config: loadLegacy(file), file };
};

// A legacy config as cascade items: its extended configs first, then its own
// settings, then its overrides. Patterns stay relative to the root config.
const expand = (c, file, crit, seen) => {
  const items = [];
  for (const e of strs(c.extends)) {
    let r;
    try { r = resolveExtends(e, file); } catch (err) {
      warnings.push(`extends "${e}" couldn't be loaded (${String(err.message).split('\n')[0]}), so rules it enables aren't converted`);
      continue;
    }
    if (r && !seen.has(r.file)) items.push(...expand(r.config || {}, r.file, crit, new Set([...seen, r.file])));
  }
  if (c.ignorePatterns && !crit) items.push({ files: null, ignores: strs(c.ignorePatterns), globalIgnores: true, rules: {}, resolver: null });
  items.push({ files: crit ? crit.files : null, ignores: crit ? crit.ignores : [], globalIgnores: false, rules: pick(c.rules), resolver: resolver(c.settings), boundaries: boundaries(c.settings) });
  for (const o of c.overrides || []) items.push(...expand(o, file, { files: strs(o.files), ignores: strs(o.excludedFiles) }, seen));
  return items;
};

(async () => {
  const file = process.env.TANGLE_CONFIG_FILE, base = path.basename(file);
  const flat = base.startsWith('eslint.config.');
  let c;
  if (/\.(c|m)?[jt]s$/.test(base)) { const m = await import(pathToFileURL(file).href); c = await (m.default ?? m); }
  else c = loadLegacy(file);
  if (typeof c === 'function') c = await c();
  let items;
  if (flat) {
    items = [].concat(c).flat(Infinity).filter(Boolean).map((it) => ({
      files: it.files ? strs(it.files) : null,
      ignores: strs(it.ignores),
      globalIgnores: !!it.ignores && Object.keys(it).every((k) => k === 'ignores' || k === 'name'),
      rules: pick(it.rules), resolver: resolver(it.settings), boundaries: boundaries(it.settings),
    }));
  } else {
    items = expand(c, file, null, new Set([file]));
  }
  process.stdout.write(JSON.stringify({ flat, items, warnings }));
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

    // boundaries/elements (the last definition wins, as settings merge).
    let items = v["items"].as_array().cloned().unwrap_or_default();
    let elements: Vec<Value> = items
        .iter()
        .filter_map(|i| i["boundaries"]["elements"].as_array().cloned())
        .next_back()
        .unwrap_or_default();
    let ctx = Ctx { prefix: prefix.clone(), elements: elements.clone() };
    let mut groups = vec![];
    let mut nx = false;

    let mut active: Vec<(String, Active)> = vec![];
    let mut last_opts: std::collections::HashMap<String, Vec<Value>> = Default::default();
    for item in items {
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
            let (sev, mut opts) = severity(&value);
            // A severity on its own keeps the options set earlier (as ESLint merges).
            if opts.is_empty() {
                opts = last_opts.get(&name).cloned().unwrap_or_default();
            } else {
                last_opts.insert(name.clone(), opts.clone());
            }
            let short = name.rsplit('/').next().unwrap_or(&name).to_string();
            let boundary_rule = name == "boundaries/element-types" || name == "boundaries/dependencies";
            if name.starts_with("boundaries/") && !boundary_rule {
                if sev != Severity::Off {
                    warnings.push(format!("{name} isn't converted (only boundaries/dependencies and element-types are)"));
                }
                continue;
            }
            if boundary_rule && sev != Severity::Off && groups.is_empty() {
                groups = boundary_groups(&ctx, &mut warnings);
            }
            if short == "enforce-module-boundaries" && sev != Severity::Off {
                nx = true;
            }
            match &files {
                None => {
                    // Applies to every file: replaces whatever came before.
                    active.retain(|(n, _)| *n != short);
                    if sev != Severity::Off {
                        for rule in convert(&name, &short, sev, &opts, &ctx, &mut warnings) {
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
                        for mut rule in convert(&name, &short, sev, &opts, &ctx, &mut warnings) {
                            if rule.scope == Scope::Group {
                                // Group rules apply per project/element, not per file.
                                // `**/*.ts`-style globs cover everything anyway.
                                let catch_all = globs.iter().all(|g| g.trim_start_matches("./").starts_with("**/*"));
                                let w = format!("{name}: ESLint `files` scoping doesn't apply to group rules; converted for all files");
                                if !catch_all && !warnings.contains(&w) {
                                    warnings.push(w);
                                }
                                active.push((short.clone(), Active { rule, scoped: false }));
                                continue;
                            }
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
    warnings.extend(strs(&v["warnings"]));
    let scoped = active.iter().filter(|(_, a)| a.scoped).count();
    let mut config = Config::empty();
    config.options = options;
    config.options.nx_projects = nx;
    config.groups = groups;
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

fn group_rule(name: &str, sev: Severity, comment: &str, from: FromSpec, to: ToSpec) -> Rule {
    Rule { scope: Scope::Group, ..rule(name, sev, comment, from, to) }
}

struct Ctx {
    prefix: String,
    /// `boundaries/elements` settings.
    elements: Vec<Value>,
}

const NPM_TYPES: &[&str] = &["npm", "npm-dev", "npm-peer", "npm-optional", "npm-undeclared"];

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|s| s.as_str().map(String::from)).collect(),
        _ => vec![],
    }
}

/// `@nx/enforce-module-boundaries`: projects come from Nx discovery
/// (`options.nx_projects`); each dependency constraint becomes group rules.
fn convert_nx(full: &str, sev: Severity, o: &Value, warnings: &mut Vec<String>) -> Vec<Rule> {
    let mut out = vec![
        group_rule(full, sev, "Circular dependency between projects.", FromSpec::default(), ToSpec { circular: Some(true), ..Default::default() }),
        group_rule(
            full,
            sev,
            "Imports of apps are forbidden.",
            FromSpec::default(),
            ToSpec { tags: Some(vec!["projectType:application".into()]), ..Default::default() },
        ),
        rule(
            full,
            sev,
            "Projects cannot be imported by a relative or absolute path, and must begin with an npm scope.",
            FromSpec::default(),
            ToSpec { dependency_types: Some(vec!["relative".into()]), cross_group: Some(true), ..Default::default() },
        ),
    ];
    for (key, default) in [
        ("enforceBuildableLibDependency", json!(false)),
        ("allowCircularSelfDependency", json!(false)),
        ("banTransitiveDependencies", json!(false)),
        ("checkNestedExternalImports", json!(false)),
        ("allow", json!([])),
        ("ignoredCircularDependencies", json!([])),
    ] {
        if let Some(v) = o.get(key).filter(|v| **v != default) {
            warnings.push(format!("{full}: {key} = {v} isn't converted"));
        }
    }
    let constraints = o.get("depConstraints").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut source_tags: Vec<String> = vec![];
    let mut any_all_source = false;
    for c in &constraints {
        let (from, label) = if let Some(all) = c.get("allSourceTags") {
            any_all_source = true;
            let all = strings(all);
            (FromSpec { tags_all: Some(all.clone()), ..Default::default() }, format!("tagged {}", all.join(" + ")))
        } else {
            let s = c.get("sourceTag").and_then(Value::as_str).unwrap_or("*").to_string();
            source_tags.push(s.clone());
            let from = if s == "*" { FromSpec::default() } else { FromSpec { tags: Some(vec![s.clone()]), ..Default::default() } };
            (from, if s == "*" { "any project".to_string() } else { format!("tagged \"{s}\"") })
        };
        let only = strings(c.get("onlyDependOnLibsWithTags").unwrap_or(&Value::Null));
        if !only.is_empty() && !only.iter().any(|t| t == "*") {
            out.push(group_rule(
                full,
                sev,
                &format!("A project {label} can only depend on libs tagged {}.", only.join(", ")),
                from.clone(),
                ToSpec { tags_not: Some(only), ..Default::default() },
            ));
        }
        let not = strings(c.get("notDependOnLibsWithTags").unwrap_or(&Value::Null));
        if !not.is_empty() {
            out.push(group_rule(
                full,
                sev,
                &format!("A project {label} cannot depend on libs tagged {}.", not.join(", ")),
                from.clone(),
                ToSpec { tags: Some(not), ..Default::default() },
            ));
        }
        // External imports: module rules on the project's files (they carry its tags).
        let pkg_re = |globs: Vec<String>| Pat::any(&globs.iter().map(|g| to_regex(g, Plain::Exact, false, "")).collect::<Vec<_>>());
        let banned = strings(c.get("bannedExternalImports").unwrap_or(&Value::Null));
        if !banned.is_empty() {
            out.push(rule(
                full,
                sev,
                &format!("A project {label} cannot import {}.", banned.join(", ")),
                from.clone(),
                ToSpec { dependency_types: Some(NPM_TYPES.iter().map(|s| s.to_string()).collect()), path: Some(pkg_re(banned)), ..Default::default() },
            ));
        }
        let allowed = strings(c.get("allowedExternalImports").unwrap_or(&Value::Null));
        if c.get("allowedExternalImports").is_some() {
            out.push(rule(
                full,
                sev,
                &format!("A project {label} can only import external packages {}.", allowed.join(", ")),
                from.clone(),
                ToSpec {
                    dependency_types: Some(NPM_TYPES.iter().map(|s| s.to_string()).collect()),
                    path_not: (!allowed.is_empty()).then(|| pkg_re(allowed)),
                    ..Default::default()
                },
            ));
        }
        if c.get("onlyTagsDependOnTags").is_some() {
            warnings.push(format!("{full}: onlyTagsDependOnTags isn't converted"));
        }
    }
    // Nx: a project whose tags match no constraint can't depend on other projects.
    if !constraints.is_empty() && !source_tags.iter().any(|t| t == "*") {
        if any_all_source {
            warnings.push(format!(
                "{full}: projects matching only an allSourceTags constraint are exempt from the \"no matching constraint\" check only if they have one of its tags"
            ));
            for c in &constraints {
                source_tags.extend(strings(c.get("allSourceTags").unwrap_or(&Value::Null)));
            }
        }
        out.push(group_rule(
            full,
            sev,
            "A project without tags matching at least one constraint cannot depend on any libraries.",
            FromSpec { tags_not: Some(source_tags), ..Default::default() },
            ToSpec::default(),
        ));
    }
    out
}

/// `boundaries/elements` → `[[groups]]` (one group type per element type).
fn boundary_groups(ctx: &Ctx, warnings: &mut Vec<String>) -> Vec<GroupDef> {
    let mut out = vec![];
    for e in &ctx.elements {
        let Some(ty) = e.get("type").and_then(Value::as_str) else { continue };
        let patterns = strings(e.get("pattern").unwrap_or(&Value::Null));
        if patterns.is_empty() {
            continue;
        }
        let mode = e.get("mode").and_then(Value::as_str).unwrap_or("folder");
        let base = e.get("basePattern").and_then(Value::as_str);
        let alts: Vec<String> = patterns
            .iter()
            .map(|p| {
                let body = glob_body(p);
                // Matched from the right (as if prefixed with `**/`) unless basePattern is set.
                let left = match base {
                    Some(b) => format!("{}/", glob_body(b.trim_end_matches('/'))),
                    None if mode == "full" => String::new(),
                    None => "(?:.*/)?".into(),
                };
                match mode {
                    "file" | "full" => format!("{left}{body}$"),
                    _ => format!("{left}{body}(?:/|$)"),
                }
            })
            .collect();
        let path = format!("^(?:{})", alts.join("|"));
        let path = if ctx.prefix.is_empty() { path } else { format!("^{}/(?:{})", fancy_regex::escape(&ctx.prefix), &path[1..]) };
        if e.get("capture").is_some() || e.get("baseCapture").is_some() {
            warnings.push(format!("boundaries element \"{ty}\": captures aren't converted (rules using them are skipped)"));
        }
        out.push(GroupDef { name: ty.into(), path: Pat(path), tags: vec![] });
    }
    out
}

/// A glob as an unanchored regex fragment.
fn glob_body(g: &str) -> String {
    let r = to_regex(g, Plain::Exact, false, "");
    r.trim_start_matches('^').trim_end_matches('$').to_string()
}

/// Which element types a boundaries selector matches.
#[derive(Clone)]
struct TypeSel {
    patterns: Vec<String>,
    negate: bool,
}

impl TypeSel {
    fn any() -> Self {
        TypeSel { patterns: vec!["*".into()], negate: false }
    }

    fn matches(&self, ty: &str) -> bool {
        self.patterns.iter().any(|p| crate::groups::tag_matches(p, ty)) != self.negate
    }
}

/// Parses a boundaries selector into type matchers. Supports the legacy form
/// (`"type"`, `["type", {…}]`, lists) and v6+ objects (`{ element: { type } }`,
/// `{ to: { element: { types: { anyOf } } } }`). `Err` = can't convert exactly.
fn type_selectors(v: &Value) -> Result<Vec<TypeSel>, String> {
    let one = |s: &str| {
        let (neg, pat) = s.strip_prefix('!').map_or((false, s), |p| (true, p));
        TypeSel { patterns: vec![pat.to_string()], negate: neg }
    };
    match v {
        Value::Null => Ok(vec![]),
        Value::String(s) => Ok(vec![one(s)]),
        Value::Array(a) if a.len() == 2 && a[0].is_string() && a[1].is_object() => {
            Err(format!("selector {v} uses captures"))
        }
        Value::Array(a) => a.iter().map(type_selectors).collect::<Result<Vec<_>, _>>().map(|v| v.concat()),
        Value::Object(o) => {
            if let Some(inner) = o.get("to").or_else(|| o.get("from")) {
                return type_selectors(inner);
            }
            let Some(el) = o.get("element") else {
                return Err(format!("selector {v} isn't about element types (files/modules aren't converted)"));
            };
            let extra: Vec<&String> = el.as_object().map(|e| e.keys().filter(|k| *k != "type" && *k != "types").collect()).unwrap_or_default();
            if !extra.is_empty() {
                return Err(format!("selector {v} uses {extra:?}"));
            }
            if let Some(t) = el.get("type").and_then(Value::as_str) {
                return Ok(vec![one(t)]);
            }
            match el.get("types") {
                Some(Value::Object(t)) if t.contains_key("anyOf") => Ok(vec![TypeSel { patterns: strings(&t["anyOf"]), negate: false }]),
                Some(Value::Object(t)) if t.contains_key("noneOf") => Ok(vec![TypeSel { patterns: strings(&t["noneOf"]), negate: true }]),
                Some(other) => Ok(vec![TypeSel { patterns: strings(other), negate: false }]),
                None => Ok(vec![TypeSel::any()]),
            }
        }
        _ => Err(format!("selector {v} isn't supported")),
    }
}

/// `boundaries/dependencies` (v6+ `policies`) and `boundaries/element-types`
/// (legacy `rules`): replays the policies in order — the last matching one
/// wins, starting from `default` — to get each from→to type's verdict.
fn convert_boundaries(full: &str, sev: Severity, o: &Value, ctx: &Ctx, warnings: &mut Vec<String>) -> Vec<Rule> {
    let types: Vec<String> = ctx.elements.iter().filter_map(|e| e.get("type").and_then(Value::as_str).map(String::from)).collect();
    if types.is_empty() {
        warnings.push(format!("{full}: no boundaries/elements settings found"));
        return vec![];
    }
    let default_allow = o.get("default").and_then(Value::as_str) != Some("disallow");
    let policies = o.get("policies").or_else(|| o.get("rules")).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut compiled = vec![];
    for (i, p) in policies.iter().enumerate() {
        if p.get("importKind").is_some() {
            warnings.push(format!("{full}: importKind isn't converted (the policy applies to all imports)"));
        }
        let parsed = (|| -> Result<_, String> {
            let from = match p.get("from") {
                None => vec![TypeSel::any()],
                Some(f) => type_selectors(f)?,
            };
            Ok((from, type_selectors(p.get("allow").unwrap_or(&Value::Null))?, type_selectors(p.get("disallow").unwrap_or(&Value::Null))?))
        })();
        match parsed {
            Ok((from, allow, disallow)) => compiled.push((from, allow, disallow, p.get("message").and_then(Value::as_str).map(String::from))),
            Err(why) => warnings.push(format!("{full}: policy #{} skipped — {why}", i + 1)),
        }
    }
    let hits = |sel: &[TypeSel], ty: &str| sel.iter().any(|s| s.matches(ty));
    let mut out = vec![];
    for from_ty in &types {
        let mut denied: Vec<String> = vec![];
        let mut message = None;
        for to_ty in &types {
            let mut allowed = default_allow;
            let mut msg = None;
            for (from, allow, disallow, m) in &compiled {
                if !hits(from, from_ty) {
                    continue;
                }
                if hits(allow, to_ty) {
                    allowed = true;
                }
                if hits(disallow, to_ty) {
                    allowed = false;
                    msg = m.clone();
                }
            }
            if !allowed {
                denied.push(to_ty.clone());
                message = message.or(msg);
            }
        }
        if !denied.is_empty() {
            let comment = message.unwrap_or_else(|| format!("Elements of type \"{from_ty}\" can't import {}.", denied.join(", ")));
            out.push(group_rule(
                full,
                sev,
                &comment,
                FromSpec { tags: Some(vec![from_ty.clone()]), ..Default::default() },
                ToSpec { tags: Some(denied), ..Default::default() },
            ));
        }
    }
    out
}

fn convert(full: &str, short: &str, sev: Severity, opts: &[Value], ctx: &Ctx, warnings: &mut Vec<String>) -> Vec<Rule> {
    let prefix = ctx.prefix.as_str();
    let o = opts.first().cloned().unwrap_or(json!({}));
    match short {
        "enforce-module-boundaries" => convert_nx(full, sev, &o, warnings),
        "element-types" | "dependencies" if full.starts_with("boundaries/") => convert_boundaries(full, sev, &o, ctx, warnings),
        "no-cycle" => {
            // The plugin searches breadth-first up to maxDepth imports beyond
            // the imported module, so it finds cycles of up to maxDepth + 1.
            let max_cycle_length = o.get("maxDepth").and_then(Value::as_u64).map(|d| d as usize + 1);
            if o.get("allowUnsafeDynamicCyclicDependency") == Some(&json!(true)) {
                warnings.push(format!("{full}: allowUnsafeDynamicCyclicDependency isn't supported — cycles through dynamic imports are reported"));
            }
            // Like the plugin, `import type` doesn't count (tangle's default).
            vec![rule(
                full,
                sev,
                "Circular dependency (converted from ESLint).",
                FromSpec::default(),
                ToSpec { circular: Some(true), max_cycle_length, ..Default::default() },
            )]
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
